//! TUI-DO core: a background service that owns terminal sessions and serves
//! them to clients (the Glaze app, iOS, web) over an HTTP + WebSocket API.
//!
//! Runtime files live in `$TUI_DO_CORE_DIR` (default `~/.tui-do/core`):
//! `token` (bearer token, 0600), `endpoint` (base URL clients connect to),
//! `config.json` (the workspace folder), `claude-hooks/` and
//! `task-sessions/`.
//!
//! `tui-do-core mcp` runs the TUI-DO MCP server on stdio for an agent to
//! launch; it forwards tool calls to the running core (see `mcp`).

use tui_do_core::config::ConfigStore;
use tui_do_core::fleet::Fleet;
use tui_do_core::handoff::Handoffs;
use tui_do_core::links::{SessionLink, TaskSessions};
use tui_do_core::mcp::TuiDoMcp;
use tui_do_core::relay::Relays;
use tui_do_core::workspace::shared::SharedWorkspace;
use tui_do_core::{api, hooks, net, session};

use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use rand::RngExt;

const DEFAULT_ADDR: &str = "127.0.0.1:47821";

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        Some("--version" | "-V") => {
            println!("tui-do-core {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command {other:?} (usage: tui-do-core [mcp])"),
    }
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
    let dir = core_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let token = load_token(&dir)?;
    let hooks = hooks::Hooks::prepare(&dir.join("claude-hooks"))?;
    let registry = Arc::new(session::Registry::new(hooks));
    let config = Arc::new(ConfigStore::load(dir.join("config.json"))?);
    let workspace = Arc::new(SharedWorkspace::load(
        Arc::clone(&config),
        registry.sender(),
    ));

    let addr = std::env::var("TUI_DO_CORE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("listening on {addr} (is another core running?)"))?;
    let local = listener.local_addr()?;
    std::fs::write(dir.join("endpoint"), format!("http://{local}"))?;
    tracing::info!(%local, dir = %dir.display(), "tui-do-core listening");
    // Look up the latest release now, so update checks have it
    let _ = tui_do_core::update::latest_known();

    let fleet = Arc::new(Fleet::load(dir.join("fleet.json"))?);
    let relays = Arc::new(Relays::new(registry.sender()));
    for device in fleet.devices() {
        relays.start(device);
    }
    let device = config.get();
    tracing::info!(id = %device.device_id, name = %device.device_name, "device");
    let app = api::router(api::AppState {
        registry: Arc::clone(&registry),
        config: Arc::clone(&config),
        fleet: Arc::clone(&fleet),
        relays: Arc::clone(&relays),
        handoffs: Arc::new(Handoffs::new(Arc::clone(&fleet), registry.sender(), &dir)),
        local_endpoint: format!("http://{local}").into(),
        workspace,
        sessions: Arc::new(TaskSessions::new(dir.join("task-sessions"))),
        token: token.into(),
        mcp_tokens: Arc::default(),
    });
    tokio::spawn(net::serve_tailscale(app.clone(), local.port(), config));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutting down, ending sessions");
    registry.kill_all();
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
