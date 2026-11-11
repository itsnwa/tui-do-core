//! Relays each fleet device's events into the home core's own event stream,
//! so a client watching `/v0/events` on the home core sees sessions on every
//! machine (as `deviceEvent`s) and whether each device is reachable
//! (`deviceStatus`). Input and resizes for a device's sessions go back over
//! the same connection, so keystrokes stay in order.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::fleet::Device;
use crate::session::Event;

const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often a device is pinged. A device that sleeps or changes network can
/// vanish without closing its socket, and only silence gives that away.
const PING_EVERY: Duration = Duration::from_secs(10);
/// A device that sends nothing for this long, not even a pong, is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);

type Sink = SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>;

struct Link {
    outgoing: mpsc::UnboundedSender<String>,
    task: AbortHandle,
}

pub struct Relays {
    links: Mutex<HashMap<String, Link>>,
    events: broadcast::Sender<Event>,
}

impl Relays {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            links: Mutex::new(HashMap::new()),
            events,
        }
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start (or restart) relaying a device's events.
    pub fn start(&self, device: Device) {
        let (outgoing, rx) = mpsc::unbounded_channel();
        let id = device.id.clone();
        let task = tokio::spawn(relay(device, self.events.clone(), rx)).abort_handle();
        if let Some(old) = self.links().insert(id, Link { outgoing, task }) {
            old.task.abort();
        }
    }

    pub fn stop(&self, id: &str) {
        if let Some(link) = self.links().remove(id) {
            link.task.abort();
        }
    }

    /// Send a client message (`input`, `resize`) to a device's event socket.
    pub fn send(&self, id: &str, message: String) -> bool {
        self.links()
            .get(id)
            .is_some_and(|link| link.outgoing.send(message).is_ok())
    }
}

async fn relay(
    device: Device,
    events: broadcast::Sender<Event>,
    mut outgoing: mpsc::UnboundedReceiver<String>,
) {
    // Clients attach to the sessions they show; the home only needs to know
    // when a device's shell has started printing
    let url = format!(
        "{}/v0/events?output=first&token={}",
        device.address.replacen("http", "ws", 1),
        device.token
    );
    let mut retry = RETRY_MIN;
    loop {
        let socket = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((socket, _)) => socket,
            Err(err) => {
                tracing::debug!(device = %device.name, %err, "device events unreachable");
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(RETRY_MAX);
                continue;
            }
        };
        retry = RETRY_MIN;
        tracing::info!(device = %device.name, "relaying device events");
        let _ = events.send(Event::DeviceStatus {
            device: device.id.clone(),
            online: true,
        });
        // Anything typed while the device was unreachable is stale
        while outgoing.try_recv().is_ok() {}

        let (mut tx, mut rx) = socket.split();
        let mut ping = tokio::time::interval(PING_EVERY);
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                msg = rx.next() => {
                    heard = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(event) = serde_json::from_str::<serde_json::Value>(text.as_str()) {
                                let _ = events.send(Event::DeviceEvent { device: device.id.clone(), event });
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    }
                },
                message = outgoing.recv() => match message {
                    Some(text) => {
                        if !send(&mut tx, Message::text(text)).await {
                            break;
                        }
                    }
                    None => return,
                },
                _ = ping.tick() => {
                    if heard.elapsed() >= SILENCE_LIMIT || !send(&mut tx, Message::Ping(Default::default())).await {
                        break;
                    }
                },
            }
        }
        tracing::info!(device = %device.name, "device events disconnected");
        let _ = events.send(Event::DeviceStatus {
            device: device.id.clone(),
            online: false,
        });
        tokio::time::sleep(RETRY_MIN).await;
    }
}

/// Send to a device; false if that failed or a dead connection held it up.
async fn send(tx: &mut Sink, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SILENCE_LIMIT, tx.send(message)).await,
        Ok(Ok(()))
    )
}
