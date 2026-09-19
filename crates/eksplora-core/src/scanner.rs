use crate::types::FileEntry;
use jwalk::{Parallelism, WalkDir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Max depth. None = unlimited.
    pub max_depth: Option<usize>,
    /// Scan worker threads. None = auto (all cores minus one, min 1) so the
    /// UI thread always has room to breathe. Set explicitly to benchmark.
    pub num_threads: Option<usize>,
    /// Skip well-known noisy system dirs ($Recycle.Bin, System Volume Information, ...).
    pub skip_system_dirs: bool,
    /// Skip dependency/cache dirs (node_modules, .git, browser caches, ...).
    /// Pruned at traversal time — their contents are never even enumerated.
    /// Does not apply to the scanned root itself.
    pub skip_caches: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            num_threads: None,
            skip_system_dirs: true,
            skip_caches: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub skipped: u64,
    pub duration_ms: u128,
    pub files_per_sec: u64,
    /// True when a cancellation flag stopped the walk early.
    pub cancelled: bool,
}

fn is_system_dir_name(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    matches!(
        s.as_ref(),
        "$Recycle.Bin"
            | "System Volume Information"
            | "$WinREAgent"
            | "Config.Msi"
            | "Recovery"
            | "Documents and Settings"
    )
}

/// Lowercase dir names pruned when `skip_caches` is on. Matched against the
/// plain directory name (case-insensitive), anywhere in the tree.
const CACHE_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "__pycache__",
    ".cache",
    "cache",
    "code cache",
    "gpucache",
    "dawncache",
    "crashpad",
    "service worker",
    ".npm",
    "_cacache",
];

fn is_cache_dir_name(name: &std::ffi::OsStr) -> bool {
    let lower = name.to_string_lossy().to_lowercase();
    CACHE_DIR_NAMES.iter().any(|c| *c == lower)
}

fn scan_threads(explicit: Option<usize>) -> usize {
    if let Some(n) = explicit {
        return n.max(1);
    }
    let n = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);
    // Reserve one core for the UI/renderer so a big scan can't freeze the app.
    n.saturating_sub(1).max(1)
}

/// Fast parallel scan. Never follows symlinks/reparse points (avoids loops).
/// Never panics on Access Denied — counts as error and continues.
/// Runs directory reads on a dedicated pool (all cores minus one) so the
/// global rayon pool used by search stays untouched.
pub fn scan(root: &Path, opts: &ScanOptions) -> (Vec<FileEntry>, ScanStats) {
    scan_with(root, opts, None)
}

/// Same as `scan` but aborts early when `cancel` is set (checked every 512
/// entries). Returns partial results with `stats.cancelled = true`.
pub fn scan_with(
    root: &Path,
    opts: &ScanOptions,
    cancel: Option<&AtomicBool>,
) -> (Vec<FileEntry>, ScanStats) {
    scan_with_full(root, opts, cancel, None)
}

/// Full variant with a progress callback invoked with (files, dirs) counts:
/// once at start (0, 0) and then every 512 walked entries. Runs on the
/// caller's thread — cheap enough to forward to UI progress events
/// (callers should throttle before crossing IPC).
pub fn scan_with_full(
    root: &Path,
    opts: &ScanOptions,
    cancel: Option<&AtomicBool>,
    progress: Option<&dyn Fn(u64, u64)>,
) -> (Vec<FileEntry>, ScanStats) {
    scan_with_full_streaming(root, opts, cancel, progress, None)
}

/// Fast depth-1 scan for instant first paint: the root itself plus its
/// direct children (no recursion). Uses plain `read_dir` — no thread pool,
/// typically <100ms even on huge trees — so the UI can show the first
/// layer immediately while the deep scan runs. Applies the same
/// `skip_system_dirs` / `skip_caches` filters as the full scan so the
/// partial list never shows entries the final index will prune.
pub fn scan_shallow(root: &Path, opts: &ScanOptions) -> (Vec<FileEntry>, ScanStats) {
    let start = Instant::now();
    let mut stats = ScanStats::default();
    let mut out = Vec::new();

    // Root entry itself, for consistency with the full jwalk scan.
    if let Some(e) = stat_one(root) {
        if e.is_dir {
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        out.push(e);
    }

    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => {
            stats.errors += 1;
            finish_stats(&mut stats, start);
            return (out, stats);
        }
    };
    for child in rd {
        let child = match child {
            Ok(c) => c,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let name = child.file_name();
        let ft = match child.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if ft.is_dir() {
            if opts.skip_system_dirs && is_system_dir_name(&name) {
                stats.skipped += 1;
                continue;
            }
            if opts.skip_caches && is_cache_dir_name(&name) {
                stats.skipped += 1;
                continue;
            }
        }
        // Never follow symlinks/reparse points (matches full scan).
        if ft.is_symlink() {
            match std::fs::symlink_metadata(child.path()) {
                Ok(md) if md.file_type().is_dir() => {
                    // Treat symlinked dirs as skipped, not traversed.
                    stats.skipped += 1;
                    continue;
                }
                _ => {}
            }
        }
        match stat_one(&child.path()) {
            Some(e) => {
                if e.is_dir {
                    stats.dirs += 1;
                } else {
                    stats.files += 1;
                }
                out.push(e);
            }
            None => stats.errors += 1,
        }
    }
    finish_stats(&mut stats, start);
    (out, stats)
}

/// Entries per streaming batch delivered to `on_batch`. Large enough that
/// the callback (and any index-merge + IPC behind it) fires infrequently —
/// a 500k-entry tree produces ~60 callbacks, not hundreds. Callers that
/// merge into a live index should additionally throttle by time (see the
/// Tauri `scan_dir` command) so UI refreshes stay at ~0.5Hz.
pub const STREAM_BATCH: usize = 8192;

/// Streaming variant of [`scan_with_full`]: `on_batch` receives slices of
/// newly walked entries (drained every [`STREAM_BATCH`] entries and once at
/// the end) so callers can merge them into a live index and emit partial UI
/// updates while the walk continues. `progress` semantics are unchanged.
/// When `on_batch` is None this behaves exactly like `scan_with_full`.
///
/// Performance note: the callback borrows the accumulator tail and must
/// copy what it needs synchronously. Prefer buffering + infrequent merges
/// over locking a shared index on every batch.
pub fn scan_with_full_streaming(
    root: &Path,
    opts: &ScanOptions,
    cancel: Option<&AtomicBool>,
    progress: Option<&dyn Fn(u64, u64)>,
    on_batch: Option<&dyn Fn(&[FileEntry])>,
) -> (Vec<FileEntry>, ScanStats) {
    scan_streaming_inner(root, opts, cancel, progress, on_batch, STREAM_BATCH)
}

fn scan_streaming_inner(
    root: &Path,
    opts: &ScanOptions,
    cancel: Option<&AtomicBool>,
    progress: Option<&dyn Fn(u64, u64)>,
    on_batch: Option<&dyn Fn(&[FileEntry])>,
    batch: usize,
) -> (Vec<FileEntry>, ScanStats) {
    let start = Instant::now();
    let is_cancelled = || cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false);
    let mut stats = ScanStats::default();

    if is_cancelled() {
        stats.cancelled = true;
        return (Vec::new(), stats);
    }

    let mut walk = WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .parallelism(Parallelism::RayonNewPool(scan_threads(opts.num_threads)));

    if let Some(d) = opts.max_depth {
        walk = walk.max_depth(d);
    }

    let skipped = Arc::new(AtomicU64::new(0));
    if opts.skip_system_dirs || opts.skip_caches {
        let sys = opts.skip_system_dirs;
        let caches = opts.skip_caches;
        let counter = Arc::clone(&skipped);
        // Note: closure must be 'static (jwalk bound), hence the Arc counter.
        walk = walk.process_read_dir(move |_depth, _path, _state, children| {
            let before = children.len();
            children.retain(|entry| match entry {
                Ok(e) => {
                    if !e.file_type().is_dir() {
                        return true;
                    }
                    let name = e.file_name();
                    !(sys && is_system_dir_name(name) || caches && is_cache_dir_name(name))
                }
                Err(_) => true,
            });
            counter.fetch_add((before - children.len()) as u64, Ordering::Relaxed);
        });
    }

    // jwalk parallelizes directory reads internally. Collect into Vec.
    // Pre-allocate roughly to avoid reallocs on big trees.
    let mut out: Vec<FileEntry> = Vec::with_capacity(16_384);

    let mut since_check = 0u32;
    if let Some(p) = progress {
        p(0, 0);
    }
    for entry in walk.into_iter() {
        since_check += 1;
        if since_check >= 512 {
            since_check = 0;
            if is_cancelled() {
                stats.cancelled = true;
                break;
            }
            if let Some(p) = progress {
                p(stats.files, stats.dirs);
            }
        }
        match entry {
            Ok(e) => {
                let path: PathBuf = e.path();
                let ft = e.file_type();
                let is_dir = ft.is_dir();
                // Metadata comes from FindFirstFile data jwalk already fetched.
                // If unavailable (race), fall back to zeros instead of extra syscall.
                // NOTE: no per-file GetFileAttributesW here on purpose — that
                // second syscall per file roughly doubled scan cost. Attributes
                // stay 0 in bulk scans; use `stat_one` for on-demand lookup.
                let (size, mtime_ms) = match e.metadata() {
                    Ok(m) => {
                        let size = if m.is_dir() { 0 } else { m.len() };
                        let mtime_ms = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        (size, mtime_ms)
                    }
                    Err(_) => {
                        stats.errors += 1;
                        continue;
                    }
                };
                if is_dir {
                    stats.dirs += 1;
                } else {
                    stats.files += 1;
                }
                out.push(FileEntry::from_path(path, is_dir, size, mtime_ms, 0));
                // Stream batches so the UI can show early layers live.
                // Drained every STREAM_BATCH entries; the remainder is flushed
                // below.
                if let Some(cb) = on_batch {
                    if out.len() % batch == 0 {
                        let at = out.len() - batch;
                        // Slice off the last batch without reallocating `out`.
                        // Pass the tail slice directly — the caller must copy
                        // what it needs before we push more.
                        cb(&out[at..]);
                    }
                }
            }
            Err(_) => {
                stats.errors += 1;
            }
        }
    }

    // Flush any entries not yet delivered via on_batch. To keep the contract
    // simple (batches are non-overlapping tails), re-derive the undelivered
    // tail: everything after the last batch-aligned boundary.
    if let Some(cb) = on_batch {
        let delivered = (out.len() / batch) * batch;
        if delivered < out.len() {
            cb(&out[delivered..]);
        } else if out.is_empty() {
            cb(&[]);
        }
    }

    stats.skipped = skipped.load(Ordering::Relaxed);
    finish_stats(&mut stats, start);

    (out, stats)
}

fn finish_stats(stats: &mut ScanStats, start: Instant) {
    let elapsed = start.elapsed();
    stats.duration_ms = elapsed.as_millis();
    let secs = elapsed.as_secs_f64().max(0.001);
    stats.files_per_sec = ((stats.files + stats.dirs) as f64 / secs) as u64;
}

#[cfg(windows)]
fn file_attributes_fast(path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetFileAttributesW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = unsafe { GetFileAttributesW(windows::core::PCWSTR(wide.as_ptr())) };
    if attrs == u32::MAX {
        0
    } else {
        attrs
    }
}

#[cfg(not(windows))]
fn file_attributes_fast(_path: &Path) -> u32 {
    0
}

/// Stat a single path without walking. Returns None if deleted/inaccessible.
/// Used for incremental index updates from watcher/USN events.
pub fn stat_one(path: &Path) -> Option<FileEntry> {
    let md = std::fs::symlink_metadata(path).ok()?;
    // Don't follow symlinks/reparse points for dirs to avoid loops.
    let ft = md.file_type();
    let is_dir = ft.is_dir();
    let size = if is_dir { 0 } else { md.len() };
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let attrs = file_attributes_fast(path);
    Some(FileEntry::from_path(
        path.to_path_buf(),
        is_dir,
        size,
        mtime_ms,
        attrs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eksplora-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn skips_cache_dirs_and_counts_them() {
        let root = tmp_root("skip");
        std::fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
        std::fs::write(root.join("node_modules").join("pkg").join("x.js"), "x").unwrap();
        std::fs::write(root.join("keep.txt"), "k").unwrap();
        let (entries, stats) = scan(&root, &ScanOptions::default());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"keep.txt"));
        assert!(!names.iter().any(|n| *n == "x.js"));
        assert!(stats.skipped >= 1, "pruned dirs should be counted");
        assert!(!stats.cancelled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_caches_can_be_disabled() {
        let root = tmp_root("noskip");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules").join("x.js"), "x").unwrap();
        let opts = ScanOptions { skip_caches: false, ..Default::default() };
        let (entries, _) = scan(&root, &opts);
        assert!(entries.iter().any(|e| e.name == "x.js"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn precancelled_scan_aborts_immediately() {
        let root = tmp_root("cancel");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let flag = AtomicBool::new(true);
        let (entries, stats) = scan_with(&root, &ScanOptions::default(), Some(&flag));
        assert!(stats.cancelled);
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn progress_callback_fires() {
        use std::sync::atomic::AtomicU64;
        let root = tmp_root("progress");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_cb = Arc::clone(&calls);
        let (_, stats) = scan_with_full(
            &root,
            &ScanOptions::default(),
            None,
            Some(&move |_files: u64, _dirs: u64| {
                calls_cb.fetch_add(1, Ordering::Relaxed);
            }),
        );
        assert!(!stats.cancelled);
        assert!(calls.load(Ordering::Relaxed) >= 1, "progress must fire at least once");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shallow_returns_first_layer_only() {
        let root = tmp_root("shallow");
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::write(root.join("top.txt"), "t").unwrap();
        std::fs::write(root.join("sub").join("mid.txt"), "m").unwrap();
        std::fs::write(root.join("sub").join("deep").join("leaf.txt"), "l").unwrap();
        let (entries, stats) = scan_shallow(&root, &ScanOptions::default());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"top.txt"));
        assert!(names.contains(&"sub"));
        assert!(!names.iter().any(|n| *n == "mid.txt"), "shallow must not recurse");
        assert!(!names.iter().any(|n| *n == "leaf.txt"), "shallow must not recurse");
        assert!(!stats.cancelled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn streaming_batches_cover_full_scan_without_dupes() {
        use std::collections::HashSet;
        let root = tmp_root("streaming");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        // Small batch to cross several boundaries with few files (fast).
        for i in 0..2500 {
            std::fs::write(root.join(format!("f{:05}.txt", i)), "x").unwrap();
        }
        std::fs::write(root.join("sub").join("deep.txt"), "d").unwrap();
        let seen = Arc::new(parking_lot_like());
        let seen_cb = Arc::clone(&seen);
        let (entries, stats) = scan_streaming_inner(
            &root,
            &ScanOptions::default(),
            None,
            None,
            Some(&move |batch: &[FileEntry]| {
                seen_cb.lock().extend(batch.iter().map(|e| e.path.clone()));
            }),
            512,
        );
        assert!(!stats.cancelled);
        let seen = seen.lock();
        assert_eq!(seen.len(), entries.len(), "batches must cover every entry exactly once");
        let full: HashSet<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(*seen, full);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Measures streaming-callback overhead vs a plain scan on the same tree.
    /// Ignored by default (file creation takes a few seconds); run with
    /// `cargo test -p eksplora-core -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_streaming_overhead() {
        let root = tmp_root("bench-stream");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for i in 0..8000 {
            std::fs::write(root.join(format!("f{:05}.txt", i)), "x").unwrap();
        }
        std::fs::write(root.join("sub").join("deep.txt"), "d").unwrap();

        let t = Instant::now();
        let (plain, _) = scan(&root, &ScanOptions::default());
        let plain_ms = t.elapsed().as_millis();

        let t = Instant::now();
        let staged = std::cell::RefCell::new(Vec::<FileEntry>::new());
        let (streamed, _) = scan_with_full_streaming(
            &root,
            &ScanOptions::default(),
            None,
            None,
            Some(&|batch: &[FileEntry]| {
                staged.borrow_mut().extend(batch.iter().cloned());
            }),
        );
        let stream_ms = t.elapsed().as_millis();
        assert_eq!(plain.len(), streamed.len());
        assert_eq!(staged.borrow().len(), streamed.len());
        println!(
            "plain={}ms streaming+buffer={}ms overhead={:.0}%",
            plain_ms,
            stream_ms,
            (stream_ms as f64 - plain_ms as f64) / plain_ms as f64 * 100.0
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Minimal mutex for tests without adding deps (parking_lot isn't a
    // core dependency). Wraps std Mutex with a shorter name.
    struct ParkingLotLike(std::sync::Mutex<std::collections::HashSet<PathBuf>>);
    fn parking_lot_like() -> ParkingLotLike {
        ParkingLotLike(std::sync::Mutex::new(std::collections::HashSet::new()))
    }
    impl ParkingLotLike {
        fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<PathBuf>> {
            self.0.lock().unwrap()
        }
    }
}
