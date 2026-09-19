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
