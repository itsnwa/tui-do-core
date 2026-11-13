//! Where the core listens. Always on loopback (for local clients); with
//! remote access on, also on the machine's Tailscale address, so other
//! devices in the fleet can reach it. The core never listens on any other
//! interface.
//!
//! Whether it's listening there is kept as a [`Tailscale`] state, which
//! `/v0/health` and `/v0/device` report and a `tailscaleStatus` event
//! announces, so the apps can say when Tailscale is off on this machine.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::config::ConfigStore;
use crate::session::Event;

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

/// Whether the core is listening on Tailscale, and if not, why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Tailscale {
    /// The core has only just started.
    Starting,
    /// Remote access is off, so it doesn't listen on Tailscale.
    Off,
    /// No Tailscale address: Tailscale isn't installed or isn't running.
    NotRunning,
    /// Tailscale has an address, but it isn't up on this machine (it's
    /// stopped or logged out).
    Disconnected {
        address: String,
    },
    /// Couldn't listen for another reason.
    Failed {
        address: String,
        error: String,
    },
    Listening {
        address: String,
    },
}

impl Tailscale {
    /// What a failed bind on `addr` says about Tailscale.
    fn bind_failed(addr: SocketAddr, err: &std::io::Error) -> Self {
        let address = addr.to_string();
        if err.kind() == ErrorKind::AddrNotAvailable {
            Self::Disconnected { address }
        } else {
            Self::Failed {
                address,
                error: err.to_string(),
            }
        }
    }
}

/// The current [`Tailscale`] state; a change is sent as an event.
pub struct TailscaleStatus {
    current: Mutex<Tailscale>,
    events: broadcast::Sender<Event>,
}

impl TailscaleStatus {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            current: Mutex::new(Tailscale::Starting),
            events,
        }
    }

    pub fn get(&self) -> Tailscale {
        self.current.lock().unwrap().clone()
    }

    fn set(&self, state: Tailscale) {
        let mut current = self.current.lock().unwrap();
        if *current == state {
            return;
        }
        *current = state.clone();
        drop(current);
        let _ = self
            .events
            .send(Event::TailscaleStatus { tailscale: state });
    }
}

/// Whether `ip` is still up on this machine: binding a spare port on it
/// fails once Tailscale stops, even though `tailscale ip` still prints it.
fn address_up(ip: IpAddr) -> Result<(), std::io::Error> {
    std::net::TcpListener::bind(SocketAddr::new(ip, 0)).map(drop)
}

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
/// for it to be turned on and for Tailscale to come up. Stops while
/// Tailscale is down, and listens again once it's back.
pub async fn serve_tailscale(
    app: axum::Router,
    port: u16,
    config: Arc<ConfigStore>,
    status: Arc<TailscaleStatus>,
) {
    loop {
        if !config.get().remote_access {
            status.set(Tailscale::Off);
            tokio::time::sleep(SETTING_CHECK).await;
            continue;
        }
        let Some(ip) = tailscale_ipv4().await else {
            status.set(Tailscale::NotRunning);
            tokio::time::sleep(TAILSCALE_RETRY).await;
            continue;
        };
        let addr = SocketAddr::new(ip, port);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                let state = Tailscale::bind_failed(addr, &err);
                if status.get() != state {
                    tracing::warn!(%addr, %err, "couldn't listen on Tailscale");
                }
                status.set(state);
                tokio::time::sleep(TAILSCALE_RETRY).await;
                continue;
            }
        };
        tracing::info!(%addr, "listening on Tailscale");
        status.set(Tailscale::Listening {
            address: addr.to_string(),
        });
        // Stop as soon as remote access is turned off, or Tailscale goes down
        let watched = Arc::clone(&config);
        let turned_off = async move {
            while watched.get().remote_access {
                tokio::time::sleep(SETTING_CHECK).await;
            }
        };
        let gone_down = async move {
            loop {
                tokio::time::sleep(TAILSCALE_RETRY).await;
                if let Err(err) = address_up(ip) {
                    return err;
                }
            }
        };
        // Why it stopped: Tailscale went down, or (none) remote access was turned off
        let down = Arc::new(Mutex::new(None));
        let reason = Arc::clone(&down);
        let stopping = async move {
            tokio::select! {
                () = turned_off => {}
                err = gone_down => *reason.lock().unwrap() = Some(err),
            }
        };
        let served = axum::serve(listener, app.clone())
            .with_graceful_shutdown(stopping)
            .await;
        let down = down.lock().unwrap().take();
        match (served, down) {
            (Err(err), _) => tracing::warn!(%err, "Tailscale listener stopped"),
            (Ok(()), None) => tracing::info!(%addr, "stopped listening on Tailscale"),
            (Ok(()), Some(err)) => {
                tracing::warn!(%addr, %err, "Tailscale went down; stopped listening on it");
                status.set(Tailscale::bind_failed(addr, &err));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_address_means_tailscale_is_disconnected() {
        let addr: SocketAddr = "100.91.153.110:47821".parse().unwrap();
        let gone = std::io::Error::from(ErrorKind::AddrNotAvailable);
        assert_eq!(
            Tailscale::bind_failed(addr, &gone),
            Tailscale::Disconnected {
                address: addr.to_string()
            }
        );
        let taken = std::io::Error::from(ErrorKind::AddrInUse);
        assert!(matches!(
            Tailscale::bind_failed(addr, &taken),
            Tailscale::Failed { .. }
        ));
    }

    #[test]
    fn a_change_is_announced_once() {
        let (events, mut rx) = broadcast::channel(8);
        let status = TailscaleStatus::new(events);
        status.set(Tailscale::NotRunning);
        status.set(Tailscale::NotRunning);
        let event = serde_json::to_value(rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            event,
            serde_json::json!({ "type": "tailscaleStatus", "tailscale": { "state": "notRunning" } })
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(status.get(), Tailscale::NotRunning);
    }
}
