//! Reading and writing workspace files.
//!
//! Reads go through a parse cache keyed by path and invalidated by mtime and
//! size, so listing ~1000 tasks doesn't re-parse unchanged files (external
//! edits from iCloud or an editor are picked up by the changed mtime).
//! Writes go to a temporary file that is renamed into place, so a reader (or
//! iCloud) never sees a half-written file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;

use super::{Error, Result, lock};
use crate::format::{self, Document, Map};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Tasks,
    Projects,
    Plans,
    Daily,
    Weekly,
}

impl Dir {
    pub const ALL: [Dir; 5] = [
        Dir::Projects,
        Dir::Tasks,
        Dir::Plans,
        Dir::Daily,
        Dir::Weekly,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Dir::Tasks => "tasks",
            Dir::Projects => "projects",
            Dir::Plans => "plans",
            Dir::Daily => "daily",
            Dir::Weekly => "weekly",
        }
    }
}

pub struct Entry {
    pub slug: String,
    pub doc: Document,
}

struct Cached {
    mtime: SystemTime,
    size: u64,
    doc: Document,
}

pub struct Store {
    root: PathBuf,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir_path(&self, dir: Dir) -> PathBuf {
        self.root.join(dir.name())
    }

    pub fn path(&self, dir: Dir, slug: &str) -> PathBuf {
        self.dir_path(dir).join(format!("{slug}.md"))
    }

    pub fn exists(&self, dir: Dir, slug: &str) -> bool {
        self.path(dir, slug).is_file()
    }

    /// Parse a file, or `None` if it doesn't exist. The body is trimmed.
    pub fn read(&self, dir: Dir, slug: &str) -> Result<Option<Document>> {
        let path = self.path(dir, slug);
        match self.read_path(&path) {
            Ok(doc) => Ok(Some(doc)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Like [`Store::read`], but a missing file is an error.
    pub fn read_existing(&self, dir: Dir, slug: &str, what: &str) -> Result<Document> {
        self.read(dir, slug)?
            .ok_or_else(|| Error::NotFound(format!("{what} not found: {slug}")))
    }

    fn read_path(&self, path: &Path) -> Result<Document> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(path.display().to_string()));
            }
            Err(err) => return Err(Error::Io(err.into())),
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size = meta.len();
        if let Some(hit) = lock(&self.cache).get(path)
            && hit.mtime == mtime
            && hit.size == size
        {
            return Ok(hit.doc.clone());
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let raw = String::from_utf8_lossy(&bytes);
        let doc = format::parse_file(&raw)
            .map_err(|err| Error::Invalid(format!("{}: {err}", path.display())))?;
        lock(&self.cache).insert(
            path.to_path_buf(),
            Cached {
                mtime,
                size,
                doc: doc.clone(),
            },
        );
        Ok(doc)
    }

    /// Every `*.md` file in a directory, sorted by file name (byte order, as
    /// Node's `readdir` returns them, which the app's ordering relies on).
    /// Files that fail to parse are logged and skipped.
    pub fn list(&self, dir: Dir) -> Result<Vec<Entry>> {
        let dir_path = self.dir_path(dir);
        let read_dir = match std::fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(err.into())),
        };
        let mut names: Vec<String> = read_dir
            .filter_map(|item| item.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".md"))
            .collect();
        names.sort_unstable();
        let mut entries = Vec::new();
        for name in names {
            let slug = &name[..name.len() - ".md".len()];
            match self.read_path(&dir_path.join(&name)) {
                Ok(doc) => entries.push(Entry {
                    slug: slug.to_string(),
                    doc,
                }),
                Err(Error::NotFound(_)) => {} // removed while listing
                Err(err) => tracing::warn!(%err, "skipping unreadable file"),
            }
        }
        Ok(entries)
    }

    /// Write a file atomically.
    pub fn write(&self, dir: Dir, slug: &str, data: &Map, content: &str) -> Result<()> {
        let dir_path = self.dir_path(dir);
        std::fs::create_dir_all(&dir_path)
            .with_context(|| format!("creating {}", dir_path.display()))?;
        let path = self.path(dir, slug);
        // The temp name doesn't end in ".md", so listings never pick it up.
        let tmp = dir_path.join(format!(".{slug}.md.tmp"));
        std::fs::write(&tmp, format::stringify(content, data))
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        lock(&self.cache).remove(&path);
        Ok(())
    }

    /// Delete a file. Returns whether it existed.
    pub fn remove(&self, dir: Dir, slug: &str) -> Result<bool> {
        let path = self.path(dir, slug);
        lock(&self.cache).remove(&path);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::Io(err.into())),
        }
    }
}
