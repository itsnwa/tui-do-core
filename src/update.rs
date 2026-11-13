//! Updating this core from a GitHub release: download the build for this
//! machine, check its checksum, switch to it, then exit so the service
//! (launchd's KeepAlive, systemd's `Restart=always`) starts the new one.
//! Sessions keep running: their programs are in holders (see `holder`), which
//! the new core picks up.
//!
//! The current version is the file `~/.tui-do/bin/tui-do-core`, the path the
//! services and the agents' MCP settings use, and the one before it waits
//! beside it until the new one has come up. It used to be a link to
//! `~/.tui-do/versions/<version>/`, but macOS keeps privacy grants (Files &
//! Folders) for a program outside an app by the path it runs from, so every
//! version asked again and added another "tui-do-core" to the Privacy list. A new version that doesn't come up can't be trusted to undo
//! itself, so a watcher from the old version stays behind, and switches back
//! if the new one doesn't report healthy.

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

/// The version in a release page's address (`…/releases/tag/v1.2.3`).
fn version_in_url(url: &str) -> Option<&str> {
    let (_, tag) = url.trim().rsplit_once("/releases/tag/")?;
    let version = tag.trim_start_matches('v');
    valid_version(version).then_some(version)
}

/// The newest release's version (without the `v`), cached for a while.
pub async fn latest() -> anyhow::Result<String> {
    if let Ok(cache) = LATEST.lock()
        && let Some((at, version)) = cache.as_ref()
        && at.elapsed() < LATEST_TTL
    {
        return Ok(version.clone());
    }
    // The page redirects to the newest release. GitHub's API would say the
    // same, but allows a connection only 60 anonymous requests an hour.
    let url = run(
        "curl",
        &[
            "-fsSIL",
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            &format!("https://github.com/{REPO}/releases/latest"),
        ],
    )
    .await
    .context("couldn't ask GitHub for the latest release")?;
    let Some(version) = version_in_url(&url).map(str::to_string) else {
        bail!("unexpected release page {url:?}");
    };
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

fn current_binary(root: &Path) -> PathBuf {
    root.join("bin").join(BINARY)
}

fn previous_binary(root: &Path) -> PathBuf {
    root.join("bin").join(format!("{BINARY}.previous"))
}

/// Copy `from` (or what it links to) over `to`, in one step.
fn copy_into_place(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut staged = to.as_os_str().to_owned();
    staged.push(".new");
    let staged = PathBuf::from(staged);
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(from, &staged)?;
    std::fs::rename(&staged, to)
}

/// Cores before 0.3.31 left a link to the version's folder at the current
/// binary's path. Putting the file itself there doesn't disturb the running
/// core, and the service starts the next one from the path that stays.
pub fn settle_binary(core_dir: &Path) -> anyhow::Result<()> {
    let binary = current_binary(root_of(core_dir)?);
    let Ok(meta) = std::fs::symlink_metadata(&binary) else {
        return Ok(());
    };
    if !meta.file_type().is_symlink() {
        return Ok(());
    }
    // The folder stays until the next update: the watcher of the version
    // that installed this one may still switch back to its own
    copy_into_place(&binary, &binary)?;
    tracing::info!("the current version is now a file instead of a link");
    Ok(())
}

/// This program's path, for starting it again later. An installed core uses
/// the current binary's: `current_exe` can give the file it started from,
/// which a later update replaces or deletes.
pub fn own_path(core_dir: &Path) -> anyhow::Result<PathBuf> {
    if restarts_itself() {
        let binary = current_binary(root_of(core_dir)?);
        if binary.exists() {
            return Ok(binary);
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
    // Beside the binary, so the final rename stays on one filesystem
    let tmp = root
        .join("bin")
        .join(format!(".update-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    let result = download_and_switch(&tmp, version, target, root).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result?;

    let previous = previous_binary(root);
    if let Err(err) = start_watcher(&previous, core_dir, version, endpoint) {
        let _ = switch_back(root, &previous);
        return Err(err);
    }
    tracing::info!(version, "updated; restarting");
    Ok(())
}

async fn download_and_switch(
    tmp: &Path,
    version: &str,
    target: &str,
    root: &Path,
) -> anyhow::Result<()> {
    let new = download_into(tmp, version, target).await?;
    check_runs(&new, version).await?;
    switch_to(root, &new).context("switching to the new version")
}

/// Make `new` the current binary, keeping the current one as the previous.
/// Renaming over a binary something may be running is fine on Unix.
fn switch_to(root: &Path, new: &Path) -> std::io::Result<()> {
    let current = current_binary(root);
    copy_into_place(&current, &previous_binary(root))?;
    std::fs::rename(new, current)
}

fn switch_back(root: &Path, previous: &Path) -> std::io::Result<()> {
    std::fs::rename(previous, current_binary(root))
}

/// Where releases are downloaded from; another base lets updates be tried
/// against a local folder.
fn release_base(version: &str) -> String {
    let base = std::env::var("TUI_DO_RELEASES_URL")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}/releases/download"));
    format!("{base}/v{version}")
}

/// Download and unpack `version` in `tmp`, and return the binary's path.
async fn download_into(tmp: &Path, version: &str, target: &str) -> anyhow::Result<PathBuf> {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(new)
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
    previous: &Path,
    core_dir: &Path,
    version: &str,
    endpoint: &str,
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(previous)
        // Out of the core's process group from the start, which launchd ends
        // with the core: the first run of a new copy can take longer to get
        // going than the core takes to exit
        .process_group(0)
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
            let _ = std::fs::remove_file(previous);
            prune_versions(root);
            return Ok(());
        }
    }
    eprintln!("tui-do-core {version} didn't come up; going back to the previous version");
    switch_back(root, previous).context("switching back")?;
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

/// Remove the versions folders cores before 0.3.31 left, except the one the
/// current binary links to: going back to one of those cores makes it a link
/// again. A holder still running an older version doesn't need its file.
fn prune_versions(root: &Path) {
    let keep = std::fs::read_link(current_binary(root))
        .ok()
        .and_then(|target| target.parent()?.file_name().map(ToOwned::to_owned));
    let versions = root.join("versions");
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with('.') && keep.as_ref() != Some(&name) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    // Only goes once it's empty
    let _ = std::fs::remove_dir(versions);
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
    fn the_version_should_come_from_the_release_page_address() {
        let tag = "https://github.com/itsnwa/tui-do-core/releases/tag/v0.3.14";
        assert_eq!(version_in_url(tag), Some("0.3.14"));
        // No releases yet: GitHub redirects to the list
        let list = "https://github.com/itsnwa/tui-do-core/releases";
        assert_eq!(version_in_url(list), None);
        let odd = "https://github.com/itsnwa/tui-do-core/releases/tag/bad%20tag";
        assert_eq!(version_in_url(odd), None);
    }

    /// The current binary as cores before 0.3.31 left it: a link to `version`.
    fn link_to(root: &Path, version: &str) {
        let target = Path::new("../versions").join(version).join(BINARY);
        std::os::unix::fs::symlink(target, current_binary(root)).expect("links");
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("reads")
    }

    #[test]
    fn a_link_should_become_the_file_it_points_at() {
        let (root, core_dir) = install_root();
        add_version(&root, "0.3.30");
        link_to(&root, "0.3.30");

        settle_binary(&core_dir).expect("settles");

        let binary = current_binary(&root);
        assert!(std::fs::symlink_metadata(&binary).expect("meta").is_file());
        assert_eq!(read(&binary), "0.3.30");
        // The installing version's watcher may still switch back to its link
        assert!(root.join("versions/0.3.30").exists());
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn a_binary_in_place_should_be_left_alone() {
        let (root, core_dir) = install_root();
        std::fs::write(current_binary(&root), "build").expect("binary");

        settle_binary(&core_dir).expect("does nothing");

        assert_eq!(read(&current_binary(&root)), "build");
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn switching_should_keep_the_previous_binary_to_go_back_to() {
        let (root, _) = install_root();
        let new = root.join("bin/.update-x/tui-do-core");
        std::fs::create_dir_all(new.parent().expect("folder")).expect("folder");
        std::fs::write(current_binary(&root), "1.0.0").expect("binary");
        std::fs::write(&new, "1.1.0").expect("download");

        switch_to(&root, &new).expect("switches");
        assert_eq!(read(&current_binary(&root)), "1.1.0");
        assert_eq!(read(&previous_binary(&root)), "1.0.0");

        switch_back(&root, &previous_binary(&root)).expect("switches back");
        assert_eq!(read(&current_binary(&root)), "1.0.0");
        assert!(!previous_binary(&root).exists());
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn switching_from_a_link_should_put_a_file_in_its_place() {
        let (root, _) = install_root();
        add_version(&root, "1.0.0");
        link_to(&root, "1.0.0");
        let new = root.join("bin/.update-x/tui-do-core");
        std::fs::create_dir_all(new.parent().expect("folder")).expect("folder");
        std::fs::write(&new, "1.1.0").expect("download");

        switch_to(&root, &new).expect("switches");

        let binary = current_binary(&root);
        assert!(std::fs::symlink_metadata(&binary).expect("meta").is_file());
        assert_eq!(read(&binary), "1.1.0");
        assert_eq!(read(&previous_binary(&root)), "1.0.0");
        assert_eq!(read(&root.join("versions/1.0.0/tui-do-core")), "1.0.0");
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn pruning_should_remove_the_versions_folders() {
        let (root, _) = install_root();
        for version in ["1.0.0", "1.1.0"] {
            add_version(&root, version);
        }
        std::fs::create_dir_all(root.join("versions/.update-x")).expect("in progress");
        std::fs::write(current_binary(&root), "1.2.0").expect("binary");

        prune_versions(&root);

        assert!(!root.join("versions/1.0.0").exists());
        assert!(!root.join("versions/1.1.0").exists());
        assert!(root.join("versions/.update-x").exists());

        std::fs::remove_dir(root.join("versions/.update-x")).expect("removes");
        prune_versions(&root);
        assert!(!root.join("versions").exists());
        std::fs::remove_dir_all(root).expect("cleans up");
    }

    #[test]
    fn pruning_should_keep_the_version_an_older_core_links_to() {
        let (root, _) = install_root();
        for version in ["1.0.0", "1.1.0"] {
            add_version(&root, version);
        }
        link_to(&root, "1.0.0");

        prune_versions(&root);

        assert!(root.join("versions/1.0.0").exists());
        assert!(!root.join("versions/1.1.0").exists());
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
