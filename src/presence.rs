//! Whether someone is at a Mac, so the phone stays quiet while they are.
//!
//! A macOS core reads how long the Mac's keyboard, mouse and trackpad have
//! been idle. While someone used it in the last [`AWAY`], it says so with an
//! `Event::Presence` every [`HEARTBEAT`], and once more when they leave. The
//! home hears its own and, relayed, every machine's ([`Here`]), and counts
//! someone as there while a machine said so in the last [`FRESH`]: a machine
//! that goes quiet or offline doesn't keep the phone quiet.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::session::Event;

/// Idle this long, and nobody is at the Mac.
pub const AWAY: Duration = Duration::from_secs(3 * 60);
const POLL: Duration = Duration::from_secs(10);
/// How often a Mac with someone at it says so again.
const HEARTBEAT: Duration = Duration::from_secs(60);
/// How long a machine's word that someone is there holds.
const FRESH: Duration = Duration::from_secs(90);

/// How long the Mac has been idle; none where that can't be told.
async fn idle() -> Option<Duration> {
    let out = tokio::process::Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4", "-k", "HIDIdleTime"])
        .output()
        .await
        .ok()?;
    parse_idle(&String::from_utf8_lossy(&out.stdout))
}

/// `"HIDIdleTime" = <nanoseconds>` in `ioreg`'s output.
fn parse_idle(text: &str) -> Option<Duration> {
    text.lines().find_map(|line| {
        let (_, value) = line.split_once("\"HIDIdleTime\" = ")?;
        value.trim().parse().ok().map(Duration::from_nanos)
    })
}

/// On a Mac, say whether someone is at it.
pub fn spawn(events: broadcast::Sender<Event>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    tokio::spawn(async move {
        let mut said: Option<(bool, Instant)> = None;
        let mut poll = tokio::time::interval(POLL);
        loop {
            poll.tick().await;
            let Some(idle) = idle().await else { continue };
            let here = idle < AWAY;
            let now = Instant::now();
            let due = match said {
                None => true,
                Some((was, at)) => was != here || (here && now - at >= HEARTBEAT),
            };
            if due {
                let _ = events.send(Event::Presence { here });
                said = Some((here, now));
            }
        }
    });
}

/// The machines someone is at, by when each last said so.
#[derive(Default)]
pub struct Here {
    machines: HashMap<String, Instant>,
}

impl Here {
    /// Take in what an event says, given this core's own device id.
    pub fn see(&mut self, event: &Event, own_id: &str, now: Instant) {
        match event {
            Event::Presence { here } => self.set(own_id, *here, now),
            Event::DeviceEvent { device, event } if event["type"] == "presence" => {
                if let Some(here) = event["here"].as_bool() {
                    self.set(device, here, now);
                }
            }
            Event::DeviceStatus {
                device,
                online: false,
            } => {
                self.machines.remove(device);
            }
            _ => {}
        }
    }

    fn set(&mut self, device: &str, here: bool, now: Instant) {
        if here {
            self.machines.insert(device.to_string(), now);
        } else {
            self.machines.remove(device);
        }
    }

    /// Someone is at a machine.
    pub fn anyone(&self, now: Instant) -> bool {
        self.machines.values().any(|at| now < *at + FRESH)
    }

    /// When nobody will be, unless a machine says so again.
    pub fn until(&self, now: Instant) -> Option<Instant> {
        self.machines
            .values()
            .map(|at| *at + FRESH)
            .filter(|end| *end > now)
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_should_be_read_from_ioreg() {
        let out = "+-o IOHIDSystem  <class IOHIDSystem>\n    | |   \"HIDIdleTime\" = 1347755375\n";
        assert_eq!(parse_idle(out), Some(Duration::from_nanos(1_347_755_375)));
        assert_eq!(parse_idle("nothing here"), None);
    }

    fn relayed(device: &str, here: bool) -> Event {
        Event::DeviceEvent {
            device: device.into(),
            event: serde_json::json!({ "type": "presence", "here": here }),
        }
    }

    #[test]
    fn someone_should_be_here_while_a_machine_says_so() {
        let start = Instant::now();
        let mut here = Here::default();
        assert!(!here.anyone(start));
        here.see(&relayed("mbp", true), "cube", start);
        here.see(&Event::Presence { here: true }, "cube", start);
        assert!(here.anyone(start));
        here.see(&relayed("mbp", false), "cube", start);
        assert!(here.anyone(start));
        here.see(&Event::Presence { here: false }, "cube", start);
        assert!(!here.anyone(start));
        assert_eq!(here.until(start), None);
    }

    #[test]
    fn a_machine_that_goes_quiet_or_offline_should_not_count() {
        let start = Instant::now();
        let mut here = Here::default();
        here.see(&relayed("mbp", true), "cube", start);
        assert_eq!(here.until(start), Some(start + FRESH));
        assert!(!here.anyone(start + FRESH));
        here.see(&relayed("mbp", true), "cube", start);
        let offline = Event::DeviceStatus {
            device: "mbp".into(),
            online: false,
        };
        here.see(&offline, "cube", start);
        assert!(!here.anyone(start));
    }
}
