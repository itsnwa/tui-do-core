//! Updating this core from a GitHub release: download the build for this
//! machine, check its checksum, switch to it, then exit so the service
//! (launchd's KeepAlive, systemd's `Restart=always`) starts the new one.
//! Sessions keep running: their programs are in holders (see `holder`), which
//! the new core picks up.
//!
//! Versions sit side by side in `~/.tui-do/versions/<version>/`, and
//! `~/.tui-do/bin/tui-do-core` is a link to the current one, so the services
//! and the agents' MCP settings keep one path. A new version that doesn't come
//! up can't be trusted to undo itself, so a watcher from the old version
//! stays behind, and switches back if the new one doesn't report healthy.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::json;
use tokio::process::Command;

use crate::install::{REPO, valid_version};
use crate::session::Registry;

/// How long the latest release number is reused before asking GitHub again.
const LATEST_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a new version gets to come up before it's rolled back.
const WATCH_FOR: Duration = Duration::from_secs(45);
/// Health checks in a row, a second apart, for a new version to count as up.
const STEADY_CHECKS: u32 = 10;
const BINARY: &str = "tui-do-core";
/// Written by the watcher in the core's folder when it switches back.
const ROLLBACK_FILE: &str = "update-rollback.json";

static LATEST: Mutex<Option<(Instant, String)>> = Mutex::new(None);
static REFRESHING: AtomicBool = AtomicBool::new(false);
static UPDATING: AtomicBool = AtomicBool::new(false);

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
/// Another one lets a test install run beside the real one.
fn launchd_label() -> String {
    std::env::var("TUI_DO_LAUNCHD_LABEL").unwrap_or_else(|_| "dev.tui-do.core".into())
}

/// Running as the launchd agent or systemd service, which starts the core
/// again after it exits. (Terminal shells on macOS set `XPC_SERVICE_NAME`
/// too, to something else.)
pub fn restarts_itself() -> bool {
    std::env::var("XPC_SERVICE_NAME").is_ok_and(|name| name == launchd_label())
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

/// `~/.tui-do`: the folder around the core's own.
fn root_of(core_dir: &Path) -> anyhow::Result<&Path> {
    core_dir.parent().context("the core's folder has no parent")
}

fn current_link(root: &Path) -> PathBuf {
    root.join("bin").join(BINARY)
}

/// What the current link points at for a version, relative to `bin/`.
fn link_target(version: &str) -> PathBuf {
    Path::new("../versions").join(version).join(BINARY)
}

/// Point the link at `target`, in one step.
fn point_link(link: &Path, target: &Path) -> std::io::Result<()> {
    let tmp = link.with_extension("link");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link)
}

/// Cores installed before versions sat side by side have a plain file at the
/// link's path. Moving it there doesn't disturb the running core.
pub fn use_versions_folder(core_dir: &Path, version: &str) -> anyhow::Result<()> {
    let root = root_of(core_dir)?;
    let link = current_link(root);
    let Ok(meta) = std::fs::symlink_metadata(&link) else {
        return Ok(());
    };
    if !meta.file_type().is_file() {
        return Ok(());
    }
    let binary = root.join("versions").join(version).join(BINARY);
    std::fs::create_dir_all(binary.parent().context("the version's folder")?)?;
    let _ = std::fs::remove_file(&binary);
    std::fs::hard_link(&link, &binary)?;
    point_link(&link, &link_target(version))?;
    tracing::info!(version, "moved the core into its versions folder");
    Ok(())
}

/// This program's path, for starting it again later. An installed core uses
/// the current link: on Linux `current_exe` gives the file behind it, which a
/// later update deletes, or a path that's already gone if
/// [`use_versions_folder`] replaced the file this core started from.
pub fn own_path(core_dir: &Path) -> anyhow::Result<PathBuf> {
    if restarts_itself() {
        let link = current_link(root_of(core_dir)?);
        if link.exists() {
            return Ok(link);
        }
    }
    std::env::current_exe().context("finding tui-do-core's own path")
}

/// A systemd unit with `KillMode=process` added, or None if there's nothing to do.
fn with_kill_mode(unit: &str) -> Option<String> {
    if unit.contains("KillMode=") || !unit.contains("[Service]\n") {
        return None;
    }
    Some(unit.replacen("[Service]\n", "[Service]\nKillMode=process\n", 1))
}

/// Keep holders alive when the service stops: by default systemd ends every
/// process the service started. The unit is only written at install time, so
/// machines installed before holders get it here. (launchd only ends the
/// service's process group, which holders leave.)
pub async fn keep_holders_through_restarts() -> anyhow::Result<()> {
    if std::env::consts::OS != "linux" {
        return Ok(());
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home).join(".config/systemd/user/tui-do-core.service");
    let unit =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let Some(patched) = with_kill_mode(&unit) else {
        return Ok(());
    };
    let tmp = path.with_extension("service.new");
    std::fs::write(&tmp, patched)?;
    std::fs::rename(&tmp, &path)?;
    run("systemctl", &["--user", "daemon-reload"]).await?;
    tracing::info!("the service now leaves holders running when it stops");
    Ok(())
}

/// Download `version`, switch to it and start the watcher. The caller then
/// calls [`restart_soon`].
pub async fn install(version: &str, core_dir: &Path, endpoint: &str) -> anyhow::Result<()> {
    if !valid_version(version) {
        bail!("invalid version {version:?}");
    }
    let Some(target) = target() else {
        bail!("tui-do-core has no release build for this machine");
    };
    if !restarts_itself() {
        bail!("tui-do-core isn't running as a service here, so it can't restart itself");
    }
    if UPDATING.swap(true, Ordering::SeqCst) {
        bail!("an update is already running");
    }
    let result = install_version(version, target, core_dir, endpoint).await;
    if result.is_err() {
        UPDATING.store(false, Ordering::SeqCst);
    }
    result
}

async fn install_version(
    version: &str,
    target: &str,
    core_dir: &Path,
    endpoint: &str,
) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(core_dir.join(ROLLBACK_FILE));
    let root = root_of(core_dir)?;
    let versions = root.join("versions");
    // Beside the versions, so the final rename stays on one filesystem
    let tmp = versions.join(format!(".update-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    let result = download_into(&tmp, version, target, &versions).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    check_runs(&versions.join(version).join(BINARY), version).await?;

    let link = current_link(root);
    let previous = std::fs::read_link(&link).context("the current version isn't a link")?;
    point_link(&link, &link_target(version)).context("switching to the new version")?;
    if let Err(err) = start_watcher(&link, &previous, core_dir, version, endpoint) {
        let _ = point_link(&link, &previous);
        return Err(err);
    }
    tracing::info!(version, "updated; restarting");
    Ok(())
}

/// Where releases are downloaded from; another base lets updates be tried
/// against a local folder.
fn release_base(version: &str) -> String {
    let base = std::env::var("TUI_DO_RELEASES_URL")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}/releases/download"));
    format!("{base}/v{version}")
}

async fn download_into(
    tmp: &Path,
    version: &str,
    target: &str,
    versions: &Path,
) -> anyhow::Result<()> {
    let archive = format!("tui-do-core-{target}.tar.gz");
    let base = release_base(version);
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
    let new = tmp.join(format!("tui-do-core-{target}")).join(BINARY);
    let dir = versions.join(version);
    std::fs::create_dir_all(&dir)?;
    let staged = dir.join(format!("{BINARY}.new"));
    std::fs::copy(&new, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    // Replacing a binary something may be running by renaming over it is fine on Unix
    std::fs::rename(&staged, dir.join(BINARY))?;
    Ok(())
}

/// A build that can't even say its version never gets switched to.
async fn check_runs(binary: &Path, version: &str) -> anyhow::Result<()> {
    let said = tokio::time::timeout(
        Duration::from_secs(10),
        run(&binary.to_string_lossy(), &["--version"]),
    )
    .await
    .context("the new version didn't start in time")?
    .map_err(|err| {
        // What it printed can be anything; it goes to the log, not to the app
        tracing::warn!(error = %format!("{err:#}"), "the new version failed its check");
        anyhow::anyhow!("the new version doesn't run on this machine")
    })?;
    if said.trim() != format!("{BINARY} {version}") {
        bail!("the download says it's {:?}", said.trim());
    }
    Ok(())
}

/// Start the old version's watcher, detached from this core.
fn start_watcher(
    link: &Path,
    previous: &Path,
    core_dir: &Path,
    version: &str,
    endpoint: &str,
) -> anyhow::Result<()> {
    let bin = link.parent().context("the link's folder")?;
    std::process::Command::new(bin.join(previous))
        .arg("watch-update")
        .arg(core_dir)
        .arg(previous)
        .arg(version)
        .arg(format!("{endpoint}/v0/health"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("starting the update's watcher")?;
    Ok(())
}

/// `tui-do-core watch-update`: keep the new version if it comes up and stays
/// up, otherwise switch back to `previous` and restart the service.
pub fn watch(
    core_dir: &Path,
    previous: &Path,
    version: &str,
    health_url: &str,
) -> anyhow::Result<()> {
    // Out of the core's process group, as a holder is
    unsafe { libc::setsid() };
    let root = root_of(core_dir)?;
    let started = Instant::now();
    let mut steady = 0;
    while started.elapsed() < WATCH_FOR || steady > 0 {
        std::thread::sleep(Duration::from_secs(1));
        steady = if reports_version(health_url, version) {
            steady + 1
        } else {
            0
        };
        if steady >= STEADY_CHECKS {
            prune_versions(root, previous);
            return Ok(());
        }
    }
    eprintln!("tui-do-core {version} didn't come up; going back to the previous version");
    point_link(&current_link(root), previous).context("switching back")?;
    std::fs::write(
        core_dir.join(ROLLBACK_FILE),
        json!({ "version": version, "at": chrono::Utc::now().to_rfc3339() }).to_string(),
    )?;
    restart_service()
}

fn reports_version(health_url: &str, version: &str) -> bool {
    let Ok(out) = std::process::Command::new("curl")
        .args(["-fsS", "--max-time", "2", health_url])
        .output()
    else {
        return false;
    };
    out.status.success()
        && serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .is_ok_and(|health| health["version"] == version)
}

/// Keep the current version and the one before it. A holder still running an
/// older one doesn't need its file.
fn prune_versions(root: &Path, previous: &Path) {
    let folder = |target: &Path| {
        target
            .parent()
            .and_then(Path::file_name)
            .map(ToOwned::to_owned)
    };
    let keep = [
        std::fs::read_link(current_link(root))
            .ok()
            .as_deref()
            .and_then(folder),
        folder(previous),
    ];
    let Ok(entries) = std::fs::read_dir(root.join("versions")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with('.') && !keep.iter().flatten().any(|k| *k == name) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn restart_service() -> anyhow::Result<()> {
    let status = if std::env::consts::OS == "macos" {
        let domain = format!("gui/{}/{}", unsafe { libc::getuid() }, launchd_label());
        std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &domain])
            .status()
    } else {
        // A service that kept crashing can have hit systemd's start limit
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "reset-failed", BINARY])
            .status();
        std::process::Command::new("systemctl")
            .args(["--user", "restart", BINARY])
            .status()
    }
    .context("restarting the service")?;
    if !status.success() {
        bail!("restarting the service failed: {status}");
    }
    Ok(())
}

/// The version last rolled back from, if the last update didn't take.
pub fn rolled_back(core_dir: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(core_dir.join(ROLLBACK_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Exit shortly (after the response has gone out), with every session's
/// screen saved for the next core; the service starts the new binary.
pub fn restart_soon(registry: Arc<Registry>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        registry.save_screens();
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_root() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tui-do-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bin")).expect("bin");
        std::fs::create_dir_all(root.join("core")).expect("core");
        let core_dir = root.join("core");
        (root, core_dir)
    }

    fn add_version(root: &Path, version: &str) {
        let dir = root.join("versions").join(version);
        std::fs::create_dir_all(&dir).expect("version");
        std::fs::write(dir.join(BINARY), version).expect("binary");
    }

    #[test]
    fn a_plain_binary_should_move_into_the_versions_folder() {
        let (root, core_dir) = install_root();
        let link = current_link(&root);
        std::fs::write(&link, "old build").expect("binary");

        use_versions_folder(&core_dir, "0.2.2").expect("moves");

        assert_eq!(
            std::fs::read_link(&link).expect("link"),
            link_target("0.2.2")
        );
        assert_eq!(std::fs::read_to_string(&link).expect("reads"), "old build");
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn a_link_should_be_left_alone() {
        let (root, core_dir) = install_root();
        add_version(&root, "0.3.0");
        let link = current_link(&root);
        point_link(&link, &link_target("0.3.0")).expect("links");

        use_versions_folder(&core_dir, "9.9.9").expect("does nothing");

        assert_eq!(
            std::fs::read_link(&link).expect("link"),
            link_target("0.3.0")
        );
        assert!(!root.join("versions/9.9.9").exists());
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn switching_should_replace_the_link_in_place() {
        let (root, _) = install_root();
        add_version(&root, "1.0.0");
        add_version(&root, "1.1.0");
        let link = current_link(&root);
        point_link(&link, &link_target("1.0.0")).expect("links");
        point_link(&link, &link_target("1.1.0")).expect("switches");
        assert_eq!(std::fs::read_to_string(&link).expect("reads"), "1.1.0");
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn pruning_should_keep_the_current_and_previous_versions() {
        let (root, _) = install_root();
        for version in ["1.0.0", "1.1.0", "1.2.0"] {
            add_version(&root, version);
        }
        std::fs::create_dir_all(root.join("versions/.update-x")).expect("in progress");
        let link = current_link(&root);
        point_link(&link, &link_target("1.2.0")).expect("links");

        prune_versions(&root, &link_target("1.1.0"));

        assert!(!root.join("versions/1.0.0").exists());
        assert!(root.join("versions/1.1.0").exists());
        assert!(root.join("versions/1.2.0").exists());
        assert!(root.join("versions/.update-x").exists());
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn the_service_should_get_kill_mode_once() {
        let unit = "[Unit]\nDescription=x\n\n[Service]\nRestart=always\n";
        let patched = with_kill_mode(unit).expect("adds it");
        assert_eq!(
            patched,
            "[Unit]\nDescription=x\n\n[Service]\nKillMode=process\nRestart=always\n"
        );
        assert!(with_kill_mode(&patched).is_none());
    }

    #[test]
    fn a_rollback_should_be_readable_until_cleared() {
        let (root, core_dir) = install_root();
        assert!(rolled_back(&core_dir).is_none());
        std::fs::write(core_dir.join(ROLLBACK_FILE), r#"{"version":"1.2.0"}"#).expect("writes");
        assert_eq!(rolled_back(&core_dir).expect("record")["version"], "1.2.0");
        std::fs::remove_dir_all(root).expect("cleans up");
    }
}
