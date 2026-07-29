//! Clipboard support that remains useful when a remote terminal cannot expose a
//! native system clipboard.
//!
//! [`Clipboard`] always stores the complete copied text in an internal register.
//! When enabled, it also prepares an OSC 52 terminal sequence for the caller to
//! write to the terminal. OSC 52 has no portable acknowledgement mechanism, so a
//! prepared sequence is deliberately described as an *attempt*, not proof that
//! the client clipboard changed.
//!
//! OSC 52 output is bounded by [`Osc52Config::max_payload_bytes`]. Oversized text
//! is never truncated: [`CopyOutcome::Refused`] reports the exact encoded size,
//! while the internal register still contains the complete original text.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// A conservative default limit for the base64 portion of an OSC 52 sequence.
///
/// The framing bytes are small and fixed. Keeping the limit on the payload makes
/// it independent of whether direct or tmux framing is selected and lets us
/// reject an oversized copy before allocating the encoded string.
pub const DEFAULT_MAX_OSC52_PAYLOAD_BYTES: usize = 100_000;

const DIRECT_PREFIX: &str = "\x1b]52;c;";
const DIRECT_SUFFIX: &str = "\x07";
const TMUX_PREFIX: &str = "\x1bPtmux;\x1b\x1b]52;c;";
const TMUX_SUFFIX: &str = "\x07\x1b\\";

/// How an OSC 52 sequence reaches the terminal that owns the clipboard.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Osc52Transport {
    /// Emit OSC 52 directly to the current terminal.
    #[default]
    Direct,
    /// Wrap OSC 52 in tmux's DCS passthrough envelope.
    TmuxPassthrough,
}

/// Runtime policy for system-clipboard copy attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Osc52Config {
    /// Whether an OSC 52 sequence may be prepared.
    pub enabled: bool,
    /// Direct-terminal or tmux passthrough framing.
    pub transport: Osc52Transport,
    /// Maximum number of base64 bytes allowed in a sequence.
    ///
    /// This is an encoded-payload limit, not a source-text limit. A value of
    /// zero still permits copying an empty string, but refuses any non-empty
    /// input.
    pub max_payload_bytes: usize,
}

impl Default for Osc52Config {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: Osc52Transport::Direct,
            max_payload_bytes: DEFAULT_MAX_OSC52_PAYLOAD_BYTES,
        }
    }
}

impl Osc52Config {
    /// A configuration that only uses the internal register.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            transport: Osc52Transport::Direct,
            max_payload_bytes: DEFAULT_MAX_OSC52_PAYLOAD_BYTES,
        }
    }

    /// Direct-terminal OSC 52 with an explicit encoded-payload limit.
    pub const fn direct(max_payload_bytes: usize) -> Self {
        Self {
            enabled: true,
            transport: Osc52Transport::Direct,
            max_payload_bytes,
        }
    }

    /// tmux DCS passthrough with an explicit encoded-payload limit.
    pub const fn tmux(max_payload_bytes: usize) -> Self {
        Self {
            enabled: true,
            transport: Osc52Transport::TmuxPassthrough,
            max_payload_bytes,
        }
    }
}

/// Allows `Clipboard::new(config.osc52_copy)` at simple integration sites while
/// retaining [`Osc52Config`] for transport and size customization.
impl From<bool> for Osc52Config {
    fn from(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }
}

/// Select OSC 52 framing for the current route.
///
/// Inside tmux, a direct OSC 52 sequence terminates at tmux itself and reaches
/// the outer terminal only when tmux's `set-clipboard` forwarding is on. The
/// DCS passthrough envelope instead delivers the sequence to the outer
/// terminal directly; tmux 3.3+ requires `allow-passthrough on` for that.
pub fn osc52_config_for_route(enabled: bool, inside_tmux: bool) -> Osc52Config {
    Osc52Config {
        enabled,
        transport: if inside_tmux {
            Osc52Transport::TmuxPassthrough
        } else {
            Osc52Transport::Direct
        },
        max_payload_bytes: DEFAULT_MAX_OSC52_PAYLOAD_BYTES,
    }
}

/// Why a system-clipboard attempt was not prepared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Osc52Refusal {
    /// OSC 52 is disabled by configuration.
    Disabled,
    /// Standard base64 would exceed the configured payload limit.
    ///
    /// The complete source text remains available in the internal register.
    PayloadTooLarge {
        input_bytes: usize,
        encoded_bytes: usize,
        max_payload_bytes: usize,
    },
    /// The encoded length could not be represented by this platform's `usize`.
    LengthOverflow { input_bytes: usize },
}

impl fmt::Display for Osc52Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("OSC 52 copy is disabled"),
            Self::PayloadTooLarge {
                encoded_bytes,
                max_payload_bytes,
                ..
            } => write!(
                formatter,
                "OSC 52 payload is {encoded_bytes} bytes; configured limit is {max_payload_bytes} bytes"
            ),
            Self::LengthOverflow { input_bytes } => write!(
                formatter,
                "OSC 52 payload length overflow for {input_bytes} input bytes"
            ),
        }
    }
}

/// A fully encoded, ASCII-only terminal control sequence.
///
/// Source text is encoded with standard padded base64 and therefore cannot
/// inject BEL, ESC, or another terminal control into the OSC 52 payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Osc52Sequence {
    bytes: String,
    input_bytes: usize,
    payload_bytes: usize,
    transport: Osc52Transport,
}

impl Osc52Sequence {
    /// The complete control sequence, ready for `Write::write_all`.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    /// The complete control sequence as a string.
    pub fn as_str(&self) -> &str {
        &self.bytes
    }

    /// Consume this value and return the complete control sequence.
    pub fn into_string(self) -> String {
        self.bytes
    }

    /// Number of bytes in the original UTF-8 text.
    pub const fn input_bytes(&self) -> usize {
        self.input_bytes
    }

    /// Number of bytes in the base64 payload, excluding terminal framing.
    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    /// Framing used by this sequence.
    pub const fn transport(&self) -> Osc52Transport {
        self.transport
    }
}

impl AsRef<[u8]> for Osc52Sequence {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<str> for Osc52Sequence {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Result of copying into the internal register and considering OSC 52.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyOutcome {
    /// A bounded sequence is ready to be written as a best-effort attempt.
    Prepared(Osc52Sequence),
    /// No terminal sequence was produced. The internal copy still succeeded.
    Refused(Osc52Refusal),
}

impl CopyOutcome {
    /// Borrow the terminal sequence when an OSC 52 attempt was prepared.
    pub fn sequence(&self) -> Option<&Osc52Sequence> {
        match self {
            Self::Prepared(sequence) => Some(sequence),
            Self::Refused(_) => None,
        }
    }

    /// Borrow the explicit reason no OSC 52 attempt was prepared.
    pub fn refusal(&self) -> Option<&Osc52Refusal> {
        match self {
            Self::Prepared(_) => None,
            Self::Refused(reason) => Some(reason),
        }
    }

    /// Consume the outcome and return a sequence when one was prepared.
    pub fn into_sequence(self) -> Option<Osc52Sequence> {
        match self {
            Self::Prepared(sequence) => Some(sequence),
            Self::Refused(_) => None,
        }
    }
}

/// An editor-local clipboard with optional OSC 52 system-clipboard integration.
#[derive(Clone, Debug)]
pub struct Clipboard {
    register: String,
    osc52: Osc52Config,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new(Osc52Config::default())
    }
}

impl Clipboard {
    /// Create an empty clipboard.
    ///
    /// This accepts either a full [`Osc52Config`] or a boolean enablement flag.
    /// A boolean uses direct framing and the default payload limit.
    pub fn new(config: impl Into<Osc52Config>) -> Self {
        Self {
            register: String::new(),
            osc52: config.into(),
        }
    }

    /// Read the editor-local register.
    pub fn register(&self) -> &str {
        &self.register
    }

    /// Whether the editor-local register is empty.
    pub fn is_empty(&self) -> bool {
        self.register.is_empty()
    }

    /// Inspect the current OSC 52 policy.
    pub const fn osc52_config(&self) -> Osc52Config {
        self.osc52
    }

    /// A short human-readable label for diagnostic views.
    pub const fn osc52_route_label(&self) -> &'static str {
        if !self.osc52.enabled {
            return "off";
        }
        match self.osc52.transport {
            Osc52Transport::Direct => "direct",
            Osc52Transport::TmuxPassthrough => "tmux-passthrough",
        }
    }

    /// Replace the OSC 52 policy without changing the internal register.
    pub fn set_osc52_config(&mut self, config: impl Into<Osc52Config>) {
        self.osc52 = config.into();
    }

    /// Replace the internal register and prepare a best-effort system copy.
    ///
    /// The internal register is updated first and always retains the complete
    /// input, including when OSC 52 is disabled or refuses an oversized payload.
    #[must_use = "write a prepared sequence to the terminal or surface its refusal"]
    pub fn copy(&mut self, text: &str) -> CopyOutcome {
        self.register.clear();
        self.register.push_str(text);
        prepare_osc52(&self.register, self.osc52)
    }
}

/// Encode text for a bounded, best-effort OSC 52 copy attempt.
///
/// This function never truncates. It also checks the exact padded-base64 length
/// before encoding, avoiding an oversized intermediate allocation.
pub fn prepare_osc52(text: &str, config: Osc52Config) -> CopyOutcome {
    if !config.enabled {
        return CopyOutcome::Refused(Osc52Refusal::Disabled);
    }

    let input_bytes = text.len();
    let Some(payload_bytes) = base64::encoded_len(input_bytes, true) else {
        return CopyOutcome::Refused(Osc52Refusal::LengthOverflow { input_bytes });
    };

    if payload_bytes > config.max_payload_bytes {
        return CopyOutcome::Refused(Osc52Refusal::PayloadTooLarge {
            input_bytes,
            encoded_bytes: payload_bytes,
            max_payload_bytes: config.max_payload_bytes,
        });
    }

    let payload = STANDARD.encode(text.as_bytes());
    debug_assert_eq!(payload.len(), payload_bytes);

    let (prefix, suffix) = match config.transport {
        Osc52Transport::Direct => (DIRECT_PREFIX, DIRECT_SUFFIX),
        Osc52Transport::TmuxPassthrough => (TMUX_PREFIX, TMUX_SUFFIX),
    };
    let mut bytes = String::with_capacity(prefix.len() + payload_bytes + suffix.len());
    bytes.push_str(prefix);
    bytes.push_str(&payload);
    bytes.push_str(suffix);

    CopyOutcome::Prepared(Osc52Sequence {
        bytes,
        input_bytes,
        payload_bytes,
        transport: config.transport,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(outcome: CopyOutcome) -> Osc52Sequence {
        match outcome {
            CopyOutcome::Prepared(sequence) => sequence,
            CopyOutcome::Refused(reason) => panic!("unexpected refusal: {reason}"),
        }
    }

    fn direct_payload(sequence: &Osc52Sequence) -> &str {
        sequence
            .as_str()
            .strip_prefix(DIRECT_PREFIX)
            .and_then(|text| text.strip_suffix(DIRECT_SUFFIX))
            .expect("direct OSC 52 framing")
    }

    #[test]
    fn default_config_is_enabled_direct_and_bounded() {
        let config = Osc52Config::default();
        assert!(config.enabled);
        assert_eq!(config.transport, Osc52Transport::Direct);
        assert_eq!(config.max_payload_bytes, DEFAULT_MAX_OSC52_PAYLOAD_BYTES);
    }

    #[test]
    fn bool_conversion_only_changes_enablement() {
        let enabled = Osc52Config::from(true);
        let disabled = Osc52Config::from(false);
        assert_eq!(enabled, Osc52Config::default());
        assert!(!disabled.enabled);
        assert_eq!(disabled.transport, Osc52Transport::Direct);
        assert_eq!(disabled.max_payload_bytes, DEFAULT_MAX_OSC52_PAYLOAD_BYTES);
    }

    #[test]
    fn route_outside_tmux_selects_direct_transport() {
        let config = osc52_config_for_route(true, false);
        assert!(config.enabled);
        assert_eq!(config.transport, Osc52Transport::Direct);
        assert_eq!(config.max_payload_bytes, DEFAULT_MAX_OSC52_PAYLOAD_BYTES);
    }

    #[test]
    fn route_inside_tmux_selects_dcs_passthrough() {
        let config = osc52_config_for_route(true, true);
        assert!(config.enabled);
        assert_eq!(config.transport, Osc52Transport::TmuxPassthrough);
        assert_eq!(config.max_payload_bytes, DEFAULT_MAX_OSC52_PAYLOAD_BYTES);
    }

    #[test]
    fn route_selection_preserves_disablement_in_and_out_of_tmux() {
        for inside_tmux in [false, true] {
            let config = osc52_config_for_route(false, inside_tmux);
            assert!(!config.enabled);
            assert_eq!(config.max_payload_bytes, DEFAULT_MAX_OSC52_PAYLOAD_BYTES);
        }
    }

    #[test]
    fn direct_sequence_uses_standard_padded_base64() {
        let sequence = prepared(prepare_osc52("f", Osc52Config::direct(4)));
        assert_eq!(sequence.as_str(), "\x1b]52;c;Zg==\x07");
        assert_eq!(sequence.as_bytes(), sequence.as_str().as_bytes());
        assert_eq!(sequence.input_bytes(), 1);
        assert_eq!(sequence.payload_bytes(), 4);
        assert_eq!(sequence.transport(), Osc52Transport::Direct);
    }

    #[test]
    fn arbitrary_utf8_and_controls_cannot_inject_terminal_sequences() {
        let source = "crow 🪶\n\x1b]52;c;attack\x07";
        let sequence = prepared(prepare_osc52(source, Osc52Config::direct(1_000)));
        let payload = direct_payload(&sequence);

        assert!(
            payload
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=') })
        );
        assert!(!payload.as_bytes().contains(&0x1b));
        assert!(!payload.as_bytes().contains(&0x07));
        assert_eq!(STANDARD.decode(payload).unwrap(), source.as_bytes());
    }

    #[test]
    fn tmux_sequence_uses_dcs_passthrough_and_doubled_escape() {
        let sequence = prepared(prepare_osc52("hello", Osc52Config::tmux(8)));
        assert_eq!(
            sequence.as_str(),
            "\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"
        );
        assert_eq!(sequence.input_bytes(), 5);
        assert_eq!(sequence.payload_bytes(), 8);
        assert_eq!(sequence.transport(), Osc52Transport::TmuxPassthrough);
    }

    #[test]
    fn payload_exactly_at_limit_is_prepared() {
        let sequence = prepared(prepare_osc52("abc", Osc52Config::direct(4)));
        assert_eq!(direct_payload(&sequence), "YWJj");
        assert_eq!(sequence.payload_bytes(), 4);
    }

    #[test]
    fn payload_one_byte_over_limit_is_explicitly_refused() {
        let outcome = prepare_osc52("abc", Osc52Config::direct(3));
        assert_eq!(
            outcome,
            CopyOutcome::Refused(Osc52Refusal::PayloadTooLarge {
                input_bytes: 3,
                encoded_bytes: 4,
                max_payload_bytes: 3,
            })
        );
        assert!(outcome.sequence().is_none());
        assert!(outcome.refusal().is_some());
    }

    #[test]
    fn limit_is_measured_in_encoded_not_source_bytes() {
        assert!(matches!(
            prepare_osc52("four", Osc52Config::direct(4)),
            CopyOutcome::Refused(Osc52Refusal::PayloadTooLarge {
                input_bytes: 4,
                encoded_bytes: 8,
                max_payload_bytes: 4,
            })
        ));
    }

    #[test]
    fn disabled_copy_is_explicitly_refused() {
        let outcome = prepare_osc52("text", Osc52Config::disabled());
        assert_eq!(outcome, CopyOutcome::Refused(Osc52Refusal::Disabled));
        assert_eq!(outcome.refusal(), Some(&Osc52Refusal::Disabled));
    }

    #[test]
    fn empty_copy_fits_a_zero_byte_payload_limit() {
        let sequence = prepared(prepare_osc52("", Osc52Config::direct(0)));
        assert_eq!(sequence.as_str(), "\x1b]52;c;\x07");
        assert_eq!(sequence.input_bytes(), 0);
        assert_eq!(sequence.payload_bytes(), 0);
    }

    #[test]
    fn nonempty_copy_is_refused_by_zero_byte_payload_limit() {
        assert_eq!(
            prepare_osc52("x", Osc52Config::direct(0)),
            CopyOutcome::Refused(Osc52Refusal::PayloadTooLarge {
                input_bytes: 1,
                encoded_bytes: 4,
                max_payload_bytes: 0,
            })
        );
    }

    #[test]
    fn internal_register_keeps_full_text_when_osc52_is_disabled() {
        let source = "the entire selection";
        let mut clipboard = Clipboard::new(false);
        let outcome = clipboard.copy(source);

        assert_eq!(outcome, CopyOutcome::Refused(Osc52Refusal::Disabled));
        assert_eq!(clipboard.register(), source);
        assert!(!clipboard.is_empty());
    }

    #[test]
    fn internal_register_never_silently_truncates_oversized_text() {
        let source = "🪶 complete selection, including this tail";
        let mut clipboard = Clipboard::new(Osc52Config::direct(4));
        let outcome = clipboard.copy(source);

        assert!(matches!(
            outcome,
            CopyOutcome::Refused(Osc52Refusal::PayloadTooLarge { .. })
        ));
        assert_eq!(clipboard.register(), source);
        assert_eq!(clipboard.register().as_bytes(), source.as_bytes());
    }

    #[test]
    fn copying_again_replaces_the_internal_register() {
        let mut clipboard = Clipboard::new(false);
        let _ = clipboard.copy("first");
        let _ = clipboard.copy("second");
        assert_eq!(clipboard.register(), "second");
    }

    #[test]
    fn runtime_reconfiguration_preserves_register_and_changes_transport() {
        let mut clipboard = Clipboard::new(false);
        let _ = clipboard.copy("persisted");
        clipboard.set_osc52_config(Osc52Config::tmux(100));

        assert_eq!(clipboard.register(), "persisted");
        assert_eq!(clipboard.osc52_config(), Osc52Config::tmux(100));
        let sequence = prepared(clipboard.copy("next"));
        assert_eq!(sequence.transport(), Osc52Transport::TmuxPassthrough);
    }

    #[test]
    fn outcome_can_be_consumed_into_a_sequence() {
        let outcome = prepare_osc52("ok", Osc52Config::direct(4));
        assert_eq!(
            outcome.into_sequence().unwrap().into_string(),
            "\x1b]52;c;b2s=\x07"
        );

        assert!(
            prepare_osc52("ok", Osc52Config::disabled())
                .into_sequence()
                .is_none()
        );
    }

    #[test]
    fn refusal_messages_are_actionable() {
        assert_eq!(
            Osc52Refusal::Disabled.to_string(),
            "OSC 52 copy is disabled"
        );
        assert_eq!(
            Osc52Refusal::PayloadTooLarge {
                input_bytes: 6,
                encoded_bytes: 8,
                max_payload_bytes: 4,
            }
            .to_string(),
            "OSC 52 payload is 8 bytes; configured limit is 4 bytes"
        );
    }
}
