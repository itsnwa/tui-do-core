//! Paired clients: a phone or another app that reaches the home core from
//! elsewhere. Each gets its own token, so it can be revoked on its own, and the
//! core's token never leaves the machine.
//!
//! `clients.json` in the core's folder keeps a SHA-256 hash of each token,
//! never the token itself; the token is only shown once, when it's made.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

/// Client tokens start with this, so they're easy to tell from the core's own.
const TOKEN_PREFIX: &str = "tdc_";
/// How stale a stored "last seen" may get before it's written again.
const LAST_SEEN_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Client {
    id: String,
    name: String,
    token_hash: String,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_at: Option<String>,
}

/// A client as listed: everything but the token hash.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
}

impl From<&Client> for ClientInfo {
    fn from(c: &Client) -> Self {
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            created_at: c.created_at.clone(),
            last_seen_at: c.last_seen_at.clone(),
        }
    }
}

/// What `clients.json` holds; it moves to the new home with the home role.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    clients: Vec<Client>,
}

pub struct Clients {
    path: PathBuf,
    record: Mutex<Record>,
    /// Bumped on every revoke, so open sockets can check they're still allowed.
    revoked: watch::Sender<u64>,
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn stale(last_seen: Option<&str>) -> bool {
    let Some(at) = last_seen.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) else {
        return true;
    };
    let age = chrono::Utc::now().signed_duration_since(at);
    age.to_std().map_or(true, |age| age >= LAST_SEEN_EVERY)
}

impl Clients {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let record = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} isn't valid", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path,
            record: Mutex::new(record),
            revoked: watch::Sender::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, record: &Record) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = self.path.with_extension("json.tmp");
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

    pub fn list(&self) -> Vec<ClientInfo> {
        self.lock().clients.iter().map(ClientInfo::from).collect()
    }

    /// Make a client and its token. The token is only ever returned here.
    pub fn create(&self, name: &str) -> anyhow::Result<(ClientInfo, String)> {
        let name = name.trim();
        if name.is_empty() {
            bail!("the client needs a name");
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let token = format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        );
        let client = Client {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            token_hash: hash(&token),
            created_at: crate::workspace::dates::now_iso(),
            last_seen_at: None,
        };
        let mut record = self.lock();
        let mut next = record.clone();
        next.clients.push(client.clone());
        self.save(&next)?;
        *record = next;
        Ok((ClientInfo::from(&client), token))
    }

    /// Remove a client; its token stops working and its open sockets close.
    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut record = self.lock();
        let mut next = record.clone();
        let before = next.clients.len();
        next.clients.retain(|c| c.id != id);
        if next.clients.len() == before {
            return Ok(false);
        }
        self.save(&next)?;
        *record = next;
        drop(record);
        self.revoked.send_modify(|n| *n += 1);
        Ok(true)
    }

    /// The client a token belongs to, noting that it was seen.
    pub fn authenticate(&self, token: &str) -> Option<String> {
        if !token.starts_with(TOKEN_PREFIX) {
            return None;
        }
        let presented = hash(token);
        let mut record = self.lock();
        let index = record
            .clients
            .iter()
            .position(|c| constant_time_eq(c.token_hash.as_bytes(), presented.as_bytes()))?;
        let id = record.clients[index].id.clone();
        if stale(record.clients[index].last_seen_at.as_deref()) {
            let mut next = record.clone();
            next.clients[index].last_seen_at = Some(crate::workspace::dates::now_iso());
            match self.save(&next) {
                Ok(()) => *record = next,
                Err(err) => tracing::warn!(%err, "couldn't note when a client was seen"),
            }
        }
        Some(id)
    }

    /// Every client, token hashes included, to hand to a new home.
    pub fn record(&self) -> Record {
        self.lock().clone()
    }

    /// Replace every client (taking over as home, or giving the role away);
    /// sockets of clients that are gone close.
    pub fn replace(&self, next: Record) -> anyhow::Result<()> {
        let mut record = self.lock();
        self.save(&next)?;
        *record = next;
        drop(record);
        self.revoked.send_modify(|n| *n += 1);
        Ok(())
    }

    /// Revoke the client a token belongs to, if any.
    pub fn revoke_token(&self, token: &str) -> anyhow::Result<bool> {
        let presented = hash(token);
        let id = self
            .lock()
            .clients
            .iter()
            .find(|c| constant_time_eq(c.token_hash.as_bytes(), presented.as_bytes()))
            .map(|c| c.id.clone());
        match id {
            Some(id) => self.revoke(&id),
            None => Ok(false),
        }
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.lock().clients.iter().any(|c| c.id == id)
    }

    fn watch(&self) -> watch::Receiver<u64> {
        self.revoked.subscribe()
    }
}

/// Who made a request: the core's own token, or a paired client.
#[derive(Clone, Debug)]
pub enum Caller {
    Owner,
    Client(String),
}

/// Resolves once the client behind an open socket is revoked; never for the owner.
pub struct Revocation {
    watch: Option<(watch::Receiver<u64>, Arc<Clients>, String)>,
}

impl Revocation {
    pub fn new(clients: &Arc<Clients>, caller: Option<&Caller>) -> Self {
        let watch = match caller {
            Some(Caller::Client(id)) => Some((clients.watch(), Arc::clone(clients), id.clone())),
            _ => None,
        };
        Self { watch }
    }

    pub async fn revoked(&mut self) {
        let Some((rx, clients, id)) = &mut self.watch else {
            return std::future::pending().await;
        };
        loop {
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
            if !clients.is_active(id) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clients() -> Clients {
        let dir = std::env::temp_dir().join(format!("tui-do-clients-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        Clients::load(dir.join("clients.json")).expect("loads")
    }

    #[test]
    fn token_should_identify_its_client_until_revoked() {
        let clients = clients();
        let (client, token) = clients.create("iPhone").expect("creates");
        assert_eq!(clients.authenticate(&token), Some(client.id.clone()));
        assert!(clients.revoke(&client.id).expect("revokes"));
        assert_eq!(clients.authenticate(&token), None);
    }

    #[test]
    fn stored_record_should_not_contain_the_token() {
        let clients = clients();
        let (_, token) = clients.create("iPhone").expect("creates");
        let text = std::fs::read_to_string(&clients.path).expect("saved");
        assert!(!text.contains(&token));
    }

    #[test]
    fn other_tokens_should_not_authenticate() {
        let clients = clients();
        clients.create("iPhone").expect("creates");
        assert_eq!(clients.authenticate("tdc_nope"), None);
        assert_eq!(clients.authenticate(""), None);
    }

    #[tokio::test]
    async fn revoking_should_end_that_clients_sockets() {
        let clients = Arc::new(clients());
        let (client, _) = clients.create("iPhone").expect("creates");
        let mut revocation = Revocation::new(&clients, Some(&Caller::Client(client.id.clone())));
        clients.revoke(&client.id).expect("revokes");
        tokio::time::timeout(Duration::from_secs(1), revocation.revoked())
            .await
            .expect("resolves");
    }
}
