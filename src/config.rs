//! The core's settings, in `config.json` in its directory: which workspace it
//! serves, which device it is, and whether it's reachable over Tailscale.
//! Unknown keys are kept, so newer settings survive an older binary.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<PathBuf>,
    /// Stable identity of this core within a fleet.
    #[serde(default)]
    pub device_id: String,
    /// Shown to people; defaults to the host name.
    #[serde(default)]
    pub device_name: String,
    /// Also listen on this machine's Tailscale address (other devices in the
    /// fleet reach it there). Off until the machine joins a fleet.
    #[serde(default)]
    pub remote_access: bool,
    /// On a runner: the home core to forward MCP tool calls to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<HomeLink>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// How a runner reaches its home core: its Tailscale base URL and an
/// MCP-only token the home core issued for this runner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HomeLink {
    pub address: String,
    pub token: String,
}

pub struct ConfigStore {
    path: PathBuf,
    current: Mutex<CoreConfig>,
}

/// The host name without a `.local` suffix.
fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().trim_end_matches(".local").to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "tui-do-core".into())
}

impl ConfigStore {
    /// Load the config, giving the device an id and name on first run.
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let mut config: CoreConfig = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} isn't valid", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => CoreConfig::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let first_run = config.device_id.is_empty() || config.device_name.is_empty();
        if config.device_id.is_empty() {
            config.device_id = uuid::Uuid::new_v4().to_string();
        }
        if config.device_name.is_empty() {
            config.device_name = host_name();
        }
        let store = Self {
            path,
            current: Mutex::new(config),
        };
        if first_run {
            store.save(&store.get())?;
        }
        Ok(store)
    }

    pub fn get(&self) -> CoreConfig {
        self.current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Change the config and write it to disk.
    pub fn update(&self, change: impl FnOnce(&mut CoreConfig)) -> anyhow::Result<CoreConfig> {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = current.clone();
        change(&mut next);
        self.save(&next)?;
        *current = next.clone();
        Ok(next)
    }

    fn save(&self, config: &CoreConfig) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(config)? + "\n";
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
