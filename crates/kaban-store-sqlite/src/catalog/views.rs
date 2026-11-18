//! The view rows of `cat_view` (amendment 3.2, 3.3): when a pin last used a view, its label,
//! and the retention step that marks the views past `[catalog] view_retention`
//! `needs_reimport` and retires the extents only they held.

use kaban_core::{Durability, Result, ViewName};
use rusqlite::Connection;

use super::Catalog;
use super::tables::sql_int;
use crate::sql_error::SqlResultExt;

impl Catalog {
    /// Set the label of `view` (T16 names the views it creates; `None` clears it).
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn set_view_label(
        &self,
        conn: &mut Connection,
        view: &ViewName,
        label: Option<&str>,
    ) -> Result<()> {
        self.commit(conn, Durability::Normal, |txn| {
            txn.conn()
                .execute(
                    "INSERT INTO cat_view(name, label) VALUES (?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET label = excluded.label",
                    rusqlite::params![view.to_string(), label],
                )
                .sql()
                .map(|_| ())
        })
    }

    /// Record now (unix seconds) as the last use of each of `views` (amendment 3.3): one
    /// statement per view on `conn`, outside any transaction. A connection that cannot write
    /// (a reader's) leaves the rows as they were: the pin itself does not depend on it.
    pub(super) fn touch_views(&self, conn: &Connection, views: &[ViewName]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let touched = self.outside_commit(|| {
            views.iter().try_for_each(|view| {
                conn.execute(
                    "INSERT INTO cat_view(name, last_used) VALUES (?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET last_used = excluded.last_used",
                    rusqlite::params![view.to_string(), now],
                )
                .map(|_| ())
            })
        });
        if let Err(e) = touched {
            tracing::debug!(error = %e, "a pin did not record the last use of its views");
        }
    }

    /// The retention step (amendment 3.2), in one commit: among the live views besides `main`
    /// and `protected`, keep the newest `keep` by last use (then by name) and every parent of a
    /// kept or protected view; mark the rest `needs_reimport`, drop their chains and retire
    /// the extents no other chain holds (they reclaim like any retired extent). Returns the
    /// retired views, in name order. T16 passes the git-live views as `protected`.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn retain_views(
        &self,
        conn: &mut Connection,
        keep: u32,
        protected: &[ViewName],
    ) -> Result<Vec<String>> {
        let protected: Vec<String> = protected.iter().map(ToString::to_string).collect();
        let keep = usize::try_from(keep).unwrap_or(usize::MAX);
        self.commit(conn, Durability::Normal, |txn| {
            let views: Vec<(String, Option<String>)> = {
                let mut stmt = txn
                    .conn()
                    .prepare(
                        "SELECT name, parent FROM cat_view WHERE state = 'live' AND name != 'main'
                         ORDER BY last_used IS NULL, last_used DESC, name",
                    )
                    .sql()?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .sql()?;
                rows.collect::<rusqlite::Result<_>>().sql()?
            };
            let candidates: Vec<&str> = views
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| !protected.iter().any(|p| p == name))
                .collect();
            let mut kept: Vec<&str> = candidates.iter().take(keep).copied().collect();
            kept.extend(protected.iter().map(String::as_str));
            // a kept view's parent is kept, and so on up its ancestry
            let mut i = 0;
            while let Some(name) = kept.get(i).copied() {
                let parent = views
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, p)| p.as_deref());
                if let Some(parent) = parent.filter(|p| !kept.contains(p)) {
                    kept.push(parent);
                }
                i += 1;
            }
            let mut retired: Vec<String> = candidates
                .into_iter()
                .filter(|name| !kept.contains(name))
                .map(str::to_owned)
                .collect();
            if retired.is_empty() {
                return Ok(retired);
            }
            retired.sort();
            let generation = sql_int(txn.generation()?.0).sql()?;
            for view in &retired {
                let conn = txn.conn();
                conn.execute(
                    "UPDATE cat_view SET state = 'needs_reimport' WHERE name = ?1",
                    [view],
                )
                .sql()?;
                conn.execute(
                    "UPDATE cat_extent SET dead_since = ?1
                     WHERE dead_since IS NULL
                       AND id IN (SELECT extent_id FROM cat_view_extent WHERE view = ?2)
                       AND NOT EXISTS (SELECT 1 FROM cat_view_extent v
                                       WHERE v.extent_id = cat_extent.id AND v.view != ?2)",
                    rusqlite::params![generation, view],
                )
                .sql()?;
                conn.execute("DELETE FROM cat_view_extent WHERE view = ?1", [view])
                    .sql()?;
            }
            Ok(retired)
        })
    }
}
