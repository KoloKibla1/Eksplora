use anyhow::Result;
use crossbeam_channel::{Receiver, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub paths: Vec<PathBuf>,
    pub kind: String,
}

pub struct WatchHandle {
    _watcher: RecommendedWatcher,
    pub rx: Receiver<FsEvent>,
}

/// Non-blocking recursive watch via ReadDirectoryChangesW (notify crate).
/// Caller polls `rx` and updates index incrementally (rescan affected parent).
pub fn watch(path: &Path) -> Result<WatchHandle> {
    let (tx, rx) = unbounded();
    let tx2 = tx.clone();

    let mut watcher: RecommendedWatcher = Watcher::new(
        move |res: Result<notify::Event, notify::Error>| match res {
            Ok(ev) => {
                let _ = tx2.send(FsEvent {
                    paths: ev.paths,
                    kind: format!("{:?}", ev.kind),
                });
            }
            Err(e) => eprintln!("watch error: {e:?}"),
        },
        notify::Config::default(),
    )?;
    watcher.watch(path, RecursiveMode::Recursive)?;
    Ok(WatchHandle { _watcher: watcher, rx })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn watch_reports_external_create_and_delete() {
        let mut root = std::env::temp_dir();
        root.push(format!("eksplora-test-{}-watch", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let handle = watch(&root).expect("watch must start on a plain temp dir");

        // External process simulation: create then delete a file.
        std::fs::write(root.join("outside.txt"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_create = false;
        while Instant::now() < deadline {
            match handle.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    if ev.paths.iter().any(|p| p == &root.join("outside.txt")) {
                        saw_create = true;
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        assert!(saw_create, "watcher must report the externally created file");
        std::fs::remove_file(root.join("outside.txt")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_delete = false;
        while Instant::now() < deadline {
            match handle.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    if ev.paths.iter().any(|p| p == &root.join("outside.txt")) {
                        saw_delete = true;
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        assert!(saw_delete, "watcher must report the externally deleted file");
        let _ = std::fs::remove_dir_all(&root);
    }
}
