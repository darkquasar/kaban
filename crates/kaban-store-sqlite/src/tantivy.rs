//! The store's text adapter and its most recent sealed reader. Each store has one SQLite
//! connection and one reader slot. A fresh catalog pin observes that slot while the
//! connection is locked. Reader construction and searching run after the connection unlocks.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

use kaban_core::Result;
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::text_index::{ExtentSpan, Searched, TextIndex, TextQuery, TextSnapshot};
use kaban_index_tantivy::{SealedReader, TantivyIndex};
use rusqlite::Connection;
use rustix::fs::{Mode, OFlags};

/// Every open composes the production adapter. Tests may replace it with an override.
pub(crate) enum Attached {
    Tantivy(Arc<TantivyIndex>, Recent),
    #[cfg(feature = "text-index-override")]
    Override(Box<dyn TextIndex + Send + Sync>),
}

impl std::fmt::Debug for Attached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tantivy(index, _) => index.fmt(f),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => f.write_str("TextIndexOverride"),
        }
    }
}

impl Attached {
    #[cfg_attr(not(feature = "text-index-override"), allow(clippy::unnecessary_wraps))]
    pub(crate) fn tantivy(&self) -> Option<&Arc<TantivyIndex>> {
        match self {
            Self::Tantivy(index, _) => Some(index),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => None,
        }
    }

    pub(crate) fn observe(
        &self,
        generation: u64,
        path: &Path,
        tier: Option<(u64, u64, ExtentSpan)>,
    ) -> Selection {
        match self {
            Self::Tantivy(_, recent) => recent.observe(generation, path, tier),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => Selection::Ordinary,
        }
    }

    pub(crate) fn with_reader<T>(
        &self,
        selected: Selection,
        budget: &QueryBudget,
        use_index: impl FnOnce(&dyn TextIndex) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Tantivy(index, recent) => {
                let reader = match selected {
                    Selection::Ordinary => return use_index(index.as_ref()),
                    Selection::Hit(reader) => Some(reader),
                    Selection::Load { key, file, charge } => {
                        let reader =
                            SealedReader::open(file, key.span, charge, budget)?.map(Arc::new);
                        if let Some(reader) = &reader {
                            recent.keep(&key, reader);
                        }
                        reader
                    }
                };
                use_index(&PinnedReader { index, reader })
            }
            #[cfg(feature = "text-index-override")]
            Self::Override(index) => use_index(index.as_ref()),
        }
    }
}

struct PinnedReader<'a> {
    index: &'a TantivyIndex,
    reader: Option<Arc<SealedReader>>,
}

impl TextIndex for PinnedReader<'_> {
    fn search(
        &self,
        _: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> Result<Searched> {
        if let Some(reader) = &self.reader {
            reader.search(self.index, query, budget)
        } else {
            budget.check()?;
            Ok(Searched::Unavailable)
        }
    }
}

/// The key belongs to one store handle and its SQLite connection. The descriptor provides
/// both the incarnation and the bytes, including after an atomic container replacement.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    generation: u64,
    extent: u64,
    published: u64,
    span: ExtentSpan,
    dev: u64,
    ino: u64,
    born: Option<SystemTime>,
    changed: (i64, i64),
}

pub(crate) enum Selection {
    Ordinary,
    Hit(Arc<SealedReader>),
    Load {
        key: Arc<Key>,
        file: File,
        charge: BudgetLease,
    },
}

// One maximum-size owned reader needs 16.75 MiB. The timer reservation covers its
// requested 128 KiB stack, guard, native thread metadata and handles.
const STORE_BYTES: u64 = 17 << 20;
pub(crate) const TIMER_BYTES: u64 = 256 << 10;

#[derive(Debug)]
pub(crate) struct Recent {
    slot: Arc<Mutex<Slot>>,
    pub(crate) idle: Duration,
}

impl Default for Recent {
    fn default() -> Self {
        Self {
            slot: Arc::new(Mutex::new(Slot {
                loading: Weak::new(),
                value: None,
                timer: None,
                retained: QueryBudget::new(STORE_BYTES, None),
            })),
            idle: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
struct Slot {
    retained: QueryBudget,
    // The selection owns this key until it publishes, fails or is abandoned.
    loading: Weak<Key>,
    value: Option<Entry>,
    timer: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct Entry {
    key: Key,
    reader: Arc<SealedReader>,
    expires: Instant,
}

impl Slot {
    // Reservations serialize under the slot lock. Concurrent owners can only release bytes.
    // Insufficient room uses the ordinary path without consuming any query allowance.
    fn reserve(&self, bytes: u64) -> Option<BudgetLease> {
        if bytes > STORE_BYTES - self.retained.live_bytes() {
            return None;
        }
        self.retained.reserve(bytes).ok()
    }

    fn evict(&mut self) {
        self.value = None;
        self.loading = Weak::new();
        if let Some(timer) = &self.timer {
            timer.thread().unpark();
        }
    }
}

impl Drop for Recent {
    fn drop(&mut self) {
        let timer = {
            let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            slot.evict();
            slot.timer.take()
        };
        if let Some(timer) = timer {
            let _ = timer.join();
        }
    }
}

fn expire(slot: &Weak<Mutex<Slot>>, charge: BudgetLease) {
    loop {
        let wait = {
            let Some(slot) = slot.upgrade() else { return };
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let remaining = slot
                .value
                .as_ref()
                .and_then(|e| e.expires.checked_duration_since(Instant::now()));
            let Some(wait) = remaining else {
                drop(charge);
                slot.timer = None;
                if slot.value.is_some() {
                    slot.evict();
                }
                return;
            };
            wait
        };
        // Only a weak reference survives the wait. Eviction and store close wake us.
        std::thread::park_timeout(wait);
    }
}

impl Recent {
    #[cfg(test)]
    pub(crate) fn budget(&self) -> QueryBudget {
        self.slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retained
            .clone()
    }

    /// Called under the same connection lock that took the pin. Even an uncacheable
    /// observation fences an earlier, slower load from publishing into this store.
    fn observe(
        &self,
        generation: u64,
        path: &Path,
        tier: Option<(u64, u64, ExtentSpan)>,
    ) -> Selection {
        let source = tier.and_then(|(extent, published, span)| {
            let bytes = SealedReader::reservation(span.len)?;
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            let file = File::from(rustix::fs::open(path, flags, Mode::empty()).ok()?);
            let meta = file.metadata().ok()?;
            if !meta.is_file() || span.offset.checked_add(span.len)? > meta.len() {
                return None;
            }
            let key = Key {
                generation,
                extent,
                published,
                span,
                dev: meta.dev(),
                ino: meta.ino(),
                born: meta.created().ok(),
                changed: (meta.ctime(), meta.ctime_nsec()),
            };
            Some((key, file, bytes))
        });
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((key, file, bytes)) = source {
            let now = Instant::now();
            if let Some(entry) = &mut slot.value
                && key == entry.key
                && now < entry.expires
            {
                tracing::trace!(generation, extent = key.extent, "reuse sealed text reader");
                entry.expires = now + self.idle;
                return Selection::Hit(Arc::clone(&entry.reader));
            }
            if slot.loading.upgrade().as_deref() == Some(&key) {
                return Selection::Ordinary;
            }
            slot.evict();
            if let Some(charge) = slot.reserve(bytes) {
                let key = Arc::new(key);
                slot.loading = Arc::downgrade(&key);
                return Selection::Load { key, file, charge };
            }
            return Selection::Ordinary;
        }
        slot.evict();
        Selection::Ordinary
    }

    fn keep(&self, key: &Arc<Key>, reader: &Arc<SealedReader>) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if !slot.loading.ptr_eq(&Arc::downgrade(key)) {
            return;
        }
        slot.loading = Weak::new();
        if slot.timer.is_none() {
            let Some(charge) = slot.reserve(TIMER_BYTES) else {
                return;
            };
            let weak = Arc::downgrade(&self.slot);
            let Ok(timer) = std::thread::Builder::new()
                .name("text-reader-expiry".into())
                .stack_size(128 << 10)
                .spawn(move || expire(&weak, charge))
            else {
                // The query can still use its reader. Only an expirable slot may retain it.
                return;
            };
            slot.timer = Some(timer);
        }
        slot.value = Some(Entry {
            key: **key,
            reader: Arc::clone(reader),
            expires: Instant::now() + self.idle,
        });
    }
}

/// The store's connection as the store shares it.
pub(crate) type SharedConnection = Arc<Mutex<Connection>>;
