//! The git steps of a handoff, run with the `git` command.
//!
//! Home side: [`snapshot`] a project without touching its checkout, [`bundle`]
//! only what the runner lacks, and [`land`] the work that comes back as a
//! branch. Runner side: [`receive`] a bundle into a per-project cache and a
//! worktree for the task, and [`collect`] the task's branch when it's done.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// Commits made by the handoff itself use the repo's identity, or this one
/// when the repo has none (e.g. a fresh runner).
const FALLBACK_NAME: &str = "TUI-DO";
const FALLBACK_EMAIL: &str = "tui-do@localhost";

fn git(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .with_context(|| format!("running git {}", args.first().unwrap_or(&"")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Identity env vars for a commit, if the repo doesn't configure one.
fn identity(dir: &Path) -> Vec<(&'static str, &'static str)> {
    if git(dir, &["config", "user.email"], &[]).is_ok_and(|e| !e.is_empty()) {
        return Vec::new();
    }
    vec![
        ("GIT_AUTHOR_NAME", FALLBACK_NAME),
        ("GIT_AUTHOR_EMAIL", FALLBACK_EMAIL),
        ("GIT_COMMITTER_NAME", FALLBACK_NAME),
        ("GIT_COMMITTER_EMAIL", FALLBACK_EMAIL),
    ]
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tui-do-{}-{name}", uuid::Uuid::new_v4().simple()))
}

/// A project's state at handoff: `commit` is HEAD, or a commit on top of it
/// holding uncommitted (and untracked, not ignored) changes (`dirty`).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub repo: PathBuf,
    pub base: String,
    pub commit: String,
    pub dirty: bool,
}

/// Whether `dir` is in a git repo, and how many files have uncommitted changes
/// (untracked ones included, ignored ones not).
pub fn status(dir: &Path) -> (bool, usize) {
    match git(dir, &["status", "--porcelain"], &[]) {
        Ok(out) => (true, out.lines().filter(|l| !l.is_empty()).count()),
        Err(_) => (false, 0),
    }
}

/// The repository a project directory belongs to, or an error if it isn't one.
pub fn repo_root(dir: &Path) -> anyhow::Result<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], &[])
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("{} isn't a git repository", dir.display()))
}

/// Snapshot a project for handoff. Without `include_uncommitted` only its
/// commits travel (HEAD).
pub fn snapshot(
    dir: &Path,
    handoff_ref: &str,
    include_uncommitted: bool,
) -> anyhow::Result<Snapshot> {
    let repo = repo_root(dir)?;
    let base =
        git(&repo, &["rev-parse", "HEAD"], &[]).context("the repository has no commits yet")?;
    if !include_uncommitted {
        git(&repo, &["update-ref", handoff_ref, &base], &[])?;
        return Ok(Snapshot {
            repo,
            commit: base.clone(),
            base,
            dirty: false,
        });
    }
    // A separate index, so the user's staging area is left alone
    let index = temp_path("index");
    let index_env = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index_env.as_str())];
    let tree = (|| {
        git(&repo, &["read-tree", "HEAD"], &env)?;
        git(&repo, &["add", "-A"], &env)?;
        git(&repo, &["write-tree"], &env)
    })();
    let _ = std::fs::remove_file(&index);
    let tree = tree?;
    let head_tree = git(&repo, &["rev-parse", "HEAD^{tree}"], &[])?;
    let (commit, dirty) = if tree == head_tree {
        (base.clone(), false)
    } else {
        let message = "tui-do: uncommitted work at handoff";
        let commit = git(
            &repo,
            &["commit-tree", &tree, "-p", &base, "-m", message],
            &identity(&repo),
        )?;
        (commit, true)
    };
    git(&repo, &["update-ref", handoff_ref, &commit], &[])?;
    Ok(Snapshot {
        repo,
        base,
        commit,
        dirty,
    })
}

/// A bundle of `reference` without the commits listed in `known` (those the
/// other side already has). `None` if there's nothing to send.
pub fn bundle(repo: &Path, reference: &str, known: &[String]) -> anyhow::Result<Option<Vec<u8>>> {
    let target = git(repo, &["rev-parse", reference], &[])?;
    let exclude: Vec<String> = known
        .iter()
        .filter(|c| git(repo, &["cat-file", "-e", &format!("{c}^{{commit}}")], &[]).is_ok())
        .map(|c| format!("^{c}"))
        .collect();
    // Already there: the target is one of the known commits or behind them
    for c in known {
        if git(repo, &["merge-base", "--is-ancestor", &target, c], &[]).is_ok() {
            return Ok(None);
        }
    }
    let file = temp_path("bundle");
    let file_arg = file.to_string_lossy().into_owned();
    let mut args = vec!["bundle", "create", file_arg.as_str(), reference];
    args.extend(exclude.iter().map(String::as_str));
    let created = git(repo, &args, &[]);
    let bytes = created.and_then(|_| std::fs::read(&file).map_err(Into::into));
    let _ = std::fs::remove_file(&file);
    bytes.map(Some)
}

/// Commits the runner's cache already has, so the home only sends the rest.
pub fn known_commits(cache: &Path) -> Vec<String> {
    if !cache.exists() {
        return Vec::new();
    }
    git(cache, &["for-each-ref", "--format=%(objectname)"], &[])
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Runner: take a bundle into the project's cache and check the task out in
/// its own worktree. An existing worktree (the task ran here before and wasn't
/// collected) is kept as is.
pub fn receive(
    cache: &Path,
    worktree: &Path,
    task_ref: &str,
    branch: &str,
    snapshot: &Snapshot,
    bundle: Option<&[u8]>,
) -> anyhow::Result<()> {
    if !cache.exists() {
        std::fs::create_dir_all(cache)?;
        git(cache, &["init", "--bare", "--quiet"], &[])?;
    }
    if let Some(bytes) = bundle {
        let file = temp_path("bundle");
        std::fs::write(&file, bytes)?;
        let file_arg = file.to_string_lossy().into_owned();
        let refspec = "+refs/tui-do/handoff/*:refs/tui-do/incoming/*";
        let fetched = git(cache, &["fetch", "--quiet", &file_arg, refspec], &[]);
        let _ = std::fs::remove_file(&file);
        fetched?;
    }
    git(
        cache,
        &["cat-file", "-e", &format!("{}^{{commit}}", snapshot.commit)],
        &[],
    )
    .context("the runner doesn't have the snapshot")?;
    git(cache, &["update-ref", task_ref, &snapshot.commit], &[])?;
    if worktree.exists() {
        return Ok(());
    }
    let path = worktree.to_string_lossy().into_owned();
    git(
        cache,
        &[
            "worktree",
            "add",
            "--quiet",
            "-B",
            branch,
            &path,
            &snapshot.commit,
        ],
        &[],
    )?;
    if snapshot.dirty {
        // Uncommitted work arrives as uncommitted work
        git(worktree, &["reset", "--quiet", "HEAD^"], &[])?;
    }
    Ok(())
}

/// Runner: commit whatever is left in the task's worktree and bundle its
/// branch since the snapshot. `None` if nothing changed. The worktree is
/// removed afterwards.
pub fn collect(
    cache: &Path,
    worktree: &Path,
    task_ref: &str,
    branch: &str,
    snapshot: &Snapshot,
    title: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !worktree.exists() {
        bail!("the task's working folder is gone ({})", worktree.display());
    }
    git(worktree, &["add", "-A"], &[])?;
    if git(worktree, &["diff", "--cached", "--quiet"], &[]).is_err() {
        let message = format!("tui-do: work in progress when \"{title}\" was completed");
        git(
            worktree,
            &["commit", "--quiet", "--no-verify", "-m", &message],
            &identity(worktree),
        )?;
    }
    // What the home already has: the snapshot and, for a dirty one, its parent
    let mut known = vec![snapshot.commit.clone()];
    if snapshot.dirty {
        known.push(snapshot.base.clone());
    }
    let bundle = bundle(cache, &format!("refs/heads/{branch}"), &known)?;
    let path = worktree.to_string_lossy().into_owned();
    git(cache, &["worktree", "remove", "--force", &path], &[])?;
    let _ = git(cache, &["update-ref", "-d", task_ref], &[]);
    Ok(bundle)
}

/// Home: fetch the returned work into the project as `branch`. Returns the
/// branch's new head.
pub fn land(repo: &Path, branch: &str, bundle: &[u8]) -> anyhow::Result<String> {
    let file = temp_path("bundle");
    std::fs::write(&file, bundle)?;
    let file_arg = file.to_string_lossy().into_owned();
    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    let fetched = git(repo, &["fetch", "--quiet", &file_arg, &refspec], &[]);
    let _ = std::fs::remove_file(&file);
    fetched?;
    git(repo, &["rev-parse", &format!("refs/heads/{branch}")], &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_commit(dir: &Path) {
        std::fs::create_dir_all(dir).expect("dir");
        git(dir, &["init", "--quiet", "-b", "main"], &[]).expect("init");
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        git(dir, &["add", "a.txt"], &[]).expect("add");
        git(dir, &["commit", "--quiet", "-m", "first"], &identity(dir)).expect("commit");
    }

    /// The whole round trip: dirty snapshot → runner worktree (still dirty) →
    /// work there → collect → lands as a branch at home; home's checkout untouched.
    #[test]
    fn handoff_should_round_trip_through_a_branch() {
        let root = temp_path("roundtrip");
        let home = root.join("home");
        repo_with_commit(&home);
        std::fs::write(home.join("a.txt"), "one\nuncommitted\n").expect("edit");
        std::fs::write(home.join("new.txt"), "untracked\n").expect("new");

        let snap = snapshot(&home, "refs/tui-do/handoff/t", true).expect("snapshot");
        assert!(snap.dirty);
        // Home's checkout is untouched: same HEAD, changes still uncommitted
        assert_eq!(
            git(&home, &["rev-parse", "HEAD"], &[]).expect("head"),
            snap.base
        );
        assert!(
            git(&home, &["status", "--porcelain"], &[])
                .expect("status")
                .contains("new.txt")
        );

        let cache = root.join("runner/repo.git");
        let worktree = root.join("runner/t");
        let known = known_commits(&cache);
        let bytes = bundle(&snap.repo, "refs/tui-do/handoff/t", &known).expect("bundle");
        receive(
            &cache,
            &worktree,
            "refs/tui-do/tasks/t",
            "tui-do/26-1",
            &snap,
            bytes.as_deref(),
        )
        .expect("receive");
        // The uncommitted work is uncommitted on the runner too
        assert!(
            git(&worktree, &["status", "--porcelain"], &[])
                .expect("status")
                .contains("new.txt")
        );

        std::fs::write(worktree.join("b.txt"), "done on the runner\n").expect("work");
        let back = collect(
            &cache,
            &worktree,
            "refs/tui-do/tasks/t",
            "tui-do/26-1",
            &snap,
            "Task",
        )
        .expect("collect")
        .expect("something to bring back");
        let head = land(&home, "tui-do/26-1", &back).expect("land");
        let files = git(&home, &["ls-tree", "--name-only", &head], &[]).expect("ls-tree");
        assert!(files.contains("b.txt") && files.contains("new.txt"));
        // Still on main with the user's own uncommitted changes
        assert_eq!(
            git(&home, &["rev-parse", "--abbrev-ref", "HEAD"], &[]).expect("branch"),
            "main"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn committed_only_should_leave_uncommitted_work_behind() {
        let root = temp_path("committed");
        let home = root.join("home");
        repo_with_commit(&home);
        std::fs::write(home.join("draft.txt"), "mine\n").expect("draft");
        let snap = snapshot(&home, "refs/tui-do/handoff/t", false).expect("snapshot");
        assert!(!snap.dirty && snap.commit == snap.base);
        assert_eq!(status(&home), (true, 1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn second_handoff_should_send_only_new_commits() {
        let root = temp_path("delta");
        let home = root.join("home");
        repo_with_commit(&home);
        let snap = snapshot(&home, "refs/tui-do/handoff/t", true).expect("snapshot");
        assert!(!snap.dirty);
        let full = bundle(&snap.repo, "refs/tui-do/handoff/t", &[]).expect("bundle");
        assert!(full.is_some());
        // The runner has the commit already: nothing to send
        let none = bundle(
            &snap.repo,
            "refs/tui-do/handoff/t",
            std::slice::from_ref(&snap.commit),
        )
        .expect("bundle");
        assert!(none.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
