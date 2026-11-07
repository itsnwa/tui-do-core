//! The fleet: the other machines this core (the home core) knows about.
//!
//! The home core keeps the record in `fleet.json` in its directory: each
//! device's id, name, address and the token to reach it. Clients only ever
//! talk to the home core, which forwards device requests (see `api::fleet`),
//! so device tokens never leave it.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Base URL, e.g. `http://100.79.154.110:47821`.
    pub address: String,
    pub token: String,
    pub platform: String,
    pub added_at: String,
}

/// A device as clients see it: no token, plus whether it's reachable.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    pub address: String,
    pub platform: String,
    pub added_at: String,
    pub online: bool,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FleetRecord {
    /// Bumped whenever the home role moves (see phase 3f).
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    devices: Vec<Device>,
}

/// What a device answers on `GET /v0/device`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub version: String,
}

pub struct Fleet {
    path: PathBuf,
    record: Mutex<FleetRecord>,
    client: reqwest::Client,
}

const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

impl Fleet {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let record = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} isn't valid", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => FleetRecord::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let client = reqwest::Client::builder()
            .connect_timeout(HEALTH_TIMEOUT)
            .build()?;
        Ok(Self {
            path,
            record: Mutex::new(record),
            client,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn snapshot(&self) -> FleetRecord {
        self.record
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn save(&self, record: &FleetRecord) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = self.path.with_extension("json.tmp");
        // The record holds device tokens: readable by this user only
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all((serde_json::to_string_pretty(record)? + "\n").as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.snapshot().generation
    }

    pub fn devices(&self) -> Vec<Device> {
        self.snapshot().devices
    }

    pub fn device(&self, id: &str) -> Option<Device> {
        self.devices().into_iter().find(|d| d.id == id)
    }

    /// Ask a device who it is, using its token. Fails if it can't be reached
    /// or the token is wrong.
    pub async fn identify(&self, address: &str, token: &str) -> anyhow::Result<DeviceInfo> {
        let res = self
            .client
            .get(format!("{address}/v0/device"))
            .bearer_auth(token)
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("couldn't reach a core at {address}"))?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("the core at {address} didn't accept the token");
        }
        if !res.status().is_success() {
            bail!("the core at {address} answered {}", res.status());
        }
        res.json().await.context("unexpected answer from the core")
    }

    /// Register a device after checking it answers with its token. Adding a
    /// device that's already known updates its address and token.
    pub async fn add(&self, address: &str, token: &str, own_id: &str) -> anyhow::Result<Device> {
        let address = address.trim_end_matches('/').to_string();
        let info = self.identify(&address, token).await?;
        if info.id == own_id {
            bail!("that's this core itself");
        }
        let device = Device {
            id: info.id,
            name: info.name,
            address,
            token: token.to_string(),
            platform: info.platform,
            added_at: crate::workspace::dates::now_iso(),
        };
        let mut record = self.record.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = record.clone();
        match next.devices.iter_mut().find(|d| d.id == device.id) {
            Some(existing) => {
                existing.name.clone_from(&device.name);
                existing.address.clone_from(&device.address);
                existing.token.clone_from(&device.token);
                existing.platform.clone_from(&device.platform);
            }
            None => next.devices.push(device.clone()),
        }
        self.save(&next)?;
        *record = next;
        Ok(device)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<bool> {
        let mut record = self.record.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = record.clone();
        let before = next.devices.len();
        next.devices.retain(|d| d.id != id);
        if next.devices.len() == before {
            return Ok(false);
        }
        self.save(&next)?;
        *record = next;
        Ok(true)
    }

    /// Every device with whether it's reachable right now (checked in parallel).
    pub async fn statuses(&self) -> Vec<DeviceStatus> {
        let checks = self.devices().into_iter().map(|d| async move {
            let info = self.identify(&d.address, &d.token).await.ok();
            DeviceStatus {
                online: info.is_some(),
                version: info.map(|i| i.version),
                id: d.id,
                name: d.name,
                address: d.address,
                platform: d.platform,
                added_at: d.added_at,
            }
        });
        futures_util::future::join_all(checks).await
    }
}
