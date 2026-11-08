//! The workspace the core serves, chosen by a client (`PUT /v0/workspace`)
//! and remembered in the core's config. A core without one uses
//! `~/.tui-do/workspace`, where a runner keeps its copy of the home's (see
//! `replica`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::broadcast;

use super::{Result, Workspace, lock, watch};
use crate::config::ConfigStore;
use crate::session::Event;

pub struct SharedWorkspace {
    config: Arc<ConfigStore>,
    /// The folder used when none is configured.
    default_folder: PathBuf,
    current: RwLock<Option<Arc<Workspace>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    events: broadcast::Sender<Event>,
    /// Set while the home moves: nothing may be written.
    frozen: AtomicBool,
    /// Held (shared) by every write for as long as it runs, and (exclusive)
    /// by a move, which so waits for writes under way to finish.
    write_gate: tokio::sync::RwLock<()>,
}

impl SharedWorkspace {
    /// Open the configured workspace, if any. `TUI_DO_WORKSPACE` overrides the
    /// config (for testing against a copy).
    pub fn load(
        config: Arc<ConfigStore>,
        events: broadcast::Sender<Event>,
        default_folder: PathBuf,
    ) -> Self {
        let path = std::env::var_os("TUI_DO_WORKSPACE")
            .map(PathBuf::from)
            .or_else(|| config.get().workspace_path)
            .or_else(|| {
                default_folder
                    .join("tasks")
                    .is_dir()
                    .then(|| default_folder.clone())
            });
        let shared = Self {
            config,
            default_folder,
            current: RwLock::new(None),
            watcher: Mutex::new(None),
            events,
            frozen: AtomicBool::new(false),
            write_gate: tokio::sync::RwLock::new(()),
        };
        if let Some(path) = path
            && let Err(err) = shared.open(&path)
        {
            tracing::warn!(path = %path.display(), %err, "couldn't open the workspace");
        }
        shared
    }

    pub fn current(&self) -> Option<Arc<Workspace>> {
        self.current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn freeze(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::SeqCst);
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }

    /// A write's hold on the workspace (see `write_gate`).
    pub async fn writing(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.write_gate.read().await
    }

    /// Wait for writes under way to finish and keep new ones out, for up
    /// to `wait`.
    pub async fn hold_writes(
        &self,
        wait: std::time::Duration,
    ) -> Option<tokio::sync::RwLockWriteGuard<'_, ()>> {
        tokio::time::timeout(wait, self.write_gate.write())
            .await
            .ok()
    }

    /// The folder used when none is configured (`~/.tui-do/workspace`).
    pub fn default_folder(&self) -> &Path {
        &self.default_folder
    }

    /// The folder this core's workspace is in, or would be in.
    pub fn folder(&self) -> PathBuf {
        if let Some(ws) = self.current() {
            return ws.root().to_path_buf();
        }
        self.config
            .get()
            .workspace_path
            .unwrap_or_else(|| self.default_folder.clone())
    }

    /// Open the folder if it isn't open yet (a runner's first copy arrived).
    pub fn ensure_open(&self) -> Result<()> {
        if self.current().is_some() {
            return Ok(());
        }
        self.open(&self.folder())
    }

    fn open(&self, path: &Path) -> Result<()> {
        let workspace = Arc::new(Workspace::open(path.to_path_buf())?);
        let events = self.events.clone();
        let watcher = watch::watch(path.to_path_buf(), move |areas| {
            let _ = events.send(Event::WorkspaceChanged { areas });
        })
        .map_err(|e| anyhow::anyhow!("watching {}: {e}", path.display()))?;
        *lock(&self.watcher) = Some(watcher);
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = Some(workspace);
        tracing::info!(path = %path.display(), "workspace open");
        Ok(())
    }

    /// Switch to another workspace folder and remember it.
    pub fn set(&self, path: &Path) -> Result<()> {
        if !path.is_absolute() {
            return Err(super::Error::Invalid(
                "the workspace path must be absolute".into(),
            ));
        }
        self.open(path)?;
        self.config
            .update(|c| c.workspace_path = Some(path.to_path_buf()))?;
        Ok(())
    }
}
