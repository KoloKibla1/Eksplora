use crate::scanner::{ScanOptions, scan, stat_one};
use crate::types::FileEntry;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// In-memory index. v0: flat Vec + precomputed lowercase names.
/// Fast enough for ~1M entries for substring search with rayon (<30ms).
/// SQLite is only for persistence across restarts.
/// `pos` maps path -> vec index for O(1) incremental upsert/remove.
#[derive(Debug, Default)]
pub struct Index {
    pub entries: Vec<FileEntry>,
    pos: HashMap<PathBuf, usize>,
}

impl Index {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pos: HashMap::new(),
        }
    }

    pub fn from_entries(entries: Vec<FileEntry>) -> Self {
        let mut idx = Self {
            entries,
            pos: HashMap::new(),
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
    }

    /// Insert or update. O(1) via path map. Swap-remove keeps it O(1).
    pub fn upsert(&mut self, e: FileEntry) {
        if let Some(&i) = self.pos.get(&e.path) {
            self.entries[i] = e;
        } else {
            let i = self.entries.len();
            self.pos.insert(e.path.clone(), i);
            self.entries.push(e);
        }
    }

    /// Remove single path. Returns true if present. Swap-remove is O(1).
    pub fn remove(&mut self, path: &Path) -> bool {
        if let Some(&i) = self.pos.get(path) {
            let last = self.entries.len() - 1;
            self.entries.swap_remove(i);
            self.pos.remove(path);
            if i != last {
                let moved_path = self.entries[i].path.clone();
                self.pos.insert(moved_path, i);
            }
            true
        } else {
            false
        }
    }

    /// Remove everything under `dir` (prefix). Returns count removed.
    /// Used when a directory is deleted/renamed.
    pub fn remove_prefix(&mut self, dir: &Path) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| !e.path.starts_with(dir));
        let removed = before - self.entries.len();
        if removed > 0 {
            self.rebuild_pos();
        }
        removed
    }

    pub fn merge_entries(&mut self, entries: Vec<FileEntry>) {
        for e in entries {
            self.upsert(e);
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
