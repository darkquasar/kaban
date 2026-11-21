//! Access policy for the private query connection.
use kaban_core::query_budget::QueryBudget;
use kaban_core::{NodeId, Result, StoreError};
use kaban_vtab::QueryConnection;
use rusqlite::hooks::{AuthAction as A, AuthContext, Authorization};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

const CATALOG: u8 = 0;
const MODEL_SETUP: u8 = 3;
const USER: u8 = 1;
const ID_LOOKUP: u8 = 2;

pub(super) struct Guard {
    phase: AtomicU8,
    private: String,
    model: OnceLock<super::model::Model>,
    denied: Mutex<Option<&'static str>>,
}

struct Restore<'a>(&'a AtomicU8, u8);
impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.0.store(self.1, Ordering::Relaxed);
    }
}

impl Guard {
    pub(super) fn install(
        conn: &mut QueryConnection<'_>,
        sql: &str,
        budget: &QueryBudget,
    ) -> Result<Arc<Self>> {
        // Each name consists solely of ASCII identifier characters. SQLite identifier quoting
        // cannot synthesize those bytes. Choose a fresh name absent from all submitted text,
        // including CTE names, so only our stored projection can supply this accessor.
        let private = loop {
            budget.check()?;
            let candidate = format!(
                "kaban_projection_{}",
                NodeId::new().as_str().replace('-', "")
            );
            if !contains_identifier(sql.as_bytes(), candidate.as_bytes(), budget)? {
                break candidate;
            }
        };
        // Connect the eponymous modules using fixed SQL before SQLite authorizes public SQL.
        for name in super::functions::FUNCTIONS
            .iter()
            .map(|(name, _)| *name)
            .chain(["json_each", "pragma_table_info"])
        {
            conn.query_row(&format!("PRAGMA main.table_xinfo({name})"), [], |_| Ok(()))
                .map_err(|_| super::refused("model", "cannot connect module"))?;
        }
        let this = Arc::new(Self {
            phase: AtomicU8::new(CATALOG),
            private,
            model: OnceLock::new(),
            denied: Mutex::new(None),
        });
        let hook = Arc::clone(&this);
        conn.set_authorizer(move |ctx| hook.authorize(ctx))
            .map_err(|_| super::refused("authorizer", "cannot install guard"))?;
        Ok(this)
    }

    pub(super) fn install_model(
        &self,
        tx: &rusqlite::Transaction<'_>,
        budget: &QueryBudget,
    ) -> Result<&super::model::Model> {
        let model = self.during(MODEL_SETUP, || {
            super::model::Model::install(tx, &self.private, budget)
        })?;
        self.model
            .set(model)
            .map_err(|_| super::refused("model", "already installed"))?;
        self.model
            .get()
            .ok_or_else(|| super::refused("model", "missing model"))
    }

    pub(super) fn user<T>(&self, f: impl FnOnce() -> T) -> T {
        self.during(USER, f)
    }
    pub(super) fn catalog<T>(&self, f: impl FnOnce() -> T) -> T {
        self.during(CATALOG, f)
    }
    pub(super) fn id_lookup<T>(&self, f: impl FnOnce() -> T) -> T {
        self.during(ID_LOOKUP, f)
    }
    fn during<T>(&self, phase: u8, f: impl FnOnce() -> T) -> T {
        let _restore = Restore(&self.phase, self.phase.swap(phase, Ordering::Relaxed));
        f()
    }
    pub(super) fn failure(&self) -> StoreError {
        let name = self
            .denied
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or("statement");
        super::refused(name, "outside the declared read model or invalid SQL")
    }
    fn authorize(&self, ctx: AuthContext<'_>) -> Authorization {
        let phase = self.phase.load(Ordering::Relaxed);
        // Catalog runs only fixed store-issued SQL with the public VM suspended or unstarted.
        // Only the fixed store templates run in this scope, before the user statement is prepared.
        if phase == MODEL_SETUP {
            return match (ctx.database_name, ctx.action) {
                (
                    _,
                    A::Select
                    | A::Function {
                        function_name: "max" | "json_extract" | "json_type",
                    },
                )
                | (
                    Some("main"),
                    A::Pragma {
                        pragma_name: "table_info",
                        pragma_value: Some("schema_type"),
                    }
                    | A::Read {
                        table_name: "schema_type" | "pragma_table_info" | "json_each",
                        ..
                    },
                )
                | (
                    Some("temp"),
                    A::CreateTempView { .. }
                    | A::Insert {
                        table_name: "sqlite_temp_master",
                    }
                    | A::Update {
                        table_name: "sqlite_temp_master",
                        ..
                    }
                    | A::Read {
                        table_name: "sqlite_temp_master",
                        ..
                    },
                ) => Authorization::Allow,
                _ => Authorization::Deny,
            };
        }
        if phase == CATALOG {
            return match ctx.action {
                A::Read { .. } if ctx.database_name == Some("temp") => Authorization::Deny,
                A::Select | A::Read { .. } | A::Function { .. } | A::Transaction { .. } => {
                    Authorization::Allow
                }
                _ => Authorization::Deny,
            };
        }
        let deny = match ctx.action {
            A::Select => return Authorization::Allow,
            A::Read {
                table_name,
                column_name,
            } => {
                let ok = match (phase, ctx.database_name, table_name, ctx.accessor) {
                    (ID_LOOKUP, Some("main"), "node", None) => {
                        matches!(column_name, "id" | "rid" | "lifecycle")
                    }
                    (USER, db, table, source) => self
                        .model
                        .get()
                        .is_some_and(|m| m.allows_read(db, table, column_name, source)),
                    _ => false,
                };
                if ok {
                    return Authorization::Allow;
                }
                match table_name {
                    "node" => "node",
                    "cat_generation" => "cat_generation",
                    "sqlite_master" | "sqlite_schema" => "sqlite_schema",
                    _ => "table or column",
                }
            }
            A::Function {
                function_name: "json_extract",
            } if phase == USER
                && self
                    .model
                    .get()
                    .is_some_and(|m| m.allows_json(ctx.accessor)) =>
            {
                return Authorization::Allow;
            }
            A::Function { function_name }
                if phase == USER
                    && matches!(
                        function_name,
                        "count"
                            | "min"
                            | "max"
                            | "sum"
                            | "avg"
                            | "coalesce"
                            | "length"
                            | "lower"
                            | "upper"
                            | "abs"
                            | "like"
                    ) =>
            {
                return Authorization::Allow;
            }
            A::Pragma { .. } => "PRAGMA",
            A::Attach { .. } => "ATTACH",
            A::Detach { .. } => "DETACH",
            A::Function {
                function_name: "load_extension",
            } => "load_extension",
            A::Function { .. } => "function",
            A::Update { .. } => "UPDATE",
            A::Delete { .. } => "DELETE",
            A::Insert { .. } => "INSERT",
            A::Transaction { .. } => "transaction",
            _ => "statement",
        };
        *self.denied.lock().unwrap_or_else(PoisonError::into_inner) = Some(deny);
        Authorization::Deny
    }
}

fn contains_identifier(mut sql: &[u8], name: &[u8], budget: &QueryBudget) -> Result<bool> {
    const STRIDE: usize = 8192;
    while !sql.is_empty() {
        budget.check()?;
        let end = sql.len().min(STRIDE + name.len() - 1);
        if sql[..end]
            .windows(name.len())
            .any(|part| part.eq_ignore_ascii_case(name))
        {
            return Ok(true);
        }
        sql = &sql[sql.len().min(STRIDE)..];
    }
    Ok(false)
}
