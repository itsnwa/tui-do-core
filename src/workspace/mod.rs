//! The task workspace: a folder of markdown files (tasks, projects, plans,
//! daily notes, weekly updates) shared with the original TUI-DO app.
//!
//! Behavior follows the app's services, with a few deliberate changes that a
//! single writer makes possible: writes are atomic and serialized (no
//! duplicate task IDs), slugs are validated, creating something whose name is
//! taken picks a free slug instead of overwriting, due dates are always
//! written as `'YYYY-MM-DD'`, unknown frontmatter keys are preserved, and
//! listing never writes.

pub mod daily;
pub mod dates;
pub mod plans;
pub mod projects;
pub mod schedule;
pub mod shared;
pub mod slug;
pub mod store;
pub mod tasks;
pub mod watch;
pub mod weekly;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub use store::Dir;
use store::Store;

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    Io(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(msg) | Error::Invalid(msg) => f.write_str(msg),
            Error::Io(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Self {
        Error::Io(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Workspace {
    store: Store,
    /// Held for the whole of every read-modify-write operation.
    write_lock: Mutex<()>,
}

impl Workspace {
    /// Open a workspace folder, creating its directories if needed.
    pub fn open(root: PathBuf) -> Result<Self> {
        for dir in Dir::ALL {
            std::fs::create_dir_all(root.join(dir.name()))?;
        }
        Ok(Self {
            store: Store::new(root),
            write_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        self.store.root()
    }

    fn writing(&self) -> MutexGuard<'_, ()> {
        lock(&self.write_lock)
    }

    pub fn attachments_dir(&self, task_slug: &str) -> PathBuf {
        self.root().join("attachments").join(task_slug)
    }
}

/// An attachment's path on this machine. Attachments stored with a task are
/// saved as absolute paths into the workspace of the home that stored them
/// (`…/attachments/<task>/<file>`); every core has a copy of that folder, so
/// such a path maps to the same file under `root`. Other paths are kept.
pub fn local_attachment(path: &str, root: &Path) -> String {
    if let Some((task, file)) = stored_tail(path) {
        let local = root.join("attachments").join(task).join(file);
        if slug::validate(task).is_ok() && !file.starts_with('.') && local.is_file() {
            return local.display().to_string();
        }
    }
    path.to_string()
}

/// `…/attachments/<task>/<file>`, for a stored attachment's path.
fn stored_tail(path: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = path.rsplitn(4, '/').collect();
    match parts.as_slice() {
        [file, task, "attachments", _] => Some((task, file)),
        _ => None,
    }
}

/// Whether two paths name the same attachment: equal, or the same stored
/// file in two machines' copies of the workspace.
pub fn same_attachment(a: &str, b: &str) -> bool {
    a == b || stored_tail(a).is_some_and(|tail| stored_tail(b) == Some(tail))
}

/// Map every task's `attachments` in a JSON answer to paths on this machine.
pub fn localize_attachments(value: &mut serde_json::Value, root: &Path) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if key == "attachments"
                    && let serde_json::Value::Array(list) = item
                {
                    for entry in list.iter_mut() {
                        if let serde_json::Value::String(path) = entry {
                            *path = local_attachment(path, root);
                        }
                    }
                } else {
                    localize_attachments(item, root);
                }
            }
        }
        serde_json::Value::Array(list) => {
            for item in list {
                localize_attachments(item, root);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[test]
    fn stored_attachments_should_map_to_this_machines_copy() {
        let root = std::env::temp_dir().join(format!("tui-do-att-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("attachments/my-task")).expect("dir");
        std::fs::write(root.join("attachments/my-task/shot.png"), "png").expect("write");
        let mac = "/Users/me/Library/Mobile Documents/x/Planning/attachments/my-task/shot.png";
        assert_eq!(
            local_attachment(mac, &root),
            root.join("attachments/my-task/shot.png")
                .display()
                .to_string()
        );
        // Not in the copy, or not a stored attachment: unchanged
        let missing = "/Users/me/Planning/attachments/my-task/other.png";
        assert_eq!(local_attachment(missing, &root), missing);
        assert_eq!(
            local_attachment("/Users/me/Downloads/x.png", &root),
            "/Users/me/Downloads/x.png"
        );
        assert!(same_attachment(
            mac,
            "/home/me/.tui-do/workspace/attachments/my-task/shot.png"
        ));
        assert!(!same_attachment(
            mac,
            "/Users/me/Planning/attachments/other/shot.png"
        ));
        let mut answer = serde_json::json!({ "task": { "attachments": [mac] } });
        localize_attachments(&mut answer, &root);
        assert_eq!(
            answer["task"]["attachments"][0],
            root.join("attachments/my-task/shot.png")
                .display()
                .to_string()
        );
    }
}
