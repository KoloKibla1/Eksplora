use crate::scanner::{ScanOptions, scan, stat_one};
use crate::types::FileEntry;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Max previous searches kept for prefix-narrowing ("pho" reuses "ph"'s
/// candidates). Ring buffer: typing builds a chain, backspace hits the exact
/// entry. Small enough to stay in cache.
pub(crate) const QUERY_CACHE_MAX_ENTRIES: usize = 8;
/// Only candidate sets at most this large are cached. One-char queries can
/// match hundreds of thousands of entries — caching those would churn
/// megabytes per keystroke for little narrowing benefit.
pub(crate) const QUERY_CACHE_MAX_CANDIDATES: usize = 20_000;
/// Queries shorter than this are never cached (see above).
pub(crate) const QUERY_CACHE_MIN_LEN: usize = 2;

/// A previous search's full untruncated candidate set. Untruncated on
/// purpose: narrowing must see every candidate, not just the top-`limit`
/// that was displayed.
#[derive(Debug, Clone)]
pub(crate) struct CachedQuery {
    pub query: String,
    pub fuzzy: bool,
    pub candidates: Vec<(u32, PathBuf)>,
}

/// In-memory index. v0: flat Vec + precomputed lowercase names.
/// Fast enough for ~1M entries for substring search with rayon (<30ms).
/// SQLite is only for persistence across restarts.
/// `pos` maps path -> vec index for O(1) incremental upsert/remove.
///
/// Live caches (`counts`, `queries`) are interior-mutable so searches stay
/// `&Index` (no caller churn, no extra locking around rayon scans). They are
/// always consistent: every mutation path funnels through `invalidate_caches`
/// (`upsert` / `remove` / `remove_prefix` / `clear` / `merge_entries`), so a
/// present cache entry is always current. Future file ops (rename, delete,
/// move) must route through those same methods — then they invalidate
/// automatically.
#[derive(Debug, Default)]
pub struct Index {
    pub entries: Vec<FileEntry>,
    pos: HashMap<PathBuf, usize>,
    /// Global parent -> direct-children count, built once per mutation.
    /// Valid for any `dir`: children of a dir under `dir` are all under
    /// `dir` themselves, so scoped lookups read identical numbers.
    counts: RwLock<Option<Arc<HashMap<PathBuf, usize>>>>,
    queries: RwLock<VecDeque<CachedQuery>>,
}

impl Index {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pos: HashMap::new(),
            counts: RwLock::new(None),
            queries: RwLock::new(VecDeque::new()),
        }
    }

    pub fn from_entries(entries: Vec<FileEntry>) -> Self {
        let mut idx = Self {
            entries,
            pos: HashMap::new(),
            counts: RwLock::new(None),
            queries: RwLock::new(VecDeque::new()),
        };
        idx.rebuild_pos();
        idx
    }

    fn rebuild_pos(&mut self) {
        self.pos.clear();
        self.pos.reserve(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            self.pos.insert(e.path.clone(), i);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, e: FileEntry) {
        self.upsert(e);
    }

    /// Pre-allocate for `additional` entries (both the vec and the path map).
    /// Live-preview merges should reserve in bulk to avoid repeated HashMap
    /// rehashing while a deep scan streams in.
    pub fn reserve(&mut self, additional: usize) {
        self.entries.reserve(additional);
        self.pos.reserve(additional);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pos.clear();
        self.invalidate_caches();
    }

    /// Insert or update. O(1) via path map. Swap-remove keeps it O(1).
    /// Invalidates the live caches (see struct docs).
    pub fn upsert(&mut self, e: FileEntry) {
        self.insert_entry(e);
        self.invalidate_caches();
    }

    /// Raw insert without cache invalidation — for bulk loads that
    /// invalidate once at the end (see `merge_entries`).
    fn insert_entry(&mut self, e: FileEntry) {
        if let Some(&i) = self.pos.get(&e.path) {
            self.entries[i] = e;
        } else {
            let i = self.entries.len();
            self.pos.insert(e.path.clone(), i);
            self.entries.push(e);
        }
    }

    /// Remove single path. Returns true if present. Swap-remove is O(1).
    /// Invalidates the live caches.
    pub fn remove(&mut self, path: &Path) -> bool {
        if let Some(&i) = self.pos.get(path) {
            let last = self.entries.len() - 1;
            self.entries.swap_remove(i);
            self.pos.remove(path);
            if i != last {
                let moved_path = self.entries[i].path.clone();
                self.pos.insert(moved_path, i);
            }
            self.invalidate_caches();
            true
        } else {
            false
        }
    }

    /// Remove everything under `dir` (prefix). Returns count removed.
    /// Used when a directory is deleted/renamed. Invalidates the caches.
    pub fn remove_prefix(&mut self, dir: &Path) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| !e.path.starts_with(dir));
        let removed = before - self.entries.len();
        if removed > 0 {
            self.rebuild_pos();
            self.invalidate_caches();
        }
        removed
    }

    pub fn merge_entries(&mut self, entries: Vec<FileEntry>) {
        self.reserve(entries.len());
        self.invalidate_caches();
        for e in entries {
            self.insert_entry(e);
        }
    }

    /// Drop all cached derived data. Called by every mutation; searches
    /// rebuild lazily on next use (once per mutation, not once per query).
    fn invalidate_caches(&self) {
        if let Ok(mut c) = self.counts.write() {
            c.take();
        }
        if let Ok(mut q) = self.queries.write() {
            q.clear();
        }
    }

    /// Direct-children count for every parent in the index, cached across
    /// searches until the next mutation. Previously rebuilt from scratch on
    /// every keystroke; now a cheap `Arc` clone on cache hit.
    pub fn child_counts(&self) -> Arc<HashMap<PathBuf, usize>> {
        // Fast path: already built for the current content.
        if let Ok(guard) = self.counts.read() {
            if let Some(map) = guard.as_ref() {
                return Arc::clone(map);
            }
        }
        // Miss: rebuild under the write lock (double-checked).
        let mut guard = self.counts.write().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let mut counts: HashMap<PathBuf, usize> = HashMap::new();
            for e in &self.entries {
                if let Some(parent) = e.path.parent() {
                    *counts.entry(parent.to_path_buf()).or_default() += 1;
                }
            }
            *guard = Some(Arc::new(counts));
        }
        Arc::clone(guard.as_ref().expect("just populated"))
    }

    /// Resolve a cached path back to its entry. `None` if the index changed
    /// underneath (defensive — caches are invalidated on mutation, so this
    /// only fires on races outside the outer lock).
    pub(crate) fn resolve(&self, path: &Path) -> Option<&FileEntry> {
        self.pos.get(path).and_then(|&i| self.entries.get(i))
    }

    /// Longest previous query that is a prefix of `q` (exact matches
    /// included) with the same fuzzy flag. Cloned out so scoring can run
    /// without holding the cache lock.
    pub(crate) fn lookup_query_cache(&self, q: &str, fuzzy: bool) -> Option<CachedQuery> {
        self.queries
            .read()
            .ok()?
            .iter()
            .filter(|c| c.fuzzy == fuzzy && q.starts_with(&c.query))
            .max_by_key(|c| c.query.len())
            .cloned()
    }

    /// Remember a search's candidate set for later narrowing. Enforces the
    /// size/count policy; callers pass the untruncated set.
    pub(crate) fn store_query_cache(
        &self,
        query: String,
        fuzzy: bool,
        candidates: Vec<(u32, PathBuf)>,
    ) {
        if query.chars().count() < QUERY_CACHE_MIN_LEN
            || candidates.len() > QUERY_CACHE_MAX_CANDIDATES
        {
            return;
        }
        if let Ok(mut q) = self.queries.write() {
            // Refresh position if this exact query is already cached.
            if let Some(i) = q.iter().position(|c| c.query == query && c.fuzzy == fuzzy) {
                q.remove(i);
            }
            q.push_front(CachedQuery { query, fuzzy, candidates });
            while q.len() > QUERY_CACHE_MAX_ENTRIES {
                q.pop_back();
            }
        }
    }

    /// Stat one path and upsert, or remove if deleted. Returns:
    /// Ok(true) = upserted, Ok(false) = removed, Err = io error counted by caller.
    pub fn stat_and_upsert(&mut self, path: &Path) -> bool {
        match stat_one(path) {
            Some(e) => {
                let is_dir = e.is_dir;
                self.upsert(e);
                // If a dir was (re)created, merge its current children so
                // new files inside are indexed without a full rescan.
                if is_dir {
                    let (children, _) = scan(path, &ScanOptions::default());
                    // Drop stale children first (handles deletes inside dir).
                    let live: HashSet<PathBuf> =
                        children.iter().map(|c| c.path.clone()).collect();
                    let stale: Vec<PathBuf> = self
                        .entries
                        .iter()
                        .filter(|e| {
                            e.path != path
                                && e.path.starts_with(path)
                                && !live.contains(&e.path)
                        })
                        .map(|e| e.path.clone())
                        .collect();
                    for s in stale {
                        self.remove(&s);
                    }
                    self.merge_entries(children);
                }
                true
            }
            None => {
                // Deleted: remove self + any children if it was a dir.
                if self.remove(path) {
                    self.remove_prefix(path);
                }
                false
            }
        }
    }

    /// Apply a batch of watcher/USN paths. Returns (upserted, removed).
    pub fn apply_event_paths(&mut self, paths: &[PathBuf]) -> (usize, usize) {
        let mut up = 0;
        let mut del = 0;
        for p in paths {
            let before = self.pos.contains_key(p);
            if self.stat_and_upsert(p) {
                if !before {
                    up += 1;
                } else {
                    up += 1;
                }
            } else {
                if before {
                    del += 1;
                }
            }
        }
        (up, del)
    }

    /// Apply a batch of file-watcher paths (external changes by other
    /// processes): existing paths are re-statted (new files upserted,
    /// modified ones refreshed), vanished ones removed with their subtrees.
    /// Paths outside `root` or inside scan-pruned subtrees (`node_modules`,
    /// `$Recycle.Bin`, …) are ignored so the watcher can never index what
    /// the scan deliberately excludes. Returns (upserted, removed).
    pub fn apply_watch_batch(&mut self, root: &Path, paths: &[PathBuf]) -> (usize, usize) {
        let mut up = 0;
        let mut del = 0;
        for p in paths {
            if crate::scanner::is_pruned_path(root, p) {
                continue;
            }
            if p.exists() {
                if self.stat_and_upsert(p) {
                    up += 1;
                }
            } else if self.remove(p) {
                self.remove_prefix(p);
                del += 1;
            }
        }
        (up, del)
    }

    /// Bulk save with single transaction + WAL. ~100k rows/sec on NVMe.
    pub fn save_to_sqlite(&self, db_path: &Path) -> Result<()> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files(
               path TEXT PRIMARY KEY, name TEXT, name_lower TEXT,
               ext TEXT, size INTEGER, mtime_ms INTEGER,
               is_dir INTEGER, attributes INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_name_lower ON files(name_lower);
             DELETE FROM files;",
        )?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files
                 (path,name,name_lower,ext,size,mtime_ms,is_dir,attributes)
                 VALUES (?,?,?,?,?,?,?,?)",
            )?;
            for e in &self.entries {
                stmt.execute(rusqlite::params![
                    e.path.to_string_lossy().as_ref(),
                    e.name,
                    e.name_lower,
                    e.ext,
                    e.size as i64,
                    e.mtime_ms,
                    e.is_dir as i32,
                    e.attributes as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_from_sqlite(db_path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(db_path)
            .with_context(|| format!("open db {}", db_path.display()))?;
        let mut stmt = conn.prepare(
            "SELECT path,name,name_lower,ext,size,mtime_ms,is_dir,attributes FROM files",
        )?;
        let rows = stmt.query_map([], |row| {
            let path_str: String = row.get(0)?;
            let name: String = row.get(1)?;
            let name_lower: String = row.get(2)?;
            let ext: String = row.get(3)?;
            let size: i64 = row.get(4)?;
            let mtime_ms: i64 = row.get(5)?;
            let is_dir: i32 = row.get(6)?;
            let attributes: i64 = row.get(7)?;
            Ok(FileEntry {
                path: std::path::PathBuf::from(path_str),
                name,
                name_lower,
                ext,
                size: size as u64,
                mtime_ms,
                is_dir: is_dir != 0,
                attributes: attributes as u32,
            })
        })?;
        let mut entries = Vec::new();
        for r in rows {
            entries.push(r?);
        }
        Ok(Self::from_entries(entries))
    }
}
