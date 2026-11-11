//! Running a task on another machine with the project's code.
//!
//! The projects' folders are on one machine, the code machine (the home,
//! unless the home moved away from it). Starting a task on another fleet
//! device: the code machine snapshots the project's git repo (unpushed
//! commits, plus uncommitted changes when asked; the checkout untouched) and
//! bundles only what the device lacks, and the device checks it out in a
//! worktree of its own (`~/.tui-do/work/<project>/<task>`) on branch
//! `tui-do/<task id>`. Completing the task brings the work back: the device
//! commits what's left and bundles the branch, and the code machine fetches
//! it into the project as that branch. The home core runs all of it; when
//! the code machine is another core, it asks that core for the snapshot and
//! to land the branch.
//!
//! A project with a git remote (its GitHub URL) can start without the code
//! machine: when it has no folder, the code machine isn't reachable, or it's
//! asked to. The device fetches the remote's default branch instead, and
//! completing the task pushes the branch there and opens a pull request.

pub mod git;
pub mod github;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::fleet::{Device, Fleet};
use crate::session::{Event, expand_home};
use crate::workspace::projects::UpdateProject;
use crate::workspace::shared::SharedWorkspace;
use crate::workspace::tasks::UpdateTask;
use crate::workspace::{Workspace, slug};

/// What the home core remembers about a task it handed off.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub device: String,
    pub project: String,
    pub repo: PathBuf,
    pub branch: String,
    pub commit: String,
    pub base: String,
    pub dirty: bool,
    pub title: String,
    /// The task's working folder on the device.
    #[serde(default)]
    pub path: String,
    /// The machine `repo` is on; empty for the home that wrote the record.
    #[serde(default)]
    pub source: String,
    /// The git remote the task started from instead of the code machine; its
    /// work goes back there as a pull request.
    #[serde(default)]
    pub remote: String,
    /// The remote's branch it started from, the pull request's base.
    #[serde(default)]
    pub base_branch: String,
}

/// A snapshot made on the code machine, as it answers the home core.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSnapshot {
    pub repo: PathBuf,
    pub commit: String,
    pub base: String,
    pub dirty: bool,
    /// The repo's `origin`, if it has one.
    #[serde(default)]
    pub remote: String,
}

/// What the home core asks a device to fetch from a project's remote.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchRequest {
    pub remote: String,
    pub branch: String,
}

/// What a device fetched: the task's folder, and the commit and branch it
/// started from.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fetched {
    pub path: String,
    pub commit: String,
    pub base_branch: String,
}

/// What the home core asks the code machine to snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequest {
    pub directory: String,
    #[serde(default)]
    pub known: Vec<String>,
    #[serde(default)]
    pub include_uncommitted: bool,
}

/// Where the code is, for a handoff: this core, or another fleet device.
pub enum CodeMachine {
    Here(String),
    Device(Device),
}

impl CodeMachine {
    pub fn id(&self) -> &str {
        match self {
            CodeMachine::Here(id) => id,
            CodeMachine::Device(device) => &device.id,
        }
    }
}

/// The snapshot as passed to a device (query parameters).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotParams {
    pub branch: String,
    pub commit: String,
    pub base: String,
    pub dirty: bool,
    #[serde(default)]
    pub title: String,
}

impl SnapshotParams {
    fn snapshot(&self, repo: PathBuf) -> git::Snapshot {
        git::Snapshot {
            repo,
            base: self.base.clone(),
            commit: self.commit.clone(),
            dirty: self.dirty,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let hex = |s: &str| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit());
        if !hex(&self.commit) || !hex(&self.base) {
            bail!("invalid commit id");
        }
        valid_branch(&self.branch)
    }
}

/// A remote URL that's safe to hand to git (not an option, one line).
fn valid_remote(remote: &str) -> anyhow::Result<()> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains(char::is_whitespace) {
        bail!("invalid git remote {remote:?}");
    }
    Ok(())
}

fn valid_branch(branch: &str) -> anyhow::Result<()> {
    let ok = branch.starts_with("tui-do/")
        && !branch.contains("..")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        bail!("invalid branch name {branch:?}")
    }
}

/// The task that a `complete_task` answer completed; `None` if it refused.
pub fn completed_task(answer: &serde_json::Value) -> Option<&str> {
    let completed = matches!(
        answer["status"].as_str(),
        Some("completed" | "recurring_advanced")
    );
    answer["task"]["slug"].as_str().filter(|_| completed)
}

/// The "## Completion Summary" at the end of a plan, if it has one.
fn completion_summary(plan: &str) -> Option<String> {
    let (_, summary) = plan.rsplit_once("## Completion Summary")?;
    Some(summary.trim().to_string()).filter(|s| !s.is_empty())
}

/// The response header that carries a code machine's `RemoteSnapshot`.
pub const SNAPSHOT_HEADER: &str = "x-tui-do-snapshot";

pub struct Handoffs {
    fleet: Arc<Fleet>,
    workspace: Arc<SharedWorkspace>,
    /// This core as a device (its loopback address and own token), for tasks
    /// that run on the home itself.
    own: Device,
    events: broadcast::Sender<Event>,
    /// Home: one record per handed-off task.
    records: PathBuf,
    /// Device: where tasks' working folders live.
    work: PathBuf,
}

impl Handoffs {
    pub fn new(
        fleet: Arc<Fleet>,
        workspace: Arc<SharedWorkspace>,
        own: Device,
        events: broadcast::Sender<Event>,
        core_dir: &Path,
    ) -> Self {
        let work = core_dir
            .parent()
            .map_or_else(|| core_dir.join("work"), |p| p.join("work"));
        Self {
            fleet,
            workspace,
            own,
            events,
            records: core_dir.join("handoffs"),
            work,
        }
    }

    fn record_path(&self, task: &str) -> PathBuf {
        self.records.join(format!("{task}.json"))
    }

    pub fn record(&self, task: &str) -> Option<Record> {
        serde_json::from_str(&std::fs::read_to_string(self.record_path(task)).ok()?).ok()
    }

    /// Every record, by task, to hand to a new home.
    pub fn records(&self) -> Vec<(String, Record)> {
        let Ok(entries) = std::fs::read_dir(&self.records) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let task = e.file_name().to_str()?.strip_suffix(".json")?.to_string();
                Some((task.clone(), self.record(&task)?))
            })
            .collect()
    }

    /// Replace every record (taking over as home, or giving the role away).
    pub fn replace_records(&self, records: &[(String, Record)]) -> anyhow::Result<()> {
        if self.records.exists() {
            std::fs::remove_dir_all(&self.records)?;
        }
        std::fs::create_dir_all(&self.records)?;
        for (task, record) in records {
            slug::validate(task)?;
            std::fs::write(
                self.record_path(task),
                serde_json::to_string_pretty(record)? + "\n",
            )?;
        }
        Ok(())
    }

    fn emit(&self, task: &str, status: &str, branch: Option<&str>, message: Option<String>) {
        let _ = self.events.send(Event::Handoff {
            task: task.to_string(),
            status: status.to_string(),
            branch: branch.map(str::to_string),
            message,
        });
    }

    // --- device side ---

    pub fn cache(&self, project: &str) -> PathBuf {
        self.work.join(project).join("repo.git")
    }

    pub fn worktree(&self, project: &str, task: &str) -> PathBuf {
        self.work.join(project).join(task)
    }

    pub fn known_commits(&self, project: &str) -> anyhow::Result<Vec<String>> {
        slug::validate(project)?;
        Ok(git::known_commits(&self.cache(project)))
    }

    pub fn receive(
        &self,
        project: &str,
        task: &str,
        params: &SnapshotParams,
        bundle: &[u8],
    ) -> anyhow::Result<PathBuf> {
        slug::validate(project)?;
        slug::validate(task)?;
        params.validate()?;
        let worktree = self.worktree(project, task);
        let snapshot = params.snapshot(PathBuf::new());
        let bundle = (!bundle.is_empty()).then_some(bundle);
        git::receive(
            &self.cache(project),
            &worktree,
            &format!("refs/tui-do/tasks/{task}"),
            &params.branch,
            &snapshot,
            bundle,
        )?;
        Ok(worktree)
    }

    /// Fetch a project from its remote and check the task out in its own
    /// worktree, as [`Self::receive`] does with a bundle.
    pub fn fetch(&self, project: &str, task: &str, req: &FetchRequest) -> anyhow::Result<Fetched> {
        slug::validate(project)?;
        slug::validate(task)?;
        valid_remote(&req.remote)?;
        valid_branch(&req.branch)?;
        let cache = self.cache(project);
        let worktree = self.worktree(project, task);
        let (commit, base_branch) =
            git::fetch(&cache, &req.remote, &format!("refs/tui-do/incoming/{task}"))?;
        let snapshot = git::Snapshot {
            repo: PathBuf::new(),
            base: commit.clone(),
            commit: commit.clone(),
            dirty: false,
        };
        git::receive(
            &cache,
            &worktree,
            &format!("refs/tui-do/tasks/{task}"),
            &req.branch,
            &snapshot,
            None,
        )?;
        Ok(Fetched {
            path: worktree.to_string_lossy().into_owned(),
            commit,
            base_branch,
        })
    }

    /// Push a task's branch to the remote it was fetched from. Whether there
    /// was anything to push.
    pub fn publish(
        &self,
        project: &str,
        task: &str,
        params: &SnapshotParams,
        remote: &str,
    ) -> anyhow::Result<bool> {
        slug::validate(project)?;
        slug::validate(task)?;
        params.validate()?;
        valid_remote(remote)?;
        git::publish(
            &self.cache(project),
            &self.worktree(project, task),
            &[
                &format!("refs/tui-do/tasks/{task}"),
                &format!("refs/tui-do/incoming/{task}"),
            ],
            &params.branch,
            &params.snapshot(PathBuf::new()),
            &params.title,
            remote,
        )
    }

    /// Code machine: snapshot a project for the home core, and bundle what
    /// the device it's going to lacks.
    pub fn snapshot_here(
        project: &str,
        task: &str,
        req: &SnapshotRequest,
    ) -> anyhow::Result<(RemoteSnapshot, Option<Vec<u8>>)> {
        slug::validate(project)?;
        slug::validate(task)?;
        let handoff_ref = format!("refs/tui-do/handoff/{task}");
        let snapshot = git::snapshot(
            &expand_home(&req.directory),
            &handoff_ref,
            req.include_uncommitted,
        )?;
        let bundle = git::bundle(&snapshot.repo, &handoff_ref, &req.known)?;
        Ok((
            RemoteSnapshot {
                remote: git::origin(&snapshot.repo).unwrap_or_default(),
                repo: snapshot.repo,
                commit: snapshot.commit,
                base: snapshot.base,
                dirty: snapshot.dirty,
            },
            bundle,
        ))
    }

    /// Code machine: fetch returned work into the project as `branch`.
    pub fn land_here(task: &str, repo: &Path, branch: &str, bundle: &[u8]) -> anyhow::Result<()> {
        slug::validate(task)?;
        valid_branch(branch)?;
        git::land(repo, branch, bundle)?;
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["update-ref", "-d", &format!("refs/tui-do/handoff/{task}")])
            .status();
        Ok(())
    }

    pub fn collect(
        &self,
        project: &str,
        task: &str,
        params: &SnapshotParams,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        slug::validate(project)?;
        slug::validate(task)?;
        params.validate()?;
        let snapshot = params.snapshot(PathBuf::new());
        git::collect(
            &self.cache(project),
            &self.worktree(project, task),
            &[
                &format!("refs/tui-do/tasks/{task}"),
                &format!("refs/tui-do/incoming/{task}"),
            ],
            &params.branch,
            &snapshot,
            &params.title,
        )
    }

    // --- home side ---

    /// Hand a task's project from the code machine to `device` and return
    /// its working folder there (and the branch). A task that's already there
    /// keeps its copy (and the work in it). On the code machine itself the
    /// task works in the project's folder, with no branch. The project comes
    /// from its git remote instead when `from_remote` asks for it, when it
    /// has no folder, or when the code machine can't be reached.
    pub async fn start(
        &self,
        workspace: Arc<Workspace>,
        task_slug: &str,
        device: &Device,
        code: &CodeMachine,
        include_uncommitted: bool,
        from_remote: bool,
    ) -> anyhow::Result<(String, String)> {
        let ws = Arc::clone(&workspace);
        let slug_owned = task_slug.to_string();
        let (task, project) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let task = ws.get_task(&slug_owned)?;
            if task.task.project.is_empty() {
                bail!("the task has no project, so there's no code to take along");
            }
            let project = ws.get_project(&task.task.project)?;
            Ok((task, project))
        })
        .await??;
        if let Some(record) = self.record(task_slug)
            && record.device == device.id
            && !record.path.is_empty()
        {
            return Ok((record.path, record.branch));
        }
        if project.directory.is_empty() && project.remote.is_empty() {
            bail!("the project {} has no folder or git remote", project.title);
        }
        if from_remote && project.remote.is_empty() {
            bail!("the project {} has no git remote", project.title);
        }
        let from_remote = !project.remote.is_empty()
            && (from_remote || project.directory.is_empty() || !self.reachable(code).await);
        if !from_remote && device.id == code.id() {
            return Ok((project.directory, String::new()));
        }
        let branch = format!(
            "tui-do/{}",
            if task.task.id.is_empty() {
                task_slug
            } else {
                &task.task.id
            }
        );
        valid_branch(&branch)?;
        if from_remote {
            self.emit(
                task_slug,
                "starting",
                Some(&branch),
                Some(format!("Fetching {} on {}", project.title, device.name)),
            );
            let (path, record) = self
                .start_from_remote(&project.slug, &project.remote, task_slug, &branch, device)
                .await?;
            self.save_record(
                task_slug,
                &Record {
                    title: task.task.title,
                    source: code.id().to_string(),
                    ..record
                },
            )?;
            self.emit(task_slug, "started", Some(&branch), None);
            return Ok((path, branch));
        }
        self.emit(
            task_slug,
            "starting",
            Some(&branch),
            Some(format!("Copying {} to {}", project.title, device.name)),
        );

        let client = self.fleet.client();
        let known: Vec<String> = client
            .get(format!(
                "{}/v0/handoff/{}/commits",
                device.address, project.slug
            ))
            .bearer_auth(&device.token)
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", device.name))?
            .error_for_status()?
            .json()
            .await?;

        let req = SnapshotRequest {
            directory: project.directory.clone(),
            known,
            include_uncommitted,
        };
        let (snapshot, bundle) = match code {
            CodeMachine::Here(_) => {
                let (project, task) = (project.slug.clone(), task_slug.to_string());
                tokio::task::spawn_blocking(move || Self::snapshot_here(&project, &task, &req))
                    .await??
            }
            CodeMachine::Device(source) => {
                self.snapshot_on(source, &project.slug, task_slug, &req)
                    .await?
            }
        };
        if project.remote.is_empty() && !snapshot.remote.is_empty() {
            // Learn the project's remote, for when the code machine is away
            let (slug, remote) = (project.slug.clone(), snapshot.remote.clone());
            let _ = tokio::task::spawn_blocking(move || {
                workspace.update_project(
                    &slug,
                    UpdateProject {
                        remote: Some(remote),
                        ..UpdateProject::default()
                    },
                )
            })
            .await;
        }

        let params = SnapshotParams {
            branch: branch.clone(),
            commit: snapshot.commit.clone(),
            base: snapshot.base.clone(),
            dirty: snapshot.dirty,
            title: task.task.title.clone(),
        };
        let res = client
            .post(format!(
                "{}/v0/handoff/{}/{task_slug}",
                device.address, project.slug
            ))
            .bearer_auth(&device.token)
            .query(&params)
            .body(bundle.unwrap_or_default())
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .with_context(|| format!("sending the project to {}", device.name))?;
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("{} couldn't take the project: {body}", device.name);
        }
        #[derive(Deserialize)]
        struct Received {
            path: String,
        }
        let received: Received = res.json().await?;

        self.save_record(
            task_slug,
            &Record {
                device: device.id.clone(),
                project: project.slug.clone(),
                repo: snapshot.repo.clone(),
                branch: branch.clone(),
                commit: snapshot.commit,
                base: snapshot.base,
                dirty: snapshot.dirty,
                title: task.task.title,
                path: received.path.clone(),
                source: code.id().to_string(),
                remote: String::new(),
                base_branch: String::new(),
            },
        )?;
        self.emit(task_slug, "started", Some(&branch), None);
        Ok((received.path, branch))
    }

    fn save_record(&self, task_slug: &str, record: &Record) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.records)?;
        std::fs::write(
            self.record_path(task_slug),
            serde_json::to_string_pretty(record)? + "\n",
        )?;
        Ok(())
    }

    /// Whether the code machine answers.
    async fn reachable(&self, code: &CodeMachine) -> bool {
        match code {
            CodeMachine::Here(_) => true,
            CodeMachine::Device(device) => self
                .fleet
                .identify(&device.address, &device.token)
                .await
                .is_ok(),
        }
    }

    /// Have `device` fetch the project from its remote. Its folder there, and
    /// the record of the handoff (without the task's title and source).
    async fn start_from_remote(
        &self,
        project: &str,
        remote: &str,
        task_slug: &str,
        branch: &str,
        device: &Device,
    ) -> anyhow::Result<(String, Record)> {
        let res = self
            .fleet
            .client()
            .post(format!(
                "{}/v0/handoff/{project}/{task_slug}/fetch",
                device.address
            ))
            .bearer_auth(&device.token)
            .json(&FetchRequest {
                remote: remote.to_string(),
                branch: branch.to_string(),
            })
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", device.name))?;
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("{} couldn't fetch the project: {body}", device.name);
        }
        let fetched: Fetched = res.json().await?;
        let record = Record {
            device: device.id.clone(),
            project: project.to_string(),
            repo: PathBuf::new(),
            branch: branch.to_string(),
            commit: fetched.commit.clone(),
            base: fetched.commit,
            dirty: false,
            title: String::new(),
            path: fetched.path.clone(),
            source: String::new(),
            remote: remote.to_string(),
            base_branch: fetched.base_branch,
        };
        Ok((fetched.path, record))
    }

    /// A task was completed: if it ran on another machine, bring its work
    /// back, in the background.
    pub fn task_completed(self: &Arc<Self>, task_slug: &str) {
        if self.record(task_slug).is_none() {
            return;
        }
        let (handoffs, task) = (Arc::clone(self), task_slug.to_string());
        tokio::spawn(async move { handoffs.bring_back(&task).await });
    }

    /// The task was completed: bring its work back from the device it ran on,
    /// to the code machine or, if it started from the remote, as a pull
    /// request there.
    async fn bring_back(&self, task_slug: &str) {
        let Some(record) = self.record(task_slug) else {
            return;
        };
        let returned = if record.remote.is_empty() {
            self.collect_from_device(task_slug, &record)
                .await
                .map(|landed| {
                    if landed {
                        format!("The work is on branch {}", record.branch)
                    } else {
                        "Nothing changed on the other machine".to_string()
                    }
                })
        } else {
            self.publish_from_device(task_slug, &record).await
        };
        match returned {
            Ok(message) => {
                let _ = std::fs::remove_file(self.record_path(task_slug));
                tracing::info!(task = task_slug, branch = %record.branch, %message, "handoff returned");
                self.emit(task_slug, "returned", Some(&record.branch), Some(message));
            }
            Err(err) => {
                tracing::warn!(task = task_slug, error = %format!("{err:#}"), "bringing work back failed");
                self.emit(
                    task_slug,
                    "failed",
                    Some(&record.branch),
                    Some(format!("{err:#}")),
                );
            }
        }
    }

    /// The device the record's task ran on.
    fn record_device(&self, record: &Record) -> anyhow::Result<Device> {
        if record.device == self.own.id {
            Ok(self.own.clone())
        } else {
            self.fleet
                .device(&record.device)
                .context("the machine is no longer in the fleet")
        }
    }

    /// Have the device push the task's branch to the remote, and open a pull
    /// request for it. What happened, for the app.
    async fn publish_from_device(
        &self,
        task_slug: &str,
        record: &Record,
    ) -> anyhow::Result<String> {
        let device = self.record_device(record)?;
        let params = SnapshotParams {
            branch: record.branch.clone(),
            commit: record.commit.clone(),
            base: record.base.clone(),
            dirty: record.dirty,
            title: record.title.clone(),
        };
        let res = self
            .fleet
            .client()
            .post(format!(
                "{}/v0/handoff/{}/{task_slug}/publish",
                device.address, record.project
            ))
            .bearer_auth(&device.token)
            .query(&params)
            .json(&serde_json::json!({ "remote": record.remote }))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", device.name))?;
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("{} couldn't push the work: {body}", device.name);
        }
        #[derive(Deserialize)]
        struct Published {
            pushed: bool,
        }
        let published: Published = res.json().await?;
        if !published.pushed {
            return Ok("Nothing changed on the other machine".to_string());
        }
        let Some(repo) = git::github_repo(&record.remote) else {
            return Ok(format!(
                "The work is on branch {} at {}",
                record.branch, record.remote
            ));
        };
        Ok(match self.open_pr(task_slug, record, repo).await {
            Ok(url) => format!("Opened a pull request: {url}"),
            Err(err) => format!(
                "The work is on branch {}, but the pull request couldn't be opened: {err:#}",
                record.branch
            ),
        })
    }

    /// Open a pull request for a task's pushed branch, described by its
    /// completion summary, and keep its URL on the task.
    async fn open_pr(
        &self,
        task_slug: &str,
        record: &Record,
        repo: String,
    ) -> anyhow::Result<String> {
        let ws = self
            .workspace
            .current()
            .context("the workspace isn't configured")?;
        let (task_slug, branch, base) = (
            task_slug.to_string(),
            record.branch.clone(),
            record.base_branch.clone(),
        );
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let task = ws.get_task(&task_slug)?;
            let summary = task
                .task
                .plan
                .as_deref()
                .and_then(|plan| ws.get_plan(plan).ok()?.content)
                .and_then(|content| completion_summary(&content));
            let id = if task.task.id.is_empty() {
                &task_slug
            } else {
                &task.task.id
            };
            let body = format!(
                "{}\n\n---\nTUI-DO task {id}",
                summary.unwrap_or_else(|| task.description.trim().to_string())
            );
            let url =
                github::create_pr(&repo, &branch, &base, &task.task.title, body.trim_start())?;
            ws.update_task(
                &task_slug,
                UpdateTask {
                    pr: Some(Some(url.clone())),
                    ..UpdateTask::default()
                },
            )?;
            Ok(url)
        })
        .await?
    }

    async fn collect_from_device(&self, task_slug: &str, record: &Record) -> anyhow::Result<bool> {
        // Where the work lands: here, unless the project is on another machine
        let source = match self.fleet.device(&record.source) {
            Some(source) => Some(source),
            None if record.source.is_empty() || record.source == self.own.id => None,
            None => bail!("the machine with the project is no longer in the fleet"),
        };
        let device = self.record_device(record)?;
        let params = SnapshotParams {
            branch: record.branch.clone(),
            commit: record.commit.clone(),
            base: record.base.clone(),
            dirty: record.dirty,
            title: record.title.clone(),
        };
        let res = self
            .fleet
            .client()
            .post(format!(
                "{}/v0/handoff/{}/{task_slug}/collect",
                device.address, record.project
            ))
            .bearer_auth(&device.token)
            .query(&params)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", device.name))?;
        if res.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(false);
        }
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("{} couldn't hand the work back: {body}", device.name);
        }
        let bundle = res.bytes().await?.to_vec();
        match source {
            None => {
                let (repo, branch, task) = (
                    record.repo.clone(),
                    record.branch.clone(),
                    task_slug.to_string(),
                );
                tokio::task::spawn_blocking(move || {
                    Self::land_here(&task, &repo, &branch, &bundle)
                })
                .await??;
            }
            Some(source) => {
                let res = self
                    .fleet
                    .client()
                    .post(format!(
                        "{}/v0/handoff/{}/{task_slug}/land",
                        source.address, record.project
                    ))
                    .bearer_auth(&source.token)
                    .query(&[
                        ("repo", record.repo.to_string_lossy().as_ref()),
                        ("branch", &record.branch),
                    ])
                    .body(bundle)
                    .timeout(std::time::Duration::from_secs(600))
                    .send()
                    .await
                    .with_context(|| format!("{} isn't reachable", source.name))?;
                if !res.status().is_success() {
                    let body = res.text().await.unwrap_or_default();
                    bail!("{} couldn't take the work back: {body}", source.name);
                }
            }
        }
        Ok(true)
    }

    /// Ask the code machine for a snapshot and the bundle for the device.
    async fn snapshot_on(
        &self,
        source: &Device,
        project: &str,
        task: &str,
        req: &SnapshotRequest,
    ) -> anyhow::Result<(RemoteSnapshot, Option<Vec<u8>>)> {
        let res = self
            .fleet
            .client()
            .post(format!(
                "{}/v0/handoff/{project}/{task}/snapshot",
                source.address
            ))
            .bearer_auth(&source.token)
            .json(req)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .with_context(|| format!("{} isn't reachable", source.name))?;
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("{} couldn't snapshot the project: {body}", source.name);
        }
        let snapshot: RemoteSnapshot = res
            .headers()
            .get(SNAPSHOT_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str(v).ok())
            .context("the snapshot didn't say what it is")?;
        let bundle = res.bytes().await?.to_vec();
        Ok((snapshot, (!bundle.is_empty()).then_some(bundle)))
    }
}
