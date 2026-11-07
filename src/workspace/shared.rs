//! The workspace the core serves, chosen by a client (`PUT /v0/workspace`)
//! and remembered in the core's `config.json`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::{Result, Workspace, lock, watch};
use crate::session::Event;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    workspace_path: Option<PathBuf>,
}

pub struct SharedWorkspace {
    config_path: PathBuf,
    current: RwLock<Option<Arc<Workspace>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    events: broadcast::Sender<Event>,
}

impl SharedWorkspace {
    /// Open the configured workspace, if any. `TUI_DO_WORKSPACE` overrides the
    /// config (for testing against a copy).
    pub fn load(config_path: PathBuf, events: broadcast::Sender<Event>) -> Self {
        let shared = Self {
            config_path,
            current: RwLock::new(None),
            watcher: Mutex::new(None),
            events,
        };
        let path = std::env::var_os("TUI_DO_WORKSPACE")
            .map(PathBuf::from)
            .or_else(|| {
                let text = std::fs::read_to_string(&shared.config_path).ok()?;
                serde_json::from_str::<Config>(&text).ok()?.workspace_path
            });
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
        let config = Config {
            workspace_path: Some(path.to_path_buf()),
        };
        let text = serde_json::to_string_pretty(&config).context("encoding the config")? + "\n";
        std::fs::write(&self.config_path, text)
            .with_context(|| format!("writing {}", self.config_path.display()))?;
        Ok(())
    }
}
