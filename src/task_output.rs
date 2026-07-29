//! Incremental UTF-8 decoding for interleaved task output streams.
//!
//! Pipe reads may split one Unicode scalar value across several task events.
//! `TaskOutputDecoder` retains only the incomplete suffix for each pipe so a
//! read from the other pipe cannot disturb it. The retained suffix is always
//! at most three bytes: UTF-8 scalar values are at most four bytes long, and a
//! complete scalar is emitted immediately.

use crate::tasks::OutputStream;

const MAX_PENDING_BYTES: usize = 3;

/// A bounded incremental UTF-8 decoder for a task's stdout and stderr pipes.
#[derive(Clone, Debug, Default)]
pub struct TaskOutputDecoder {
    stdout: PendingUtf8,
    stderr: PendingUtf8,
}

impl TaskOutputDecoder {
    /// Creates a decoder with no pending bytes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one pipe-read chunk, retaining only a possible incomplete UTF-8
    /// suffix for the selected stream.
    ///
    /// Complete malformed sequences are emitted as Unicode replacement
    /// characters. The returned string contains only text made available by
    /// this call; it does not retain prior output.
    pub fn push(&mut self, stream: OutputStream, bytes: &[u8]) -> String {
        let pending = self.pending_mut(stream);
        let mut output = String::with_capacity(bytes.len().saturating_add(pending.len()));
        let mut remaining = bytes;

        // Resolve an existing suffix using at most enough input to form one
        // four-byte scalar. Invalid input can leave a different incomplete
        // suffix in that small window, so repeat while this call still has
        // bytes available.
        while !pending.is_empty() && !remaining.is_empty() {
            let take = (4 - pending.len()).min(remaining.len());
            let mut prefix = [0_u8; 4];
            let pending_len = pending.copy_into(&mut prefix);
            prefix[pending_len..pending_len + take].copy_from_slice(&remaining[..take]);
            pending.clear();
            append_lossy(&prefix[..pending_len + take], &mut output, pending);
            remaining = &remaining[take..];
        }

        if pending.is_empty() && !remaining.is_empty() {
            append_lossy(remaining, &mut output, pending);
        }

        output
    }

    /// Discards incomplete suffixes after the task event queue reports an
    /// output gap. Returns whether either stream had bytes to discard.
    pub fn discard_pending_on_gap(&mut self) -> bool {
        let discarded = !self.stdout.is_empty() || !self.stderr.is_empty();
        self.stdout.clear();
        self.stderr.clear();
        discarded
    }

    /// Finishes one stream, emitting a replacement character if its final read
    /// ended partway through a UTF-8 scalar value.
    pub fn finish(&mut self, stream: OutputStream) -> String {
        let pending = self.pending_mut(stream);
        if pending.is_empty() {
            return String::new();
        }
        pending.clear();
        '\u{fffd}'.to_string()
    }

    /// Clears both streams so the decoder can be reused for another task.
    pub fn reset(&mut self) {
        self.stdout.clear();
        self.stderr.clear();
    }

    fn pending_mut(&mut self, stream: OutputStream) -> &mut PendingUtf8 {
        match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PendingUtf8 {
    bytes: [u8; MAX_PENDING_BYTES],
    len: u8,
}

impl PendingUtf8 {
    fn len(&self) -> usize {
        usize::from(self.len)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn set(&mut self, bytes: &[u8]) {
        assert!(
            bytes.len() <= MAX_PENDING_BYTES,
            "an incomplete UTF-8 suffix cannot exceed three bytes"
        );
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len() as u8;
    }

    fn copy_into(&self, destination: &mut [u8; 4]) -> usize {
        let len = self.len();
        destination[..len].copy_from_slice(&self.bytes[..len]);
        len
    }
}

fn append_lossy(mut bytes: &[u8], output: &mut String, pending: &mut PendingUtf8) {
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                output.push_str(valid);
                return;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let valid = std::str::from_utf8(&bytes[..valid_up_to])
                    .expect("Utf8Error::valid_up_to always identifies valid UTF-8");
                output.push_str(valid);

                match error.error_len() {
                    Some(invalid_len) => {
                        output.push('\u{fffd}');
                        bytes = &bytes[valid_up_to + invalid_len..];
                    }
                    None => {
                        pending.set(&bytes[valid_up_to..]);
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_ascii_and_complete_unicode_immediately() {
        let mut decoder = TaskOutputDecoder::new();

        assert_eq!(
            decoder.push(OutputStream::Stdout, "check café/🦀.rs\n".as_bytes()),
            "check café/🦀.rs\n"
        );
        assert!(decoder.stdout.is_empty());
        assert!(decoder.stderr.is_empty());
    }

    #[test]
    fn decodes_every_unicode_byte_boundary() {
        let expected = "路径/café-🦀.rs:27:4: déjà vu\n";
        let mut decoder = TaskOutputDecoder::new();
        let mut actual = String::new();

        for byte in expected.as_bytes() {
            actual.push_str(&decoder.push(OutputStream::Stdout, std::slice::from_ref(byte)));
            assert!(decoder.stdout.len() <= MAX_PENDING_BYTES);
        }
        actual.push_str(&decoder.finish(OutputStream::Stdout));

        assert_eq!(actual, expected);
    }

    #[test]
    fn keeps_interleaved_stream_suffixes_independent() {
        let mut decoder = TaskOutputDecoder::new();
        let stdout = "α.rs:1:2".as_bytes();
        let stderr = "路径.rs:3:4".as_bytes();
        let mut stdout_text = String::new();
        let mut stderr_text = String::new();

        let rounds = stdout.len().max(stderr.len());
        for index in 0..rounds {
            if let Some(byte) = stdout.get(index) {
                stdout_text
                    .push_str(&decoder.push(OutputStream::Stdout, std::slice::from_ref(byte)));
            }
            if let Some(byte) = stderr.get(index) {
                stderr_text
                    .push_str(&decoder.push(OutputStream::Stderr, std::slice::from_ref(byte)));
            }
            assert!(decoder.stdout.len() <= MAX_PENDING_BYTES);
            assert!(decoder.stderr.len() <= MAX_PENDING_BYTES);
        }

        stdout_text.push_str(&decoder.finish(OutputStream::Stdout));
        stderr_text.push_str(&decoder.finish(OutputStream::Stderr));
        assert_eq!(stdout_text, "α.rs:1:2");
        assert_eq!(stderr_text, "路径.rs:3:4");
    }

    #[test]
    fn replaces_complete_malformed_sequences() {
        let mut decoder = TaskOutputDecoder::new();

        assert_eq!(
            decoder.push(OutputStream::Stdout, &[b'a', 0xff, b'b']),
            "a�b"
        );
        assert_eq!(decoder.push(OutputStream::Stdout, &[0xe2]), "");
        assert_eq!(decoder.push(OutputStream::Stdout, &[0x28, 0xa1]), "�(�");
        assert_eq!(
            decoder.push(OutputStream::Stdout, &[0xf0, 0x80, 0x80, 0x80]),
            "����"
        );
    }

    #[test]
    fn gap_discards_both_stream_suffixes_and_reports_it() {
        let mut decoder = TaskOutputDecoder::new();
        assert_eq!(decoder.push(OutputStream::Stdout, &[0xe2]), "");
        assert_eq!(decoder.push(OutputStream::Stderr, &[0xf0, 0x9f]), "");

        assert!(decoder.discard_pending_on_gap());
        assert!(!decoder.discard_pending_on_gap());
        assert_eq!(decoder.push(OutputStream::Stdout, &[0x82, 0xac]), "��");
        assert_eq!(decoder.push(OutputStream::Stderr, &[0xa6, 0x80]), "��");
    }

    #[test]
    fn finish_flushes_only_the_selected_incomplete_stream() {
        let mut decoder = TaskOutputDecoder::new();
        assert_eq!(decoder.push(OutputStream::Stdout, &[0xf0, 0x9f, 0xa6]), "");
        assert_eq!(decoder.push(OutputStream::Stderr, &[0xe2, 0x82]), "");

        assert_eq!(decoder.finish(OutputStream::Stdout), "�");
        assert_eq!(decoder.finish(OutputStream::Stdout), "");
        assert_eq!(decoder.push(OutputStream::Stderr, &[0xac]), "€");
        assert_eq!(decoder.finish(OutputStream::Stderr), "");
    }

    #[test]
    fn reset_drops_state_without_emitting_text() {
        let mut decoder = TaskOutputDecoder::new();
        assert_eq!(decoder.push(OutputStream::Stdout, &[0xe2]), "");
        assert_eq!(decoder.push(OutputStream::Stderr, &[0xf0, 0x9f]), "");

        decoder.reset();

        assert_eq!(decoder.finish(OutputStream::Stdout), "");
        assert_eq!(decoder.finish(OutputStream::Stderr), "");
        assert_eq!(decoder.push(OutputStream::Stdout, b"fresh"), "fresh");
    }

    #[test]
    fn all_chunkings_match_standard_lossy_decoding() {
        let bytes = b"ok \xe8\xb7\xaf\xe5\xbe\x84 \xf0\x9f\xa6\x80 \xff \xe2(\xa1 end \xf0\x9f\xa6";
        let expected = String::from_utf8_lossy(bytes);

        for first in 0..=bytes.len() {
            for second in first..=bytes.len() {
                let mut decoder = TaskOutputDecoder::new();
                let mut actual = decoder.push(OutputStream::Stdout, &bytes[..first]);
                actual.push_str(&decoder.push(OutputStream::Stdout, &bytes[first..second]));
                actual.push_str(&decoder.push(OutputStream::Stdout, &bytes[second..]));
                assert!(decoder.stdout.len() <= MAX_PENDING_BYTES);
                actual.push_str(&decoder.finish(OutputStream::Stdout));
                assert_eq!(actual, expected, "chunk boundaries {first}, {second}");
            }
        }
    }

    #[test]
    fn retained_tail_never_exceeds_three_bytes() {
        let mut decoder = TaskOutputDecoder::new();
        let corpus = [
            0x00, 0x7f, 0x80, 0xbf, 0xc0, 0xc2, 0xdf, 0xe0, 0xe1, 0xed, 0xef, 0xf0, 0xf1, 0xf4,
            0xf5, 0xff,
        ];

        for first in corpus {
            for second in corpus {
                for third in corpus {
                    decoder.reset();
                    for byte in [first, second, third] {
                        let _ = decoder.push(OutputStream::Stdout, &[byte]);
                        assert!(decoder.stdout.len() <= MAX_PENDING_BYTES);
                        assert!(decoder.stderr.len() <= MAX_PENDING_BYTES);
                    }
                }
            }
        }
    }
}
