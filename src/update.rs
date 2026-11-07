//! Updating this core in place from a GitHub release: download the build for
//! this machine, check its checksum, swap the binary, then exit so the service
//! (launchd's KeepAlive, systemd's `Restart=always`) starts the new one.
//! Exiting ends the sessions running here.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::process::Command;

use crate::install::{REPO, valid_version};

/// How long the latest release number is reused before asking GitHub again.
const LATEST_TTL: Duration = Duration::from_secs(15 * 60);

static LATEST: Mutex<Option<(Instant, String)>> = Mutex::new(None);
static REFRESHING: AtomicBool = AtomicBool::new(false);

/// The release build for this machine.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The launchd label both installs use (`scripts/install-dev.sh`, `install/remote.sh`).
const LAUNCHD_LABEL: &str = "dev.tui-do.core";

/// Running as the launchd agent or systemd service, which starts the core
/// again after it exits. (Terminal shells on macOS set `XPC_SERVICE_NAME`
/// too, to something else.)
pub fn restarts_itself() -> bool {
    std::env::var("XPC_SERVICE_NAME").is_ok_and(|name| name == LAUNCHD_LABEL)
        || std::env::var_os("INVOCATION_ID").is_some()
}

async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The newest release's version (without the `v`), cached for a while.
pub async fn latest() -> anyhow::Result<String> {
    if let Ok(cache) = LATEST.lock()
        && let Some((at, version)) = cache.as_ref()
        && at.elapsed() < LATEST_TTL
    {
        return Ok(version.clone());
    }
    let body = run(
        "curl",
        &[
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ],
    )
    .await
    .context("couldn't ask GitHub for the latest release")?;
    let release: serde_json::Value = serde_json::from_str(&body)?;
    let version = release["tag_name"]
        .as_str()
        .context("the latest release has no tag")?
        .trim_start_matches('v')
        .to_string();
    if !valid_version(&version) {
        bail!("unexpected release tag {version:?}");
    }
    if let Ok(mut cache) = LATEST.lock() {
        *cache = Some((Instant::now(), version.clone()));
    }
    Ok(version)
}

/// The latest release as last seen, without waiting for GitHub: a missing or
/// stale one is looked up again in the background.
pub fn latest_known() -> Option<String> {
    let cached = LATEST.lock().ok().and_then(|cache| cache.clone());
    let fresh = cached
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < LATEST_TTL);
    if !fresh && !REFRESHING.swap(true, Ordering::SeqCst) {
        tokio::spawn(async {
            if let Err(err) = latest().await {
                tracing::warn!(error = %format!("{err:#}"), "couldn't check for updates");
            }
            REFRESHING.store(false, Ordering::SeqCst);
        });
    }
    cached.map(|(_, version)| version)
}

async fn sha256(file: &Path) -> anyhow::Result<String> {
    let path = file.to_string_lossy();
    let out = match run("sha256sum", &[&path]).await {
        Ok(out) => out,
        Err(_) => run("shasum", &["-a", "256", &path]).await?,
    };
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .context("no checksum")
}

/// Download `version` and put it in place of the running binary. The caller
/// then calls [`restart_soon`].
pub async fn install(version: &str) -> anyhow::Result<()> {
    if !valid_version(version) {
        bail!("invalid version {version:?}");
    }
    let Some(target) = target() else {
        bail!("tui-do-core has no release build for this machine");
    };
    if !restarts_itself() {
        bail!("tui-do-core isn't running as a service here, so it can't restart itself");
    }
    let exe = std::env::current_exe().context("finding tui-do-core's own path")?;
    let dir = exe.parent().context("tui-do-core's folder")?;
    // Next to the binary, so the final rename stays on one filesystem
    let tmp = dir.join(format!(".update-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    let result = download_into(&tmp, version, target, &exe).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

async fn download_into(tmp: &Path, version: &str, target: &str, exe: &Path) -> anyhow::Result<()> {
    let archive = format!("tui-do-core-{target}.tar.gz");
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let archive_path = tmp.join(&archive);
    let sum_path = tmp.join(format!("{archive}.sha256"));
    for (url, path) in [
        (format!("{base}/{archive}"), &archive_path),
        (format!("{base}/{archive}.sha256"), &sum_path),
    ] {
        run(
            "curl",
            &[
                "-fsSL",
                "--max-time",
                "300",
                "-o",
                &path.to_string_lossy(),
                &url,
            ],
        )
        .await
        .with_context(|| format!("couldn't download {url}"))?;
    }
    let expected = std::fs::read_to_string(&sum_path)?;
    let expected = expected.split_whitespace().next().unwrap_or_default();
    if sha256(&archive_path).await? != expected {
        bail!("the download didn't match its checksum");
    }
    run(
        "tar",
        &[
            "-xzf",
            &archive_path.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ],
    )
    .await?;
    let new = tmp
        .join(format!("tui-do-core-{target}"))
        .join("tui-do-core");
    let staged = exe.with_extension("new");
    std::fs::copy(&new, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    // Replacing a running binary by renaming over it is fine on Unix
    std::fs::rename(&staged, exe)?;
    tracing::info!(version, "updated; restarting");
    Ok(())
}

/// Exit shortly (after the response has gone out); the service starts the new binary.
pub fn restart_soon() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(500));
        std::process::exit(0);
    });
}
