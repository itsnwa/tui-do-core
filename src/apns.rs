//! Apple push notifications, sent from the home core with the app's APNs
//! auth key.
//!
//! The key (a `.p8` file from the Apple Developer portal), its key id and
//! the team id are kept in `apns.json` in the home core's folder, readable
//! only by its user. A new home gets a copy when the home moves. Requests go through `curl`
//! (HTTP/2, which APNs needs), as updates do, so the core needs no TLS stack
//! of its own.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;

const FILE: &str = "apns.json";
/// APNs takes a provider token for an hour, and refuses new ones more often
/// than every 20 minutes.
const TOKEN_LIFETIME: Duration = Duration::from_secs(50 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Key {
    pub key_id: String,
    pub team_id: String,
    /// The `.p8` file's contents (a PKCS#8 PEM).
    pub key: String,
}

/// Where a client's notifications go: its device token, the APNs server for
/// its build (`development` or `production`), and the app's bundle id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub device_token: String,
    pub environment: String,
    pub topic: String,
}

impl Target {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.device_token.is_empty()
            || self.device_token.len() > 200
            || !self.device_token.chars().all(|c| c.is_ascii_hexdigit())
        {
            bail!("the device token should be hex");
        }
        if !matches!(self.environment.as_str(), "development" | "production") {
            bail!("environment should be development or production");
        }
        if self.topic.is_empty()
            || !self
                .topic
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            bail!("the topic should be the app's bundle id");
        }
        Ok(())
    }

    fn host(&self) -> &'static str {
        if self.environment == "development" {
            "https://api.sandbox.push.apple.com"
        } else {
            "https://api.push.apple.com"
        }
    }
}

pub struct Alert {
    pub title: String,
    pub body: String,
    /// Opened when the notification is tapped.
    pub link: Option<String>,
    /// Notifications with the same thread are grouped.
    pub thread: Option<String>,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The key on this core, if it has one.
pub fn load(dir: &Path) -> Option<Key> {
    let text = std::fs::read_to_string(path(dir)).ok()?;
    match serde_json::from_str(&text) {
        Ok(key) => Some(key),
        Err(err) => {
            tracing::warn!(%err, "apns.json isn't valid");
            None
        }
    }
}

pub fn save(dir: &Path, key: &Key) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let target = path(dir);
    let tmp = target.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all((serde_json::to_string_pretty(key)? + "\n").as_bytes())?;
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

/// A key from the portal's `.p8` file. Its id comes from the file's name
/// (`AuthKey_<key id>.p8`) unless given.
pub fn from_file(p8: &Path, team_id: &str, key_id: Option<&str>) -> anyhow::Result<Key> {
    let key = std::fs::read_to_string(p8).with_context(|| format!("reading {}", p8.display()))?;
    SigningKey::from_pkcs8_pem(&key).context("that isn't an APNs auth key (.p8)")?;
    let key_id = match key_id {
        Some(id) => id.to_string(),
        None => p8
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("AuthKey_"))
            .context("give the key id: the file isn't named AuthKey_<key id>.p8")?
            .to_string(),
    };
    let valid = |id: &str| id.len() == 10 && id.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid(&key_id) {
        bail!("the key id should be 10 letters and digits, not {key_id:?}");
    }
    if !valid(team_id) {
        bail!("the team id should be 10 letters and digits, not {team_id:?}");
    }
    Ok(Key {
        key_id,
        team_id: team_id.to_string(),
        key,
    })
}

/// The last provider token: (key id, made at, token).
static PROVIDER_TOKEN: Mutex<Option<(String, Instant, String)>> = Mutex::new(None);

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A JWT signed with the key (ES256), as APNs wants.
fn sign(key: &Key, issued_at: u64) -> anyhow::Result<String> {
    let signing = SigningKey::from_pkcs8_pem(&key.key).context("the APNs key isn't valid")?;
    let header = base64url(
        json!({ "alg": "ES256", "kid": key.key_id })
            .to_string()
            .as_bytes(),
    );
    let claims = base64url(
        json!({ "iss": key.team_id, "iat": issued_at })
            .to_string()
            .as_bytes(),
    );
    let input = format!("{header}.{claims}");
    let signature: Signature = signing.sign(input.as_bytes());
    Ok(format!("{input}.{}", base64url(&signature.to_bytes())))
}

fn provider_token(key: &Key) -> anyhow::Result<String> {
    let mut cached = PROVIDER_TOKEN.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((id, at, token)) = cached.as_ref()
        && *id == key.key_id
        && at.elapsed() < TOKEN_LIFETIME
    {
        return Ok(token.clone());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = sign(key, now)?;
    *cached = Some((key.key_id.clone(), Instant::now(), token.clone()));
    Ok(token)
}

/// A value in a curl config file: quoted, with `\` and `"` escaped.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn payload(alert: &Alert) -> serde_json::Value {
    let mut aps = json!({
        "alert": { "title": alert.title, "body": alert.body },
        "sound": "default",
    });
    if let Some(thread) = &alert.thread {
        aps["thread-id"] = thread.clone().into();
    }
    let mut payload = json!({ "aps": aps });
    if let Some(link) = &alert.link {
        payload["link"] = link.clone().into();
    }
    payload
}

#[derive(Debug, PartialEq)]
pub enum Sent {
    Delivered,
    /// The device token isn't valid any more (the app was removed, say).
    Unregistered,
}

/// Send one notification.
pub async fn send(key: &Key, target: &Target, alert: &Alert) -> anyhow::Result<Sent> {
    target.validate()?;
    let token = provider_token(key)?;
    // Passed on stdin, so the token doesn't show in the process list
    let config = [
        format!(
            "url = {}",
            quoted(&format!(
                "{}/3/device/{}",
                target.host(),
                target.device_token
            ))
        ),
        format!(
            "header = {}",
            quoted(&format!("authorization: bearer {token}"))
        ),
        format!(
            "header = {}",
            quoted(&format!("apns-topic: {}", target.topic))
        ),
        format!("header = {}", quoted("apns-push-type: alert")),
        format!("header = {}", quoted("apns-priority: 10")),
        format!("data = {}", quoted(&payload(alert).to_string())),
    ]
    .join("\n");
    let mut child = tokio::process::Command::new("curl")
        .args([
            "--http2",
            "-sS",
            "--max-time",
            "15",
            "-w",
            "\n%{http_code}",
            "-K",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running curl")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body, status) = stdout.trim_end().rsplit_once('\n').unwrap_or(("", &stdout));
    match status.trim() {
        "200" => Ok(Sent::Delivered),
        "410" => Ok(Sent::Unregistered),
        "000" | "" => bail!(
            "couldn't reach APNs: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        status => {
            let reason = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.trim().to_string());
            bail!("APNs refused the notification ({status}): {reason}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    fn key() -> Key {
        use rand::RngExt;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let signing = SigningKey::from_slice(&bytes).expect("a valid scalar");
        Key {
            key_id: "ABC123DEFG".into(),
            team_id: "C4H3XBVK2C".into(),
            key: signing
                .to_pkcs8_pem(LineEnding::LF)
                .expect("pem")
                .to_string(),
        }
    }

    #[test]
    fn provider_token_should_be_a_verifiable_es256_jwt() {
        let key = key();
        let token = sign(&key, 1_700_000_000).expect("signs");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decode = |s: &str| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .expect("base64url");
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
        };
        assert_eq!(
            decode(parts[0]),
            json!({ "alg": "ES256", "kid": "ABC123DEFG" })
        );
        assert_eq!(
            decode(parts[1]),
            json!({ "iss": "C4H3XBVK2C", "iat": 1_700_000_000 })
        );
        let signing = SigningKey::from_pkcs8_pem(&key.key).expect("key");
        let signature = Signature::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[2])
                .expect("base64url"),
        )
        .expect("64 bytes");
        VerifyingKey::from(&signing)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("verifies");
    }

    #[test]
    fn key_should_be_read_from_a_p8_file() {
        let dir = std::env::temp_dir().join(format!("tui-do-apns-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p8 = dir.join("AuthKey_ABC123DEFG.p8");
        std::fs::write(&p8, key().key).expect("write");
        let read = from_file(&p8, "C4H3XBVK2C", None).expect("reads");
        assert_eq!(read.key_id, "ABC123DEFG");
        assert!(from_file(&p8, "nope", None).is_err());
        let other = dir.join("key.p8");
        std::fs::write(&other, key().key).expect("write");
        assert!(from_file(&other, "C4H3XBVK2C", None).is_err());
        assert!(from_file(&other, "C4H3XBVK2C", Some("XYZ987WVUT")).is_ok());
        std::fs::write(&other, "not a key").expect("write");
        assert!(from_file(&other, "C4H3XBVK2C", Some("XYZ987WVUT")).is_err());

        save(&dir, &read).expect("saves");
        assert_eq!(load(&dir).expect("loads").key_id, "ABC123DEFG");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(FILE))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn targets_should_be_checked() {
        let target = Target {
            device_token: "a1b2c3".into(),
            environment: "development".into(),
            topic: "com.itsnwa.tuido".into(),
        };
        assert!(target.validate().is_ok());
        assert_eq!(target.host(), "https://api.sandbox.push.apple.com");
        let bad = |change: fn(&mut Target)| {
            let mut t = target.clone();
            change(&mut t);
            t.validate().is_err()
        };
        assert!(bad(|t| t.device_token = "../clients".into()));
        assert!(bad(|t| t.device_token = String::new()));
        assert!(bad(|t| t.environment = "staging".into()));
        assert!(bad(|t| t.topic = "com.x\"\nurl = evil".into()));
    }

    #[test]
    fn payload_should_carry_the_link_and_thread() {
        let alert = Alert {
            title: "Fix it · cube".into(),
            body: "Which one?".into(),
            link: Some("tuido://session/a/b".into()),
            thread: Some("a/b".into()),
        };
        assert_eq!(
            payload(&alert),
            json!({
                "aps": {
                    "alert": { "title": "Fix it · cube", "body": "Which one?" },
                    "sound": "default",
                    "thread-id": "a/b",
                },
                "link": "tuido://session/a/b",
            })
        );
    }

    #[test]
    fn config_values_should_be_quoted() {
        assert_eq!(quoted(r#"{"a":"b\c"}"#), r#""{\"a\":\"b\\c\"}""#);
    }
}
