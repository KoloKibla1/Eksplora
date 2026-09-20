#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eksplora_core::{ListOptions, QueryOptions, ScanOptions};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tauri::{Emitter, State};

// State pieces live behind Arcs so the background scan thread can own them
// independently of any single command invocation.
struct AppState {
    index: Arc<Mutex<eksplora_core::Index>>,
    /// Last scanned root. Empty queries list this directory (browse mode).
    root: Arc<Mutex<Option<PathBuf>>>,
    /// Copy clipboard: paths staged by `copy_entry`, consumed by
    /// `paste_entry`. Mirrored onto the OS clipboard (CF_HDROP) so pasting
    /// into Explorer works too; the in-app paste reads this, not the OS.
    clipboard: Arc<Mutex<Vec<PathBuf>>>,
    /// Cancel flag of the currently running search, if any.
    search_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Cancel flag of the currently running scan, if any.
    scan_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Undo journal for deletes: original paths moved to the Recycle Bin by
    /// `delete_entry`, newest last. `undo_entry` (Ctrl+Z) restores the tail.
    undo: Arc<Mutex<Vec<PathBuf>>>,
    /// File-watch commands for the manager thread (external changes by
    /// other processes). Fire-and-forget: a full channel never blocks a
    /// scan, the tick just coalesces.
    watch_tx: Sender<WatchCmd>,
}

/// Max delete-undo steps kept. Older entries fall off the journal (the files
/// stay in the Recycle Bin — only the shortcut forgets them).
const MAX_UNDO: usize = 100;

#[derive(serde::Serialize, Clone)]
struct ScanProgressEv {
    path: String,
    files: u64,
    dirs: u64,
    /// Live per-folder counts: cumulative `(dir path, direct children walked
    /// so far)` for dirs that changed since the previous tick. Lets folder
    /// item counts tick live during the walk instead of jumping 0 -> final.
    counts: Vec<(String, u64)>,
}

#[derive(serde::Serialize, Clone)]
struct ScanPartialEv {
    path: String,
    len: usize,
    files: u64,
    dirs: u64,
    duration_ms: u128,
}

#[derive(serde::Serialize, Clone)]
struct ScanDoneEv {
    path: String,
    len: usize,
    files: u64,
    dirs: u64,
    duration_ms: u128,
    cancelled: bool,
}

/// Commands for the file-watch manager thread. Only the currently scanned
/// root is ever watched.
enum WatchCmd {
    /// (Re)watch a scanned root, replacing any previous watch.
    Watch(PathBuf),
    /// Stop watching (the path field was cleared).
    Unwatch,
}

#[derive(serde::Serialize, Clone)]
struct FsChangedEv {
    changed: usize,
}

#[derive(serde::Serialize)]
struct SearchHit {
    path: String,
    name: String,
    score: u32,
    is_dir: bool,
    size: u64,
    /// Direct children (browse mode, dirs only). 0 otherwise.
    child_count: usize,
    /// Search mode only: true for real hits, false for ancestor context rows.
    matched: bool,
}

fn cancel_in_flight(state: &State<'_, AppState>) {
    if let Some(prev) = state.search_cancel.lock().take() {
        prev.store(true, Ordering::SeqCst);
    }
}

/// Debounce window for external-change bursts: one file copy fires
/// create + N modifies, which collapse into a single index patch + UI tick.
const WATCH_DEBOUNCE_MS: u64 = 400;
/// Poll tick for watch commands and raw watcher events.
const WATCH_POLL_MS: u64 = 50;

/// Long-lived thread behind external-change tracking: owns the recursive
/// watch on the current scan root, debounces bursts into one live-index
/// patch, and nudges the UI (`fs-changed` → the frontend just re-runs its
/// search, tree and selection preserved). Best-effort throughout: a failed
/// watch or a superseded scan never errors.
///
/// Race note: an external change landing mid-scan can be overwritten by the
/// scan's final authoritative replace; it then heals on the next change or
/// navigation. The window is sub-second and Explorer shares the same race.
fn watch_manager(
    cmd_rx: Receiver<WatchCmd>,
    app: tauri::AppHandle,
    index: Arc<Mutex<eksplora_core::Index>>,
) {
    let mut watched: Option<(PathBuf, eksplora_core::watcher::WatchHandle)> = None;
    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut last_activity: Option<Instant> = None;
    loop {
        match cmd_rx.try_recv() {
            Ok(WatchCmd::Watch(p)) => {
                let same = watched.as_ref().map(|(cur, _)| cur == &p).unwrap_or(false);
                if !same {
                    watched = eksplora_core::watcher::watch(&p).ok().map(|h| (p, h));
                }
                pending.clear();
                last_activity = None;
            }
            Ok(WatchCmd::Unwatch) => {
                watched = None;
                pending.clear();
                last_activity = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if let Some((root, handle)) = &watched {
            while let Ok(ev) = handle.rx.try_recv() {
                if ev.paths.is_empty() {
                    continue;
                }
                for p in ev.paths {
                    pending.insert(p);
                }
                last_activity = Some(Instant::now());
            }
            let quiet = last_activity
                .map(|t| t.elapsed() >= Duration::from_millis(WATCH_DEBOUNCE_MS))
                .unwrap_or(false);
            if !pending.is_empty() && quiet {
                let batch: Vec<PathBuf> = pending.drain().collect();
                last_activity = None;
                let (up, del) = index.lock().apply_watch_batch(root, &batch);
                if up + del > 0 {
                    let _ = app.emit("fs-changed", FsChangedEv { changed: up + del });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(WATCH_POLL_MS));
    }
}

/// Starts a scan on a dedicated background thread and returns immediately —
/// the Tauri worker pool is never blocked, so search stays servable while a
/// big tree is being walked.
///
/// Progressive loading: a fast depth-1 `scan_shallow` publishes the first
/// layer within ~100ms (emitted as `scan-partial`), then the full walk runs
/// at full speed while a trickle of `scan-partial` events (~every 600ms) keeps
/// the list growing — including per-folder item counts. The final numbers
/// arrive as one `scan-done` event.
///
/// Why so infrequent? Each partial triggers a full re-search + re-render,
/// and every shared-index lock contends with search. Merging every batch
/// (and emitting at tens of Hz) turned a ~20s scan into ~90s in testing.
/// Buffering preview batches and merging under a single lock every 600ms
/// keeps the overhead near zero while counts still tick live.
/// Typing a new path calls this again, which cancels the previous scan.
#[tauri::command]
fn scan_dir(state: State<'_, AppState>, app: tauri::AppHandle, path: String) -> Result<bool, String> {
    let root = PathBuf::from(&path);
    if !root.exists() {
        return Err(format!("path does not exist: {}", path));
    }
    // External-change watch follows the scanned root (best-effort: a path
    // that can't be watched simply never emits `fs-changed`).
    let _ = state.watch_tx.send(WatchCmd::Watch(root.clone()));
    cancel_in_flight(&state);
    let token = Arc::new(AtomicBool::new(false));
    if let Some(prev) = state.scan_cancel.lock().replace(token.clone()) {
        prev.store(true, Ordering::SeqCst);
    }

    let index = Arc::clone(&state.index);
    let root_state = Arc::clone(&state.root);
    let cancel_state = Arc::clone(&state.scan_cancel);
    let path_s = path.clone();
    std::thread::Builder::new()
        .name("eksplora-scan".to_string())
        .spawn(move || {
            let wall = Instant::now();
            let is_latest = || {
                cancel_state
                    .lock()
                    .clone()
                    .map(|t| Arc::ptr_eq(&t, &token))
                    .unwrap_or(false)
            };
            let is_cancelled =
                || token.load(Ordering::Relaxed) || !is_latest();

            // Phase 1: first layer instantly (plain read_dir, no pool).
            let scan_opts = ScanOptions::default();
            let (shallow_entries, shallow_stats) =
                eksplora_core::scan_shallow(&root, &scan_opts);
            if is_cancelled() {
                let _ = app.emit(
                    "scan-done",
                    ScanDoneEv {
                        path: path_s,
                        len: 0,
                        files: 0,
                        dirs: 0,
                        duration_ms: wall.elapsed().as_millis(),
                        cancelled: true,
                    },
                );
                return;
            }
            let shallow_len = shallow_entries.len();
            *index.lock() = eksplora_core::Index::from_entries(shallow_entries);
            *root_state.lock() = Some(root.clone());
            let _ = app.emit(
                "scan-partial",
                ScanPartialEv {
                    path: path_s.clone(),
                    len: shallow_len,
                    files: shallow_stats.files,
                    dirs: shallow_stats.dirs,
                    duration_ms: wall.elapsed().as_millis(),
                },
            );

            // Phase 2: deep walk at full speed. Preview batches are buffered
            // lock-free and merged into the live index at most every 600ms
            // under a single lock — frequent merging contended with search
            // and quadrupled wall time on large trees. The final `from_entries`
            // replace below is authoritative, so the preview is best-effort:
            // whatever is buffered at the end is included via the replace.
            let last_emit = std::cell::Cell::new(Instant::now());
            let last_progress = std::cell::Cell::new(Instant::now() - Duration::from_secs(60));
            let flag: &AtomicBool = &token;
            let index_cb = &index;
            let root_cb = root.clone();
            let path_cb = path_s.clone();
            let app_cb = &app;
            // Running totals for the deep walk (for partial event payloads).
            let deep_files = std::cell::Cell::new(0u64);
            let deep_dirs = std::cell::Cell::new(0u64);
            // Lock-free staging: batch callbacks only push clones here.
            // Drained into the shared index on the 600ms emit tick.
            let pending: std::cell::RefCell<Vec<eksplora_core::FileEntry>> =
                std::cell::RefCell::new(Vec::new());
            // Live per-folder counts, counted straight from the walk stream
            // (cumulative, so always a correct-so-far lower bound converging
            // on the final index counts). Drained into scan-progress events
            // at ~7Hz — far livelier than the index-merge tick, with no
            // extra index locking.
            let live_counts: RefCell<eksplora_core::LiveDirCounts> =
                RefCell::new(eksplora_core::LiveDirCounts::new());
            let emit_partial = |len: usize| {
                let _ = app_cb.emit(
                    "scan-partial",
                    ScanPartialEv {
                        path: path_cb.clone(),
                        len,
                        files: deep_files.get(),
                        dirs: deep_dirs.get(),
                        duration_ms: wall.elapsed().as_millis(),
                    },
                );
            };
            let (entries, stats) = eksplora_core::scan_with_full_streaming(
                &root_cb,
                &scan_opts,
                Some(flag),
                Some(&|files: u64, dirs: u64| {
                    deep_files.set(files);
                    deep_dirs.set(dirs);
                    if last_progress.get().elapsed() >= Duration::from_millis(150) {
                        last_progress.set(Instant::now());
                        // Only dirs that changed since the previous tick;
                        // cumulative values, so a skipped tick loses nothing.
                        let counts: Vec<(String, u64)> = live_counts
                            .borrow_mut()
                            .drain_changed()
                            .into_iter()
                            .map(|(p, n)| (p.to_string_lossy().into_owned(), n))
                            .collect();
                        let _ = app_cb.emit(
                            "scan-progress",
                            ScanProgressEv { path: path_cb.clone(), files, dirs, counts },
                        );
                    }
                }),
                Some(&|batch: &[eksplora_core::FileEntry]| {
                    if batch.is_empty() {
                        return;
                    }
                    // Stale scan: ignore the batch; the walk aborts shortly
                    // via its own cancel check and the final replace is
                    // guarded by the latest-token check below.
                    let latest = cancel_state
                        .lock()
                        .clone()
                        .map(|t| Arc::ptr_eq(&t, &token))
                        .unwrap_or(false);
                    if !latest || token.load(Ordering::Relaxed) {
                        return;
                    }
                    pending.borrow_mut().extend(batch.iter().cloned());
                    live_counts.borrow_mut().add_batch(batch);
                    if last_emit.get().elapsed() >= Duration::from_millis(600) {
                        last_emit.set(Instant::now());
                        let staged = std::mem::take(&mut *pending.borrow_mut());
                        if !staged.is_empty() {
                            let mut idx = index_cb.lock();
                            idx.merge_entries(staged);
                            emit_partial(idx.len());
                        }
                    }
                }),
            );
            // Publish only if still the latest requested scan — a newer path
            // (or an explicit cancel) must never be overwritten by stale data.
            let latest = is_latest();
            let cancelled = stats.cancelled || !latest;
            if !cancelled {
                // Authoritative replace: single bulk build (reserved upfront),
                // exactly the pre-progressive cost. This also absorbs any
                // preview batches still sitting in `pending`.
                let _ = pending.take();
                let done = ScanDoneEv {
                    path: path_s,
                    len: entries.len(),
                    files: stats.files,
                    dirs: stats.dirs,
                    duration_ms: wall.elapsed().as_millis(),
                    cancelled: false,
                };
                *index.lock() = eksplora_core::Index::from_entries(entries);
                *root_state.lock() = Some(root);
                let _ = app.emit("scan-done", done);
            } else {
                let _ = app.emit(
                    "scan-done",
                    ScanDoneEv {
                        path: path_s,
                        len: 0,
                        files: 0,
                        dirs: 0,
                        duration_ms: wall.elapsed().as_millis(),
                        cancelled: true,
                    },
                );
            }
        })
        .map_err(|e| format!("spawn scan thread: {}", e))?;
    Ok(true)
}

/// Cancels the running scan, if any (e.g. the path field was cleared).
#[tauri::command]
fn scan_cancel(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(prev) = state.scan_cancel.lock().take() {
        prev.store(true, Ordering::Relaxed);
    }
    let _ = state.watch_tx.send(WatchCmd::Unwatch);
    Ok(())
}

#[tauri::command]
fn search_index(
    state: State<'_, AppState>,
    query: String,
    limit: Option<usize>,
    fuzzy: Option<bool>,
    depth: Option<usize>,
) -> Result<Vec<SearchHit>, String> {
    // Every new query cancels the previous one so stale work never
    // overwrites (or delays) fresher results.
    let token = Arc::new(AtomicBool::new(false));
    if let Some(prev) = state.search_cancel.lock().replace(token.clone()) {
        prev.store(true, Ordering::SeqCst);
    }
    let limit = limit.unwrap_or(200).clamp(1, 5000);

    // Browse mode: empty query lists direct children of the scanned root.
    if query.trim().is_empty() {
        let root = state.root.lock().clone();
        let Some(dir) = root else {
            return Ok(Vec::new());
        };
        let idx = state.index.lock();
        let kids = eksplora_core::list_dir(
            &idx,
            &dir,
            &ListOptions {
                limit,
                dirs_first: true,
                max_depth: Some(depth.unwrap_or(1).clamp(1, 32)),
            },
        );
        return Ok(kids
            .into_iter()
            .map(|l| SearchHit {
                path: l.entry.path.to_string_lossy().into_owned(),
                name: l.entry.name.clone(),
                score: 0,
                is_dir: l.entry.is_dir,
                size: l.entry.size,
                child_count: l.child_count,
                matched: false,
            })
            .collect());
    }

    // Filtered tree: ranked matches plus ancestor chains, nested in place.
    // Depth filters do not apply — lower-layer matches show anyways.
    let root = state.root.lock().clone();
    let Some(dir) = root else {
        return Ok(Vec::new()); // nothing scanned yet, no tree to anchor
    };
    let idx = state.index.lock();
    match eksplora_core::search_tree_cancelable(
        &idx,
        &dir,
        &query,
        &QueryOptions {
            limit,
            fuzzy: fuzzy.unwrap_or(true),
        },
        &token,
    ) {
        Some(rows) => Ok(rows
            .into_iter()
            .map(|t| SearchHit {
                path: t.entry.path.to_string_lossy().into_owned(),
                name: t.entry.name.clone(),
                score: t.score,
                is_dir: t.entry.is_dir,
                size: t.entry.size,
                child_count: t.child_count,
                matched: t.matched,
            })
            .collect()),
        None => Err("cancelled".to_string()),
    }
}

/// Path-field Tab completion: drives + known folders for empty input,
/// child directories filtered by the partial name otherwise.
#[tauri::command]
fn complete_path(input: String, limit: Option<usize>) -> Result<Vec<eksplora_core::Completion>, String> {
    Ok(eksplora_core::complete_path(&input, limit.unwrap_or(50)))
}

/// Opens a file with its default application (double-click on a file row).
/// Validation runs synchronously (cheap metadata); the `ShellExecuteW` call
/// itself runs on a throwaway thread so the IPC roundtrip returns instantly
/// — the shell can take hundreds of ms resolving the association and cold
/// starting the target app, and none of that should block the UI.
/// Failures there are rare and only logged (the path was just validated).
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("no longer exists: {}", path));
    }
    if p.is_dir() {
        return Err("use navigation for directories".to_string());
    }
    std::thread::Builder::new()
        .name("eksplora-open".to_string())
        .spawn(move || {
            if let Err(e) = eksplora_core::windows_integration::open_with_default_app(&p) {
                eprintln!("open failed for {}: {:#}", p.display(), e);
            }
        })
        .map_err(|e| format!("open failed: {}", e))?;
    Ok(())
}

/// Context menu: copy a file/folder to the clipboard (no duplicate).
/// Stages the path for in-app paste AND writes CF_HDROP to the OS clipboard
/// so pasting into Explorer works. Returns the staged path.
#[tauri::command]
fn copy_entry(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("no longer exists: {}", path));
    }
    *state.clipboard.lock() = vec![p.clone()];
    // OS mirror is best-effort: in-app paste reads the state above and
    // must keep working even if another app is holding the clipboard.
    if let Err(e) = eksplora_core::windows_integration::set_clipboard_files(&[p]) {
        eprintln!("os clipboard mirror failed: {:#}", e);
    }
    Ok(path)
}

/// Paste clipboard contents into `target_dir`. Keeps original names when
/// free, appends `-copy` on conflict. Patches the live index per new path
/// (no rescan). Returns the created paths.
#[tauri::command]
fn paste_entry(
    state: State<'_, AppState>,
    target_dir: String,
) -> Result<Vec<String>, String> {
    let dir = PathBuf::from(&target_dir);
    if !dir.is_dir() {
        return Err(format!("not a folder: {}", target_dir));
    }
    let staged: Vec<PathBuf> = state.clipboard.lock().clone();
    if staged.is_empty() {
        return Err("clipboard is empty".to_string());
    }
    let mut created: Vec<String> = Vec::new();
    let mut idx = state.index.lock();
    for src in &staged {
        let dst = eksplora_core::fs_ops::paste_into(src, &dir).map_err(|e| e.to_string())?;
        idx.stat_and_upsert(&dst);
        created.push(dst.to_string_lossy().into_owned());
    }
    Ok(created)
}

/// Context menu: duplicate with an explicit new name (refuses on conflict —
/// the UI shows the message). Returns the new path.
#[tauri::command]
fn duplicate_entry(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<String, String> {
    let new_path = eksplora_core::fs_ops::duplicate_path(&PathBuf::from(&path), &new_name)
        .map_err(|e| e.to_string())?;
    state.index.lock().stat_and_upsert(&new_path);
    Ok(new_path.to_string_lossy().into_owned())
}

/// Context menu: rename within the same folder. Returns the new path.
/// Drops the old entry plus stale children first (dir renames move whole
/// subtrees), then stats the new path (dirs re-pull their children).
#[tauri::command]
fn rename_entry(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<String, String> {
    let old = PathBuf::from(&path);
    let new_path =
        eksplora_core::fs_ops::rename_entry(&old, &new_name).map_err(|e| e.to_string())?;
    {
        let mut idx = state.index.lock();
        idx.remove_prefix(&old);
        idx.stat_and_upsert(&new_path);
    }
    Ok(new_path.to_string_lossy().into_owned())
}

/// Context menu: move a file/folder to the Recycle Bin.
/// Removes the entry plus any children from the live index — no rescan.
/// Pushes the original path onto the undo journal for Ctrl+Z restore.
#[tauri::command]
fn delete_entry(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    eksplora_core::fs_ops::delete_path(&p).map_err(|e| e.to_string())?;
    {
        let mut idx = state.index.lock();
        idx.remove(&p);
        idx.remove_prefix(&p);
    }
    {
        let mut undo = state.undo.lock();
        undo.push(p);
        if undo.len() > MAX_UNDO {
            let drop = undo.len() - MAX_UNDO;
            undo.drain(..drop);
        }
    }
    Ok(())
}

/// Ctrl+Z: restore the most recently deleted item from the Recycle Bin to
/// its original place. Re-indexes the restored path (dirs re-pull their
/// children). Returns the restored path. A failed restore drops the journal
/// entry and reports why (e.g. the bin was emptied since).
#[tauri::command]
fn undo_entry(state: State<'_, AppState>) -> Result<String, String> {
    let orig: PathBuf = state
        .undo
        .lock()
        .pop()
        .ok_or_else(|| "nothing to undo".to_string())?;
    let restored =
        eksplora_core::fs_ops::restore_from_bin(&orig).map_err(|e| e.to_string())?;
    state.index.lock().stat_and_upsert(&restored);
    Ok(restored.to_string_lossy().into_owned())
}

/// Drag and drop: move a file/folder into another folder, keeping its name.
/// No-op when dropped into its own folder; refuses on name conflict and
/// when a folder would land inside itself. Returns the final path.
#[tauri::command]
fn move_entry(
    state: State<'_, AppState>,
    path: String,
    target_dir: String,
) -> Result<String, String> {
    let old = PathBuf::from(&path);
    let new_path = eksplora_core::fs_ops::move_into(&old, &PathBuf::from(&target_dir))
        .map_err(|e| e.to_string())?;
    {
        let mut idx = state.index.lock();
        if new_path != old {
            idx.remove(&old);
            idx.remove_prefix(&old);
        }
        idx.stat_and_upsert(&new_path);
    }
    Ok(new_path.to_string_lossy().into_owned())
}

/// Lists direct children of any indexed directory (tree expand).
/// `path` need not be the scanned root — used to splice one layer below an
/// expanded row without re-scanning or changing the current root.
#[tauri::command]
fn list_children(
    state: State<'_, AppState>,
    path: String,
    limit: Option<usize>,
) -> Result<Vec<SearchHit>, String> {
    let dir = PathBuf::from(&path);
    let idx = state.index.lock();
    let kids = eksplora_core::list_dir(
        &idx,
        &dir,
        &ListOptions {
            limit: limit.unwrap_or(2000).clamp(1, 5000),
            dirs_first: true,
            max_depth: Some(1),
        },
    );
    Ok(kids
        .into_iter()
        .map(|l| SearchHit {
            path: l.entry.path.to_string_lossy().into_owned(),
            name: l.entry.name.clone(),
            score: 0,
            is_dir: l.entry.is_dir,
            size: l.entry.size,
            child_count: l.child_count,
            matched: false,
        })
        .collect())
}

#[tauri::command]
fn desktop_path() -> Result<String, String> {
    let folders =
        eksplora_core::windows_integration::known_folders().map_err(|e| e.to_string())?;
    for (name, p) in folders {
        if name == "Desktop" {
            return Ok(p.to_string_lossy().into_owned());
        }
    }
    Err("Desktop not found".into())
}

#[tauri::command]
fn sysinfo() -> Result<String, String> {
    let root = PathBuf::from("C:\\");
    let mut out = String::new();
    match eksplora_core::windows_integration::known_folders() {
        Ok(f) => {
            for (n, p) in f {
                out.push_str(&format!("{}: {}\n", n, p.display()));
            }
        }
        Err(e) => out.push_str(&format!("known_folders error: {}\n", e)),
    }
    out.push_str(&eksplora_core::windows_integration::usn_status(&root));
    out.push('\n');
    out.push_str(&eksplora_core::usn::status_string(&root));
    Ok(out)
}

fn main() {
    let (watch_tx, watch_rx) = std::sync::mpsc::channel::<WatchCmd>();
    // Built here (not inline in `.manage`) so the watch manager thread can
    // share the live index.
    let index = Arc::new(Mutex::new(eksplora_core::Index::new()));
    let watch_index = Arc::clone(&index);
    tauri::Builder::default()
        .manage(AppState {
            index,
            root: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(Vec::new())),
            search_cancel: Arc::new(Mutex::new(None)),
            scan_cancel: Arc::new(Mutex::new(None)),
            undo: Arc::new(Mutex::new(Vec::new())),
            watch_tx,
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let _watcher_thread = std::thread::Builder::new()
                .name("eksplora-watch".to_string())
                .spawn(move || watch_manager(watch_rx, handle, watch_index))
                .expect("spawn watch thread");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            scan_cancel,
            search_index,
            complete_path,
            open_path,
            copy_entry,
            paste_entry,
            duplicate_entry,
            rename_entry,
            delete_entry,
            undo_entry,
            move_entry,
            list_children,
            desktop_path,
            sysinfo
        ])
        .run(tauri::generate_context!())
        .expect("error while running eksplora");
}
