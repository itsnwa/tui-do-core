//! TUI-DO core: a background service that owns terminal sessions and serves
//! them to clients (the Glaze app, iOS, web) over an HTTP + WebSocket API.
//!
//! Runtime files live in `$TUI_DO_CORE_DIR` (default `~/.tui-do/core`):
//! `token` (bearer token, 0600), `endpoint` (base URL clients connect to),
//! `config.json` (the workspace folder, the home link), `fleet.json`,
//! `clients.json` (paired clients), `handoffs/`, `claude-hooks/` and
//! `task-sessions/`. A runner keeps its copy of the workspace in
//! `~/.tui-do/workspace` unless a folder is configured.
//!
//! `tui-do-core mcp` runs the TUI-DO MCP server on stdio for an agent to
//! launch; it forwards tool calls to the running core (see `mcp`).
//! `tui-do-core hold` is what the core starts to keep a session's program
//! running (see `holder`); sockets for those are in `holders/`.

use tui_do_core::clients::Clients;
use tui_do_core::config::ConfigStore;
use tui_do_core::fleet::Fleet;
use tui_do_core::handoff::Handoffs;
use tui_do_core::links::{SessionLink, TaskSessions, is_codex_conversation};
use tui_do_core::mcp::TuiDoMcp;
use tui_do_core::relay::Relays;
use tui_do_core::replica::Replicas;
use tui_do_core::workspace::shared::SharedWorkspace;
use tui_do_core::{api, holder, hooks, net, session, update};

use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use rand::RngExt;

const DEFAULT_ADDR: &str = "127.0.0.1:47821";
/// Where each task's Codex session on this machine is recorded.
const CODEX_SESSIONS_DIR: &str = "codex-sessions";
/// launchd starts services with a limit of only 256 open files.
const OPEN_FILES: u64 = 10_240;
/// How long requests in progress get to finish when the core stops.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn core_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("TUI_DO_CORE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".tui-do").join("core"))
}

/// Read the token, creating a random one (readable only by this user) on first run.
fn load_token(dir: &Path) -> anyhow::Result<String> {
    let path = dir.join("token");
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    std::fs::write(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(token)
}

fn main() -> anyhow::Result<()> {
    // These two outlive the core that starts them, and stay out of the runtime
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    match args.first().and_then(|command| command.to_str()) {
        Some("hold") => {
            let [_, socket] = &args[..] else {
                anyhow::bail!("usage: tui-do-core hold <socket>");
            };
            holder::run(socket)
        }
        Some("watch-update") => {
            let [_, core_dir, previous, version, health] = &args[..] else {
                anyhow::bail!("usage: tui-do-core watch-update <dir> <previous> <version> <url>");
            };
            update::watch(
                core_dir,
                previous,
                &version.to_string_lossy(),
                &health.to_string_lossy(),
            )
        }
        _ => run(),
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    // Logs go to stderr: in `mcp` mode stdout carries the protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tui_do_core=info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        None => serve().await,
        Some("mcp") => serve_mcp_stdio().await,
        Some("codex-notify") => codex_notify(&std::env::args().nth(2).unwrap_or_default()),
        Some("apns-key") => apns_key(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("--version" | "-V") => {
            println!("tui-do-core {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => anyhow::bail!(
            "unknown command {other:?} (usage: tui-do-core [mcp | apns-key <AuthKey_KEYID.p8> <team id> [key id]])"
        ),
    }
}

/// `tui-do-core codex-notify <json>`: Codex's `notify` command in the sessions
/// TUI-DO starts. Links the task to the Codex session and reports that it's
/// waiting. Codex's session ids only mean something on this machine, so the
/// link stays in this core's folder.
fn codex_notify(payload: &str) -> anyhow::Result<()> {
    let Some(link) = SessionLink::of_codex_turn(payload) else {
        return Ok(());
    };
    let codex_home = match std::env::var_os("CODEX_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".codex"),
    };
    if !is_codex_conversation(&codex_home, &link.session_id) {
        return Ok(());
    }
    let task = std::env::var(hooks::SESSION_ENV).context("not in a TUI-DO session")?;
    TaskSessions::new(core_dir()?.join(CODEX_SESSIONS_DIR)).set(&task, &link)?;
    if let Some(state_file) = std::env::var_os(hooks::STATE_FILE_ENV) {
        let state_file = PathBuf::from(state_file);
        std::fs::write(hooks::detail_file(&state_file), payload)?;
        std::fs::write(state_file, "waiting")?;
    }
    Ok(())
}

/// `tui-do-core apns-key <AuthKey_KEYID.p8> <team id> [key id]`: keep the
/// APNs auth key phone notifications are sent with. Only the home sends
/// them; the key moves with the home.
fn apns_key(args: &[String]) -> anyhow::Result<()> {
    let [p8, team_id, rest @ ..] = args else {
        anyhow::bail!("usage: tui-do-core apns-key <AuthKey_KEYID.p8> <team id> [key id]");
    };
    let key =
        tui_do_core::apns::from_file(Path::new(p8), team_id, rest.first().map(String::as_str))?;
    let dir = core_dir()?;
    tui_do_core::apns::save(&dir, &key)?;
    println!(
        "Saved APNs key {} for team {} in {}. Phones can turn notifications on now.",
        key.key_id,
        key.team_id,
        dir.join("apns.json").display()
    );
    Ok(())
}

/// `tui-do-core mcp`: an MCP stdio server that forwards to the running core.
async fn serve_mcp_stdio() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let dir = core_dir()?;
    let token = std::fs::read_to_string(dir.join("token"))
        .context("tui-do-core isn't set up (no token); start the core first")?;
    let endpoint = std::fs::read_to_string(dir.join("endpoint"))
        .unwrap_or_else(|_| format!("http://{DEFAULT_ADDR}"));
    let server = TuiDoMcp::forwarding(
        endpoint.trim().to_string(),
        token.trim().to_string(),
        SessionLink::of_parent_claude(),
    );
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

async fn serve() -> anyhow::Result<()> {
    if let Err(err) = rlimit::increase_nofile_limit(OPEN_FILES) {
        tracing::warn!(%err, "couldn't raise the open files limit");
    }
    let dir = core_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let token = load_token(&dir)?;
    if update::restarts_itself() {
        if let Err(err) = update::use_versions_folder(&dir, env!("CARGO_PKG_VERSION")) {
            tracing::warn!(error = %format!("{err:#}"), "couldn't set up the versions folder");
        }
        if let Err(err) = update::keep_holders_through_restarts().await {
            tracing::warn!(error = %format!("{err:#}"), "sessions won't survive a service restart");
        }
    }
    let hooks = hooks::Hooks::prepare(&dir.join("claude-hooks"))?;
    let exe = update::own_path(&dir)?;
    let registry = Arc::new(session::Registry::new(hooks, dir.join("holders"), exe)?);
    let config = Arc::new(ConfigStore::load(dir.join("config.json"))?);
    let default_workspace = dir
        .parent()
        .map_or_else(|| dir.join("workspace"), |p| p.join("workspace"));
    let workspace = Arc::new(SharedWorkspace::load(
        Arc::clone(&config),
        registry.sender(),
        default_workspace,
    ));

    let addr = std::env::var("TUI_DO_CORE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("listening on {addr} (is another core running?)"))?;
    let local = listener.local_addr()?;
    std::fs::write(dir.join("endpoint"), format!("http://{local}"))?;
    tracing::info!(%local, dir = %dir.display(), "tui-do-core listening");
    // After taking the port, so a second core can't take these from the first
    let adopted = registry.adopt();
    if adopted > 0 {
        tracing::info!(adopted, "picked up running sessions");
    }
    // Look up the latest release now, so update checks have it
    let _ = update::latest_known();

    let fleet = Arc::new(Fleet::load(dir.join("fleet.json"))?);
    let relays = Arc::new(Relays::new(registry.sender()));
    let device = config.get();
    tracing::info!(id = %device.device_id, name = %device.device_name, "device");
    let state = api::AppState {
        registry: Arc::clone(&registry),
        config: Arc::clone(&config),
        fleet: Arc::clone(&fleet),
        relays: Arc::clone(&relays),
        handoffs: Arc::new(Handoffs::new(
            Arc::clone(&fleet),
            tui_do_core::fleet::Device {
                id: device.device_id.clone(),
                name: device.device_name.clone(),
                address: format!("http://{local}"),
                token: token.clone(),
                platform: String::new(),
                added_at: String::new(),
                ssh_host: None,
                mcp_token: String::new(),
            },
            registry.sender(),
            &dir,
        )),
        local_endpoint: format!("http://{local}").into(),
        workspace,
        sessions: Arc::new(TaskSessions::new(dir.join("task-sessions"))),
        codex_sessions: Arc::new(TaskSessions::new(dir.join(CODEX_SESSIONS_DIR))),
        token: token.into(),
        clients: Arc::new(Clients::load(dir.join("clients.json"))?),
        replicas: Arc::new(Replicas::default()),
        core_dir: dir.clone().into(),
        moving: Arc::default(),
        linking: Arc::default(),
        app_tokens: Arc::default(),
        watching: Arc::default(),
    };
    // Devices are linked and given the workspace as their relays come online
    api::spawn_background(state.clone());
    api::spawn_notifier(state.clone());
    for device in fleet.devices() {
        relays.start(device);
    }
    let app = api::router(state);
    tokio::spawn(net::serve_tailscale(app.clone(), local.port(), config));
    let stopping = async move {
        shutdown_signal().await;
        tracing::info!("shutting down; sessions keep running");
        let _ = tokio::task::spawn_blocking(move || registry.save_screens()).await;
        // Open WebSockets never finish by themselves
        tokio::spawn(async {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            std::process::exit(0);
        });
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(stopping)
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
