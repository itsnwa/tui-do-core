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

pub mod git;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::fleet::{Device, Fleet};
use crate::session::{Event, expand_home};
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
}

/// A snapshot made on the code machine, as it answers the home core.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSnapshot {
    pub repo: PathBuf,
    pub commit: String,
    pub base: String,
    pub dirty: bool,
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

/// The response header that carries a code machine's `RemoteSnapshot`.
pub const SNAPSHOT_HEADER: &str = "x-tui-do-snapshot";

pub struct Handoffs {
    fleet: Arc<Fleet>,
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
        own: Device,
        events: broadcast::Sender<Event>,
        core_dir: &Path,
    ) -> Self {
        let work = core_dir
            .parent()
            .map_or_else(|| core_dir.join("work"), |p| p.join("work"));
        Self {
            fleet,
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
    /// task works in the project's folder, with no branch.
    pub async fn start(
        &self,
        workspace: Arc<Workspace>,
        task_slug: &str,
        device: &Device,
        code: &CodeMachine,
        include_uncommitted: bool,
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
        if project.directory.is_empty() {
            bail!("the project {} has no folder", project.title);
        }
        if device.id == code.id() {
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

        let record = Record {
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
        };
        std::fs::create_dir_all(&self.records)?;
        std::fs::write(
            self.record_path(task_slug),
            serde_json::to_string_pretty(&record)? + "\n",
        )?;
        self.emit(task_slug, "started", Some(&branch), None);
        Ok((received.path, branch))
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

    /// The task was completed: bring its work back from the device it ran on.
    async fn bring_back(&self, task_slug: &str) {
        let Some(record) = self.record(task_slug) else {
            return;
        };
        match self.collect_from_device(task_slug, &record).await {
            Ok(landed) => {
                let _ = std::fs::remove_file(self.record_path(task_slug));
                let message = if landed {
                    format!("The work is on branch {}", record.branch)
                } else {
                    "Nothing changed on the other machine".to_string()
                };
                tracing::info!(task = task_slug, branch = %record.branch, landed, "handoff returned");
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

    async fn collect_from_device(&self, task_slug: &str, record: &Record) -> anyhow::Result<bool> {
        // Where the work lands: here, unless the project is on another machine
        let source = match self.fleet.device(&record.source) {
            Some(source) => Some(source),
            None if record.source.is_empty() || record.source == self.own.id => None,
            None => bail!("the machine with the project is no longer in the fleet"),
        };
        let device = if record.device == self.own.id {
            self.own.clone()
        } else {
            self.fleet
                .device(&record.device)
                .context("the machine is no longer in the fleet")?
        };
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
