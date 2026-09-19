#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eksplora_core::{ListOptions, QueryOptions, ScanOptions};
use parking_lot::Mutex;
use std::path::PathBuf;
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
    /// Cancel flag of the currently running search, if any.
    search_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Cancel flag of the currently running scan, if any.
    scan_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

#[derive(serde::Serialize, Clone)]
struct ScanProgressEv {
    path: String,
    files: u64,
    dirs: u64,
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

/// Starts a scan on a dedicated background thread and returns immediately —
/// the Tauri worker pool is never blocked, so search stays servable while a
/// big tree is being walked.
///
/// Progressive loading: a fast depth-1 `scan_shallow` publishes the first
/// layer within ~100ms (emitted as `scan-partial`), then the full walk runs
/// at full speed while a trickle of `scan-partial` events (~every 2s) keeps
/// the list growing. The final numbers arrive as one `scan-done` event.
///
/// Why so infrequent? Each partial triggers a full re-search + re-render,
/// and every shared-index lock contends with search. Merging every batch
/// (and emitting at ~2Hz) turned a ~20s scan into ~90s in testing. Buffering
/// preview batches and merging under a single lock every 2s keeps the
/// overhead near zero while the UI still feels live.
/// Typing a new path calls this again, which cancels the previous scan.
#[tauri::command]
fn scan_dir(state: State<'_, AppState>, app: tauri::AppHandle, path: String) -> Result<bool, String> {
    let root = PathBuf::from(&path);
    if !root.exists() {
        return Err(format!("path does not exist: {}", path));
    }
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
            // lock-free and merged into the live index at most every 2s under
            // a single lock — frequent merging contended with search and
            // quadrupled wall time on large trees. The final `from_entries`
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
            // Drained into the shared index on the 2s emit tick.
            let pending: std::cell::RefCell<Vec<eksplora_core::FileEntry>> =
                std::cell::RefCell::new(Vec::new());
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
                        let _ = app_cb.emit(
                            "scan-progress",
                            ScanProgressEv { path: path_cb.clone(), files, dirs },
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
                    if last_emit.get().elapsed() >= Duration::from_millis(2000) {
                        last_emit.set(Instant::now());
                        let staged = std::mem::take(&mut *pending.borrow_mut());
                        if !staged.is_empty() {
                            let mut idx = index_cb.lock();
                            idx.reserve(staged.len());
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
        prev.store(true, Ordering::SeqCst);
    }
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
    tauri::Builder::default()
        .manage(AppState {
            index: Arc::new(Mutex::new(eksplora_core::Index::new())),
            root: Arc::new(Mutex::new(None)),
            search_cancel: Arc::new(Mutex::new(None)),
            scan_cancel: Arc::new(Mutex::new(None)),
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            scan_cancel,
            search_index,
            complete_path,
            desktop_path,
            sysinfo
        ])
        .run(tauri::generate_context!())
        .expect("error while running eksplora");
}
