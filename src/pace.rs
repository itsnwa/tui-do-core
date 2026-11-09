//! Holds a viewer's terminal output back to what its connection can take.
//!
//! Output goes out as frames, and the client acknowledges each one. Only
//! `WINDOW` bytes may be unacknowledged at a time, so a slow link can't build
//! a long queue that delays everything behind it. While the window is full,
//! output collects here. Once it's more than `MAX_BEHIND` old, the viewer is
//! better off with the current screen than with how the screen got there.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Unacknowledged bytes a viewer may have. 1.6 MB/s at 40 ms, 130 KB/s at 500 ms.
const WINDOW: usize = 64 * 1024;
/// The most output in one frame.
const FRAME: usize = 16 * 1024;
/// How old waiting output may get before the viewer gets a repaint instead.
const MAX_BEHIND: Duration = Duration::from_secs(1);
/// The most output kept for a viewer that has stopped acknowledging.
const MAX_WAITING: usize = 1024 * 1024;

/// What a paced connection should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing can go out until more output or an acknowledgement arrives.
    Wait,
    Send(String),
    /// Output was given up on: send the current screen, and report it with `sent`.
    Repaint,
}

#[derive(Default)]
pub struct Pacer {
    /// The size of each frame not yet acknowledged, oldest first.
    in_flight: VecDeque<usize>,
    in_flight_bytes: usize,
    /// Output waiting to go out, each piece with when it arrived.
    waiting: VecDeque<(String, Instant)>,
    waiting_bytes: usize,
    /// Output was thrown away, so only a repaint can catch the viewer up.
    dropped: bool,
}

impl Pacer {
    /// A session produced this output.
    pub fn push(&mut self, data: &str, now: Instant) {
        if self.dropped {
            return;
        }
        if self.waiting_bytes + data.len() > MAX_WAITING {
            self.clear();
            self.dropped = true;
            return;
        }
        self.waiting.push_back((data.to_string(), now));
        self.waiting_bytes += data.len();
    }

    /// A frame of this size went out other than through `next` or `flush`.
    pub fn sent(&mut self, bytes: usize) {
        self.in_flight.push_back(bytes);
        self.in_flight_bytes += bytes;
    }

    /// The client has the oldest frame still out.
    pub fn ack(&mut self) {
        if let Some(bytes) = self.in_flight.pop_front() {
            self.in_flight_bytes -= bytes;
        }
    }

    /// Forget waiting output: what's sent next covers it.
    pub fn clear(&mut self) {
        self.waiting.clear();
        self.waiting_bytes = 0;
        self.dropped = false;
    }

    pub fn next(&mut self, now: Instant) -> Next {
        if self.in_flight_bytes >= WINDOW {
            return Next::Wait;
        }
        let aged = self
            .waiting
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > MAX_BEHIND);
        if self.dropped || aged {
            self.clear();
            return Next::Repaint;
        }
        let frame = self.take(FRAME);
        if frame.is_empty() {
            return Next::Wait;
        }
        self.sent(frame.len());
        Next::Send(frame)
    }

    /// Everything waiting, whatever the window says: for an event that has to
    /// stay in order with the output before it.
    pub fn flush(&mut self) -> Next {
        if self.dropped {
            self.clear();
            return Next::Repaint;
        }
        let frame = self.take(self.waiting_bytes);
        if frame.is_empty() {
            return Next::Wait;
        }
        self.sent(frame.len());
        Next::Send(frame)
    }

    /// Up to `limit` bytes of waiting output, ending on a character boundary.
    fn take(&mut self, limit: usize) -> String {
        let mut frame = String::new();
        while let Some((piece, _)) = self.waiting.front_mut() {
            let room = limit - frame.len();
            if piece.len() <= room {
                frame.push_str(piece);
                self.waiting.pop_front();
                continue;
            }
            if frame.is_empty() {
                let mut end = room;
                while !piece.is_char_boundary(end) {
                    end -= 1;
                }
                frame.push_str(&piece[..end]);
                piece.drain(..end);
            }
            break;
        }
        self.waiting_bytes -= frame.len();
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(next: Next) -> String {
        match next {
            Next::Send(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn nothing_waiting_should_mean_waiting() {
        assert_eq!(Pacer::default().next(Instant::now()), Next::Wait);
    }

    #[test]
    fn small_pieces_should_share_a_frame() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.push("one", now);
        pacer.push("two", now);
        assert_eq!(sent(pacer.next(now)), "onetwo");
        assert_eq!(pacer.next(now), Next::Wait);
    }

    #[test]
    fn a_full_window_should_hold_output_until_it_is_acknowledged() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.push(&"x".repeat(WINDOW + FRAME), now);
        let mut out = 0;
        while let Next::Send(frame) = pacer.next(now) {
            assert!(frame.len() <= FRAME);
            out += frame.len();
        }
        assert_eq!(out, WINDOW);
        pacer.ack();
        assert_eq!(sent(pacer.next(now)).len(), FRAME);
    }

    #[test]
    fn a_frame_should_not_split_a_character() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.push(&format!("{}é{}", "x".repeat(FRAME - 1), "y"), now);
        let first = sent(pacer.next(now));
        assert_eq!(first.len(), FRAME - 1);
        assert_eq!(sent(pacer.next(now)), "éy");
    }

    #[test]
    fn output_that_waited_too_long_should_become_a_repaint() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.sent(WINDOW);
        pacer.push("old", now);
        let later = now + MAX_BEHIND + Duration::from_millis(1);
        assert_eq!(pacer.next(later), Next::Wait);
        pacer.ack();
        assert_eq!(pacer.next(later), Next::Repaint);
        assert_eq!(pacer.next(later), Next::Wait);
    }

    #[test]
    fn a_viewer_that_stopped_acknowledging_should_not_grow_without_end() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.sent(WINDOW);
        for _ in 0..100 {
            pacer.push(&"x".repeat(64 * 1024), now);
        }
        assert_eq!(pacer.waiting_bytes, 0);
        pacer.ack();
        assert_eq!(pacer.next(now), Next::Repaint);
    }

    #[test]
    fn flush_should_ignore_the_window() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.sent(WINDOW);
        pacer.push("before the event", now);
        assert_eq!(sent(pacer.flush()), "before the event");
        assert_eq!(pacer.flush(), Next::Wait);
    }
}
