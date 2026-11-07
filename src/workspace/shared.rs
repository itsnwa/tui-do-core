//! The workspace the core serves, chosen by a client (`PUT /v0/workspace`)
//! and remembered in the core's config.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::broadcast;

use super::{Result, Workspace, lock, watch};
use crate::config::ConfigStore;
use crate::session::Event;

pub struct SharedWorkspace {
    config: Arc<ConfigStore>,
    current: RwLock<Option<Arc<Workspace>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    events: broadcast::Sender<Event>,
}

impl SharedWorkspace {
    /// Open the configured workspace, if any. `TUI_DO_WORKSPACE` overrides the
    /// config (for testing against a copy).
    pub fn load(config: Arc<ConfigStore>, events: broadcast::Sender<Event>) -> Self {
        let path = std::env::var_os("TUI_DO_WORKSPACE")
            .map(PathBuf::from)
            .or_else(|| config.get().workspace_path);
        let shared = Self {
            config,
            current: RwLock::new(None),
            watcher: Mutex::new(None),
            events,
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
