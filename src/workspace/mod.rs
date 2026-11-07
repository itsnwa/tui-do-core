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
