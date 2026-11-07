//! Decodes a PTY byte stream into UTF-8 text without splitting characters.
//!
//! PTY reads can end in the middle of a multi-byte character. The incomplete
//! tail is held back until the next chunk arrives, so every chunk sent to
//! clients is valid UTF-8 and clients never see a stray replacement character
//! at a read boundary.

#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// Append bytes and return all text that is complete so far. Invalid bytes
    /// become U+FFFD; an incomplete trailing sequence is kept for the next call.
    pub fn push(&mut self, data: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);

        let mut out = String::with_capacity(buf.len());
        let mut rest = buf.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    // `valid_up_to` guarantees this prefix is valid UTF-8.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match err.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &after[len..];
                        }
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_should_hold_back_split_character_until_complete() {
        let mut stream = Utf8Stream::default();
        let bytes = "é".as_bytes();
        assert_eq!(stream.push(&bytes[..1]), "");
        assert_eq!(stream.push(&bytes[1..]), "é");
    }

    #[test]
    fn push_should_replace_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(b"a\xffb"), "a\u{FFFD}b");
    }
}
