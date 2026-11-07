//! Watches the workspace for changes (the core's own writes, edits in an
//! editor, iCloud syncing from other Macs) and reports which areas changed.
//!
//! Like the app, changes are coalesced over a 150 ms window that starts with
//! the first event, then reported once.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::store::Dir;

const WINDOW: Duration = Duration::from_millis(150);

fn area_of(root: &Path, path: &Path) -> Option<&'static str> {
    let first = path.strip_prefix(root).ok()?.components().next()?;
    let name = first.as_os_str().to_str()?;
    Dir::ALL.iter().map(|d| d.name()).find(|n| *n == name)
}

/// Start watching `root`. `on_change` gets the changed areas (`tasks`,
/// `plans`, …). Dropping the returned watcher stops it.
pub fn watch(
    root: PathBuf,
    on_change: impl Fn(Vec<String>) + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    let (tx, rx) = mpsc::channel::<Vec<&'static str>>();
    let event_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let areas = match res {
            Ok(event) if event.paths.is_empty() => Dir::ALL.iter().map(|d| d.name()).collect(),
            Ok(event) => event
                .paths
                .iter()
                .filter_map(|p| area_of(&event_root, p))
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "workspace watcher error");
                return;
            }
        };
        let _ = tx.send(areas);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    std::thread::Builder::new()
        .name("workspace-watch".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut areas: BTreeSet<&'static str> = first.into_iter().collect();
                let deadline = Instant::now() + WINDOW;
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    match rx.recv_timeout(left) {
                        Ok(more) => areas.extend(more),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if !areas.is_empty() {
                    on_change(areas.into_iter().map(str::to_string).collect());
                }
            }
        })
        .map_err(|e| notify::Error::generic(&e.to_string()))?;
    Ok(watcher)
}
