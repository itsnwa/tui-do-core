//! Install tui-do-core on another machine over ssh and add it to the fleet.
//!
//! The home core runs `remote.sh` on the target with `ssh … bash -s`: it
//! downloads this core's version from the GitHub release (checking its
//! SHA-256), installs it as a systemd user service or launchd agent, turns on
//! remote access and prints the new core's Tailscale address and token. SSH
//! access is what makes the machine trusted, so no pairing code is needed.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::fleet::{Device, Fleet};
use crate::session::Event;

const SCRIPT: &str = include_str!("remote.sh");
const REPO: &str = "itsnwa/tui-do-core";
/// The port cores listen on (loopback and Tailscale).
const PORT: u16 = 47821;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InstallRequest {
    /// An ssh host: an alias from ~/.ssh/config, `host` or `user@host`.
    pub host: String,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// Release to install; defaults to this core's own version.
    pub version: Option<String>,
    /// Workflow instructions for the coding agents there (see `agents`); when
    /// set, the machine's Claude Code / Codex get the TUI-DO tools registered.
    pub instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScriptLine {
    step: Option<String>,
    error: Option<String>,
    address: Option<String>,
    token: Option<String>,
    linger: Option<String>,
}

/// Only plain host names: anything that could be read as an ssh option is refused.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host.len() <= 255
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':'))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

pub async fn install(
    req: InstallRequest,
    fleet: &Fleet,
    own_id: &str,
    events: &broadcast::Sender<Event>,
) -> anyhow::Result<Device> {
    if !valid_host(&req.host) {
        bail!("invalid ssh host {:?}", req.host);
    }
    let version = req
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    if !valid_version(&version) {
        bail!("invalid version {version:?}");
    }
    let progress = |step: &str| {
        tracing::info!(host = %req.host, step, "install");
        let _ = events.send(Event::InstallProgress {
            host: req.host.clone(),
            step: step.to_string(),
        });
    };
    progress("Connecting");

    let mut ssh = tokio::process::Command::new("ssh");
    // BatchMode: never hang on a password or host-key prompt
    ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    if let Some(port) = req.port {
        ssh.args(["-p", &port.to_string()]);
    }
    if let Some(identity) = req
        .identity_file
        .as_deref()
        .filter(|i| !i.trim().is_empty())
    {
        ssh.arg("-i")
            .arg(crate::session::expand_home(identity.trim()));
    }
    ssh.args([
        "--",
        &req.host,
        "bash",
        "-s",
        "--",
        &version,
        REPO,
        &PORT.to_string(),
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = ssh.spawn().context("couldn't run ssh")?;
    let mut stdin = child.stdin.take().context("ssh has no stdin")?;
    stdin.write_all(SCRIPT.as_bytes()).await?;
    drop(stdin);

    let mut lines = BufReader::new(child.stdout.take().context("ssh has no stdout")?).lines();
    let mut result: Option<(String, String, Option<String>)> = None;
    let mut failure: Option<String> = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(parsed) = serde_json::from_str::<ScriptLine>(&line) else {
            continue;
        };
        if let Some(step) = parsed.step {
            progress(&step);
        }
        if let Some(error) = parsed.error {
            failure = Some(error);
        }
        if let (Some(address), Some(token)) = (parsed.address, parsed.token) {
            result = Some((address, token, parsed.linger));
        }
    }
    let output = child.wait_with_output().await?;
    if let Some(error) = failure {
        bail!("{error}");
    }
    let Some((address, token, linger)) = result else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output");
        bail!("installing over ssh failed: {reason}");
    };
    if linger.as_deref() == Some("no") {
        tracing::warn!(host = %req.host, "lingering is off: the core stops when the user logs out (loginctl enable-linger)");
    }

    // The new core starts listening on Tailscale within a few seconds
    progress("Adding it to the fleet");
    let mut last_error = None;
    for _ in 0..15 {
        match fleet.add(&address, &token, own_id, Some(&req.host)).await {
            Ok(device) => {
                progress("Done");
                return Ok(device);
            }
            Err(err) => last_error = Some(err),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("couldn't reach the new core"))
        .context(format!(
            "the core is installed on {} but couldn't be reached at {address}",
            req.host
        )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_that_look_like_options_should_be_refused() {
        assert!(!valid_host("-oProxyCommand=evil"));
        assert!(!valid_host("cube; rm -rf /"));
        assert!(valid_host("nichlas@cube.tailf627b2.ts.net"));
    }
}
