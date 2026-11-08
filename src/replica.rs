//! Replicas: every core keeps a copy of the workspace. The home's copy is the
//! one that's written; it pushes changes to the other cores, which serve
//! theirs read-only (an offline view, and a quick switch when the home moves).
//!
//! A core describes its copy as a manifest (each file's size and mtime). The
//! home compares it with its own and sends only the files that differ, in
//! batches: a JSON header line (the files, in order, and the paths to
//! delete), then the files' bytes. Files keep the home's mtimes, so the next
//! comparison matches. Dotfiles (`.DS_Store`, temporary files) are left out.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::fleet::Device;

/// The workspace folders that are copied.
pub const AREAS: [&str; 6] = [
    "projects",
    "tasks",
    "plans",
    "daily",
    "weekly",
    "attachments",
];

/// How many bytes of files go in one batch (a larger file goes alone).
const BATCH_BYTES: u64 = 16 * 1024 * 1024;

/// A copy of the workspace, as `path → [size, mtime in ms]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: BTreeMap<String, (u64, i64)>,
    /// False when iCloud hasn't downloaded some files (`.name.icloud`
    /// placeholders), so missing files mustn't be deleted elsewhere.
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileEntry {
    path: String,
    size: u64,
    mtime_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BatchHeader {
    files: Vec<FileEntry>,
    #[serde(default)]
    delete: Vec<String>,
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A workspace-relative path that may be copied: inside one of the areas,
/// with no empty, `.`-prefixed or `..` components.
pub fn valid_path(path: &str) -> bool {
    let mut parts = path.split('/');
    let area_ok = parts.next().is_some_and(|area| AREAS.contains(&area));
    let parts: Vec<&str> = parts.collect();
    area_ok
        && !parts.is_empty()
        && parts
            .iter()
            .all(|p| !p.is_empty() && !p.starts_with('.') && !p.contains(['\\', '\0']))
}

/// Describe the workspace copy at `root` (empty if it doesn't exist).
pub fn manifest(root: &Path) -> Manifest {
    let mut manifest = Manifest {
        files: BTreeMap::new(),
        complete: true,
    };
    for area in AREAS {
        walk(&root.join(area), area, &mut manifest);
    }
    manifest
}

fn walk(dir: &Path, prefix: &str, manifest: &mut Manifest) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            if name.ends_with(".icloud") {
                manifest.complete = false;
            }
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = format!("{prefix}/{name}");
        if kind.is_dir() {
            walk(&entry.path(), &path, manifest);
        } else if kind.is_file()
            && let Ok(meta) = entry.metadata()
        {
            manifest.files.insert(path, (meta.len(), mtime_ms(&meta)));
        }
    }
}

/// What to send so `replica` matches `home`, and what to delete there.
/// Nothing is deleted when the home's copy is incomplete or has no tasks.
pub fn diff(home: &Manifest, replica: &Manifest) -> (Vec<String>, Vec<String>) {
    let send = home
        .files
        .iter()
        .filter(|(path, stamp)| replica.files.get(*path) != Some(stamp))
        .map(|(path, _)| path.clone())
        .collect();
    let has_tasks = home.files.keys().any(|p| p.starts_with("tasks/"));
    let delete = if home.complete && has_tasks {
        replica
            .files
            .keys()
            .filter(|p| !home.files.contains_key(*p))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    (send, delete)
}

/// Read `paths` from `root` into one batch, with `delete` in its header.
fn encode(root: &Path, paths: &[String], delete: Vec<String>) -> anyhow::Result<Vec<u8>> {
    let mut header = BatchHeader {
        files: Vec::with_capacity(paths.len()),
        delete,
    };
    let mut data = Vec::new();
    for path in paths {
        let full = root.join(path);
        let Some((bytes, mtime_ms)) = read_steady(&full)? else {
            // Deleted since the manifest was made; the next push deletes it
            continue;
        };
        header.files.push(FileEntry {
            path: path.clone(),
            size: bytes.len() as u64,
            mtime_ms,
        });
        data.extend_from_slice(&bytes);
    }
    let mut body = serde_json::to_vec(&header)?;
    body.push(b'\n');
    body.extend_from_slice(&data);
    Ok(body)
}

/// A file's bytes with the mtime they were read at: an edit while reading
/// would pair old bytes with a new mtime, which no later comparison catches,
/// so the read is retried until the file holds still. `None` if it's gone.
fn read_steady(path: &Path) -> anyhow::Result<Option<(Vec<u8>, i64)>> {
    let stamp = |meta: &std::fs::Metadata| (meta.len(), mtime_ms(meta));
    for _ in 0..5 {
        let before = match std::fs::metadata(path) {
            Ok(meta) => stamp(&meta),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let after = std::fs::metadata(path).map(|m| stamp(&m)).ok();
        if after == Some(before) && before.0 == bytes.len() as u64 {
            return Ok(Some((bytes, before.1)));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("{} kept changing while it was read", path.display())
}

/// One batch is written at a time, so two pushes can't interleave.
static APPLYING: Mutex<()> = Mutex::new(());

/// Write a batch into the copy at `root`. Returns how many files changed.
/// Deletions go first, so a file renamed only in case (on a case-insensitive
/// disk) isn't deleted right after it's written.
pub fn apply(root: &Path, body: &[u8]) -> anyhow::Result<usize> {
    let _applying = APPLYING.lock().unwrap_or_else(|p| p.into_inner());
    let newline = body
        .iter()
        .position(|b| *b == b'\n')
        .context("the batch has no header")?;
    let header: BatchHeader =
        serde_json::from_slice(&body[..newline]).context("the batch header isn't valid")?;
    let mut data = &body[newline + 1..];
    let total: u64 = header.files.iter().map(|f| f.size).sum();
    if total != data.len() as u64 {
        bail!(
            "the batch is {} bytes short or long",
            total.abs_diff(data.len() as u64)
        );
    }
    if let Some(bad) = header
        .files
        .iter()
        .map(|f| f.path.as_str())
        .chain(header.delete.iter().map(String::as_str))
        .find(|p| !valid_path(p))
    {
        bail!("invalid path {bad:?}");
    }
    for path in &header.delete {
        let full = root.join(path);
        match std::fs::remove_file(&full) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("deleting {}", full.display())),
        }
        // A task's attachments folder goes once it's empty
        if let Some(parent) = full.parent()
            && parent != root.join("attachments")
            && parent.starts_with(root.join("attachments"))
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
    for file in &header.files {
        let (bytes, rest) = data.split_at(usize::try_from(file.size)?);
        data = rest;
        write_file(&root.join(&file.path), bytes, file.mtime_ms)?;
    }
    Ok(header.files.len() + header.delete.len())
}

/// Write atomically, keeping the home's mtime.
fn write_file(path: &Path, bytes: &[u8], mtime_ms: i64) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path.parent().context("no parent folder")?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("no file name")?;
    let tmp = parent.join(format!(
        ".{name}.{}.tui-do-tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    if let Ok(ms) = u64::try_from(mtime_ms) {
        file.set_modified(UNIX_EPOCH + Duration::from_millis(ms))?;
    }
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// How many backups of folders that became copies are kept.
const BACKUPS_KEPT: usize = 2;

fn marked(core_dir: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(core_dir.join("replica.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["folder"].as_str().map(PathBuf::from))
}

/// Whether the folder at `root` may take copies: the default folder, or one
/// marked as a copy (`<core dir>/replica.json`) when the home moved away from
/// this core. A folder of a core's own is never overwritten otherwise.
pub fn accepts(core_dir: &Path, root: &Path, default: &Path) -> anyhow::Result<()> {
    if marked(core_dir).as_deref() == Some(root) {
        return Ok(());
    }
    if root == default {
        return mark(core_dir, root);
    }
    bail!(
        "this machine serves a workspace folder of its own ({}), so it doesn't keep a copy of the home's",
        root.display()
    )
}

/// Mark `root` as a copy, keeping what's in it first:
/// `<core dir>/backups/workspace-<time>` (the newest few are kept).
pub fn mark(core_dir: &Path, root: &Path) -> anyhow::Result<()> {
    if !manifest(root).files.is_empty() {
        let backups = core_dir.join("backups");
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let backup = backups.join(format!("workspace-{stamp}"));
        for area in AREAS {
            copy_dir(&root.join(area), &backup.join(area))?;
        }
        tracing::info!(backup = %backup.display(), "kept the folder's contents before it became a copy");
        let mut old: Vec<PathBuf> = std::fs::read_dir(&backups)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        old.sort();
        let excess = old.len().saturating_sub(BACKUPS_KEPT);
        for path in old.into_iter().take(excess) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    std::fs::write(
        core_dir.join("replica.json"),
        serde_json::json!({ "folder": root }).to_string() + "\n",
    )?;
    Ok(())
}

/// The folder isn't a copy any more (this core took the home).
pub fn unmark(core_dir: &Path) {
    let _ = std::fs::remove_file(core_dir.join("replica.json"));
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    std::fs::create_dir_all(to)?;
    for entry in entries.flatten() {
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The home a push comes from; a core only takes pushes from its own home.
pub struct Sender {
    pub id: String,
    pub generation: u64,
}

/// The home side: pushes its copy to devices, one push per device at a time.
#[derive(Default)]
pub struct Replicas {
    busy: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Replicas {
    fn lock_for(&self, device: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut busy = self.busy.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(busy.entry(device.to_string()).or_default())
    }

    /// Bring `device`'s copy up to date with the one at `root`. Returns how
    /// many files were sent or deleted.
    pub async fn push(
        &self,
        client: &reqwest::Client,
        device: &Device,
        root: &Path,
        from: &Sender,
    ) -> anyhow::Result<usize> {
        let lock = self.lock_for(&device.id);
        let _pushing = lock.lock().await;
        let res = client
            .get(format!("{}/v0/replica", device.address))
            .bearer_auth(&device.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", device.name))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("{}'s core is too old to keep a copy", device.name);
        }
        let theirs: Manifest = res.error_for_status()?.json().await?;
        let root = root.to_path_buf();
        let ours = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || manifest(&root)).await?
        };
        let (send, delete) = diff(&ours, &theirs);
        let changed = send.len() + delete.len();
        if changed == 0 {
            return Ok(0);
        }
        let mut batches: Vec<Vec<String>> = vec![Vec::new()];
        let mut size = 0;
        for path in send {
            let file_size = ours.files.get(&path).map_or(0, |s| s.0);
            if size + file_size > BATCH_BYTES
                && let Some(last) = batches.last()
                && !last.is_empty()
            {
                batches.push(Vec::new());
                size = 0;
            }
            size += file_size;
            if let Some(last) = batches.last_mut() {
                last.push(path);
            }
        }
        let mut delete = Some(delete);
        for paths in batches {
            // Deletions go with the first batch, ahead of its writes
            let deleting = delete.take().unwrap_or_default();
            let root = root.clone();
            let body =
                tokio::task::spawn_blocking(move || encode(&root, &paths, deleting)).await??;
            let res = client
                .post(format!("{}/v0/replica", device.address))
                .bearer_auth(&device.token)
                .query(&[
                    ("home", from.id.as_str()),
                    ("generation", &from.generation.to_string()),
                ])
                .body(body)
                .timeout(Duration::from_secs(600))
                .send()
                .await
                .with_context(|| format!("sending the workspace to {}", device.name))?;
            if !res.status().is_success() {
                let text = res.text().await.unwrap_or_default();
                bail!("{} didn't take the workspace: {text}", device.name);
            }
        }
        tracing::info!(device = %device.name, changed, "workspace copy updated");
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tui-do-replica-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
        std::fs::write(full, text).expect("write");
    }

    fn sync(home: &Path, replica: &Path) {
        let (send, delete) = diff(&manifest(home), &manifest(replica));
        let body = encode(home, &send, delete).expect("encodes");
        apply(replica, &body).expect("applies");
    }

    #[test]
    fn copy_should_match_the_home_after_a_sync() {
        let (home, replica) = (temp(), temp());
        write(&home, "tasks/a.md", "a");
        write(&home, "attachments/a/shot.png", "png");
        write(&home, "tasks/.DS_Store", "x");
        write(&replica, "tasks/gone.md", "old");
        write(&replica, "attachments/gone/x.png", "old");
        sync(&home, &replica);
        assert_eq!(manifest(&home), manifest(&replica));
        assert!(!replica.join("attachments/gone").exists());
        assert!(!replica.join("tasks/.DS_Store").exists());
        // Nothing left to send
        let (send, delete) = diff(&manifest(&home), &manifest(&replica));
        assert!(send.is_empty() && delete.is_empty());
    }

    #[test]
    fn changed_files_should_be_sent_again() {
        let (home, replica) = (temp(), temp());
        write(&home, "tasks/a.md", "a");
        sync(&home, &replica);
        write(&home, "tasks/a.md", "longer");
        let (send, _) = diff(&manifest(&home), &manifest(&replica));
        assert_eq!(send, vec!["tasks/a.md".to_string()]);
    }

    #[test]
    fn nothing_should_be_deleted_from_an_incomplete_or_empty_home() {
        let (home, replica) = (temp(), temp());
        write(&replica, "tasks/b.md", "b");
        assert!(diff(&manifest(&home), &manifest(&replica)).1.is_empty());
        write(&home, "tasks/a.md", "a");
        write(&home, "tasks/.c.md.icloud", "");
        let ours = manifest(&home);
        assert!(!ours.complete);
        assert!(diff(&ours, &manifest(&replica)).1.is_empty());
    }

    #[test]
    fn paths_outside_the_workspace_should_be_refused() {
        for bad in [
            "../x",
            "tasks/../../x",
            "tasks/.hidden",
            "other/x",
            "tasks",
            "tasks//x",
            "/etc/passwd",
        ] {
            assert!(!valid_path(bad), "{bad}");
        }
        assert!(valid_path("attachments/my-task/shot 1.png"));
        let replica = temp();
        let body = b"{\"files\":[],\"delete\":[\"../x\"]}\n";
        assert!(apply(&replica, body).is_err());
    }

    #[test]
    fn a_folder_of_its_own_should_not_take_copies_until_marked() {
        let (core, own, default) = (temp(), temp(), temp());
        write(&own, "tasks/mine.md", "mine");
        assert!(accepts(&core, &own, &default).is_err());
        // The default folder is always a copy
        accepts(&core, &default, &default).expect("default");
        mark(&core, &own).expect("marks");
        accepts(&core, &own, &default).expect("marked");
        let backups = std::fs::read_dir(core.join("backups"))
            .expect("backups")
            .count();
        assert_eq!(backups, 1);
        unmark(&core);
        assert!(accepts(&core, &own, &default).is_err());
    }

    #[test]
    fn a_rename_in_case_only_should_keep_the_file() {
        let (home, replica) = (temp(), temp());
        write(&replica, "tasks/Foo.md", "old");
        write(&home, "tasks/foo.md", "new");
        sync(&home, &replica);
        assert_eq!(
            std::fs::read_to_string(replica.join("tasks/foo.md")).expect("kept"),
            "new"
        );
    }
}
