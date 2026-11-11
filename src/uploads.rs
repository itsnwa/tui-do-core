//! Files sent to a session from a client, such as a photo or screenshot from
//! the phone's compose sheet. They're saved on the session's machine, in
//! `uploads/<session>/`, so the agent can read them at the path the client
//! pastes in. A session's files go when it's removed, and any file goes after
//! a week.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;

/// How long an upload is kept if its session isn't removed.
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A file name that's safe to save and to paste: the last path component,
/// with spaces and unusual characters as `-` and no leading dot.
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // The end of a long name, which keeps its extension
    let skip = cleaned.chars().count().saturating_sub(100);
    let cleaned: String = cleaned.chars().skip(skip).collect();
    match cleaned.trim_start_matches('.') {
        "" => "file".into(),
        name => name.into(),
    }
}

/// Save a file for a session and return where it is. A name that's taken
/// gets a number: `shot.png`, `shot-2.png`, ….
pub fn save(dir: &Path, session: &str, name: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let folder = dir.join(session);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let name = clean_name(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name.as_str(), None),
    };
    for n in 1.. {
        let candidate = match (n, ext) {
            (1, _) => name.clone(),
            (_, Some(ext)) => format!("{stem}-{n}.{ext}"),
            (_, None) => format!("{stem}-{n}"),
        };
        let path = folder.join(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| format!("creating {}", path.display())),
        }
    }
    unreachable!()
}

/// Delete a session's uploads.
pub fn remove(dir: &Path, session: &str) {
    let _ = std::fs::remove_dir_all(dir.join(session));
}

/// Delete uploads older than `keep`, and folders left empty.
pub fn prune(dir: &Path, keep: Duration) {
    let Ok(sessions) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for folder in sessions.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        for file in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let old = file
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age >= keep));
            if old {
                let _ = std::fs::remove_file(file.path());
            }
        }
        // Only removed when empty
        let _ = std::fs::remove_dir(&folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uploads-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn names_should_be_safe_to_save_and_paste() {
        assert_eq!(
            clean_name("Photo 2026-11-11 10.22.33.jpg"),
            "Photo-2026-11-11-10.22.33.jpg"
        );
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name(".hidden"), "hidden");
        assert_eq!(clean_name(""), "file");
        assert_eq!(clean_name("a/.."), "file");
    }

    #[test]
    fn a_taken_name_should_get_a_number() {
        let dir = temp();
        let first = save(&dir, "s", "shot.png", b"1").expect("saves");
        let second = save(&dir, "s", "shot.png", b"2").expect("saves");
        assert_eq!(first, dir.join("s/shot.png"));
        assert_eq!(second, dir.join("s/shot-2.png"));
        assert_eq!(std::fs::read(&second).expect("reads"), b"2");
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[test]
    fn removing_and_pruning_should_delete_uploads() {
        let dir = temp();
        save(&dir, "a", "x.png", b"x").expect("saves");
        save(&dir, "b", "y.png", b"y").expect("saves");
        remove(&dir, "a");
        assert!(!dir.join("a").exists());
        prune(&dir, KEEP);
        assert!(dir.join("b/y.png").exists());
        prune(&dir, Duration::ZERO);
        assert!(!dir.join("b").exists());
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }
}
