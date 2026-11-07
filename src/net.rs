//! Where the core listens. Always on loopback (for local clients); with
//! remote access on, also on the machine's Tailscale address, so other
//! devices in the fleet can reach it. The core never listens on any other
//! interface.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crate::config::ConfigStore;

/// Where the `tailscale` CLI lives on macOS and Linux.
const TAILSCALE_CLIS: [&str; 4] = [
    "tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];
/// How often to check whether remote access was turned on or off.
const SETTING_CHECK: Duration = Duration::from_secs(2);
/// How often to look for Tailscale again when it isn't running.
const TAILSCALE_RETRY: Duration = Duration::from_secs(30);

/// This machine's Tailscale IPv4 address, if Tailscale is running.
pub async fn tailscale_ipv4() -> Option<IpAddr> {
    for cli in TAILSCALE_CLIS {
        let Ok(out) = tokio::process::Command::new(cli)
            .args(["ip", "-4"])
            .output()
            .await
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Some(ip) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| line.trim().parse::<IpAddr>().ok())
        {
            return Some(ip);
        }
    }
    None
}

/// Serve `app` on the Tailscale address while remote access is on, waiting
/// for it to be turned on and for Tailscale to come up.
pub async fn serve_tailscale(app: axum::Router, port: u16, config: Arc<ConfigStore>) {
    loop {
        if !config.get().remote_access {
            tokio::time::sleep(SETTING_CHECK).await;
            continue;
        }
        let Some(ip) = tailscale_ipv4().await else {
            tokio::time::sleep(TAILSCALE_RETRY).await;
            continue;
        };
        let addr = SocketAddr::new(ip, port);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!(%addr, %err, "couldn't listen on Tailscale");
                tokio::time::sleep(TAILSCALE_RETRY).await;
                continue;
            }
        };
        tracing::info!(%addr, "listening on Tailscale");
        // Stop as soon as remote access is turned off
        let watched = Arc::clone(&config);
        let turned_off = async move {
            while watched.get().remote_access {
                tokio::time::sleep(SETTING_CHECK).await;
            }
        };
        match axum::serve(listener, app.clone())
            .with_graceful_shutdown(turned_off)
            .await
        {
            Ok(()) => tracing::info!(%addr, "stopped listening on Tailscale"),
            Err(err) => tracing::warn!(%err, "Tailscale listener stopped"),
        }
    }
}
