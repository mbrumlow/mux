use tracing::info;

const ESC: u8 = 0x1b;
const MAX_SEQ_LEN: usize = 128;

// KKP flag bits
const REPORT_EVENT_TYPES: u32 = 2;
const REPORT_ALTERNATE_KEYS: u32 = 4;
const REPORT_ALL_KEYS_AS_ESCAPE_CODES: u32 = 8;
const REPORT_ASSOCIATED_TEXT: u32 = 16;

/// A parsed CSI-u (kitty keyboard protocol) event.
struct KkpEvent {
    keycode: u32,
    shifted_key: Option<u32>,
    base_key: Option<u32>,
    modifiers: u32,    // 1 + modifier_bits (1 = no mods)
    event_type: u32,   // 0=implicit press, 1=press, 2=repeat, 3=release
    text_codepoints: Vec<u32>,
}

enum State {
    Normal,
    Esc,
    Csi,
}

pub struct KkpTranslator {
    inner_flags: u32,
    state: State,
    seq_buf: Vec<u8>,
}

/// True for bytes that are CSI parameter characters.
/// ECMA-48 defines parameter bytes as 0x30-0x3F, which includes digits,
/// semicolons, colons, and private-use markers (<, =, >, ?).
fn is_csi_param(b: u8) -> bool {
    (0x30..=0x3F).contains(&b)
}

/// True for CSI intermediate bytes (space through /)
fn is_csi_intermediate(b: u8) -> bool {
    (0x20..=0x2F).contains(&b)
}

/// True for CSI final bytes
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7E).contains(&b)
}

/// Parse the first parameter group (keycode[:shifted_key[:base_key]])
fn parse_key_group(s: &str) -> Option<(u32, Option<u32>, Option<u32>)> {
    let mut parts = s.split(':');
    let keycode: u32 = parts.next()?.parse().ok()?;
    let shifted_key = parts.next().and_then(|p| {
        if p.is_empty() { None } else { p.parse().ok() }
    });
    let base_key = parts.next().and_then(|p| {
        if p.is_empty() { None } else { p.parse().ok() }
    });
    Some((keycode, shifted_key, base_key))
}

/// Parse the modifier group (modifiers[:event_type])
fn parse_modifier_group(s: &str) -> (u32, u32) {
    let mut parts = s.split(':');
    let modifiers = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1);
    let event_type = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    (modifiers, event_type)
}

/// Parse a full CSI-u sequence. `params` is the bytes between '[' and 'u'.
fn parse_csi_u(params: &[u8]) -> Option<KkpEvent> {
    let s = std::str::from_utf8(params).ok()?;
    let mut semicolons = s.split(';');

    // First group: keycode[:shifted_key[:base_key]]
    let key_part = semicolons.next()?;
    let (keycode, shifted_key, base_key) = parse_key_group(key_part)?;

    // Second group (optional): modifiers[:event_type]
    let (modifiers, event_type) = match semicolons.next() {
        Some(mod_part) => parse_modifier_group(mod_part),
        None => (1, 0),
    };

    // Third group (optional): text as codepoints separated by ':'
    let text_codepoints = match semicolons.next() {
        Some(text_part) => text_part
            .split(':')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect(),
        None => Vec::new(),
    };

    Some(KkpEvent {
        keycode,
        shifted_key,
        base_key,
        modifiers,
        event_type,
        text_codepoints,
    })
}

/// True if keycode is a standalone modifier key (Shift, Ctrl, Alt, Super, Hyper, locks).
fn is_modifier_key(keycode: u32) -> bool {
    // KKP modifier keys: 57441 (Left Shift) through 57453 (Num Lock)
    (57441..=57453).contains(&keycode)
}

/// True if the event_type represents a press (including repeat treated as press).
fn dominated_press(event_type: u32, inner_flags: u32) -> bool {
    event_type <= 1 || (event_type == 2 && (inner_flags & REPORT_EVENT_TYPES) == 0)
}

/// Reverse a C0 control code to the base key that produces it with Ctrl.
/// Used as fallback when base_key is not available.
fn reverse_c0(c0: u32) -> u32 {
    match c0 {
        0 => 32,           // NUL → Space
        1..=26 => c0 + 96, // SOH-SUB → a-z
        27 => 91,          // ESC → [
        28 => 92,          // FS → backslash
        29 => 93,          // GS → ]
        30 => 94,          // RS → ^
        31 => 95,          // US → _
        _ => c0,
    }
}

/// Re-encode a KkpEvent for the inner app's flags, appending to `out`.
fn reencode_csi_u(ev: &KkpEvent, inner_flags: u32, out: &mut Vec<u8>) {
    // Drop release events if inner doesn't want event types
    if ev.event_type == 3 && (inner_flags & REPORT_EVENT_TYPES) == 0 {
        return;
    }

    // Drop standalone modifier key events if inner doesn't want all-keys-as-escapes
    if is_modifier_key(ev.keycode) && (inner_flags & REPORT_ALL_KEYS_AS_ESCAPE_CODES) == 0 {
        return;
    }

    // WezTerm sends the "effective" character as keycode (e.g., Ctrl+a → keycode=1).
    // Kitty at flags=1 sends the base key identity (e.g., Ctrl+a → keycode=97).
    // When the keycode is a C0 control code produced by Ctrl, recover the base key
    // so re-encoded output matches what Kitty-compatible apps expect.
    let ctrl_active = (ev.modifiers.wrapping_sub(1)) & 4 != 0;
    let keycode = if ev.keycode < 32 && ctrl_active {
        ev.base_key.unwrap_or_else(|| reverse_c0(ev.keycode))
    } else {
        ev.keycode
    };

    let modifiers = ev.modifiers;
    let has_modifiers = modifiers != 1;

    // Unmodified printable key → emit plain UTF-8 if inner doesn't want all-keys
    if !has_modifiers
        && dominated_press(ev.event_type, inner_flags)
        && (inner_flags & REPORT_ALL_KEYS_AS_ESCAPE_CODES) == 0
        && is_printable_codepoint(keycode)
    {
        encode_utf8(keycode, out);
        return;
    }

    // Build CSI-u sequence
    out.extend_from_slice(b"\x1b[");

    // Keycode group
    write_u32(keycode, out);
    if (inner_flags & REPORT_ALTERNATE_KEYS) != 0 {
        if ev.shifted_key.is_some() || ev.base_key.is_some() {
            out.push(b':');
            if let Some(sk) = ev.shifted_key {
                write_u32(sk, out);
            }
            if let Some(bk) = ev.base_key {
                out.push(b':');
                write_u32(bk, out);
            }
        }
    }

    // Modifier group — always emit if modifiers != 1, or if we need event_type
    let need_event_type =
        (inner_flags & REPORT_EVENT_TYPES) != 0 && ev.event_type > 0;
    if has_modifiers || need_event_type {
        out.push(b';');
        write_u32(modifiers, out);
        if need_event_type {
            out.push(b':');
            write_u32(ev.event_type, out);
        }
    }

    // Text group
    if (inner_flags & REPORT_ASSOCIATED_TEXT) != 0 && !ev.text_codepoints.is_empty() {
        // Need modifier group separator if not already emitted
        if !has_modifiers && !need_event_type {
            out.push(b';');
            // modifier defaults to 1 when text follows
            out.push(b'1');
        }
        out.push(b';');
        for (i, &cp) in ev.text_codepoints.iter().enumerate() {
            if i > 0 {
                out.push(b':');
            }
            write_u32(cp, out);
        }
    }

    out.push(b'u');
}

/// Check if a codepoint is a printable character (not a functional key).
fn is_printable_codepoint(cp: u32) -> bool {
    // Functional keys use codepoints >= 57344 (Unicode private use area)
    // Also exclude C0 control characters
    cp >= 32 && cp < 57344
}

/// Write a u32 as decimal ASCII digits into a Vec.
fn write_u32(n: u32, out: &mut Vec<u8>) {
    // Simple itoa
    if n == 0 {
        out.push(b'0');
        return;
    }
    let start = out.len();
    let mut val = n;
    while val > 0 {
        out.push(b'0' + (val % 10) as u8);
        val /= 10;
    }
    out[start..].reverse();
}

/// Encode a unicode codepoint as UTF-8.
fn encode_utf8(cp: u32, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    if let Some(c) = char::from_u32(cp) {
        let s = c.encode_utf8(&mut buf);
        out.extend_from_slice(s.as_bytes());
    }
}

/// Re-encode a functional key CSI sequence (terminated by ~, A-H, P, Q, S).
/// Strips event_type if inner flags lack REPORT_EVENT_TYPES.
/// `full_seq` includes ESC [ ... final_byte.
fn reencode_functional(full_seq: &[u8], inner_flags: u32, out: &mut Vec<u8>) {
    // params are between '[' and the final byte
    let params = &full_seq[2..full_seq.len() - 1];
    let final_byte = full_seq[full_seq.len() - 1];

    let s = match std::str::from_utf8(params) {
        Ok(s) => s,
        Err(_) => {
            out.extend_from_slice(full_seq);
            return;
        }
    };

    let mut semicolons: Vec<&str> = s.split(';').collect();

    // Check for event_type in the modifier group (second semicolon-separated part)
    if semicolons.len() >= 2 {
        let mod_part = semicolons[1];
        if mod_part.contains(':') {
            let (_modifiers, event_type) = parse_modifier_group(mod_part);
            // Drop release if inner doesn't want event types
            if (inner_flags & REPORT_EVENT_TYPES) == 0 {
                if event_type == 3 {
                    return;
                }
                // Strip event_type from modifier group
                semicolons[1] = mod_part.split(':').next().unwrap_or(mod_part);
            }
        }
    }

    // Rebuild
    out.extend_from_slice(b"\x1b[");
    for (i, part) in semicolons.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        out.extend_from_slice(part.as_bytes());
    }
    out.push(final_byte);
}

/// True if the final byte indicates a functional key CSI sequence that KKP enhances.
fn is_kkp_functional_final(b: u8) -> bool {
    matches!(b, b'~' | b'A'..=b'H' | b'P' | b'Q' | b'S')
}

impl KkpTranslator {
    pub fn new() -> Self {
        Self {
            inner_flags: 0,
            state: State::Normal,
            seq_buf: Vec::new(),
        }
    }

    pub fn set_inner_flags(&mut self, flags: u32) {
        self.inner_flags = flags;
    }

    /// Translate input data from outer terminal (flags=31) to inner app's KKP level.
    pub fn translate(&mut self, data: &[u8]) -> Vec<u8> {
        // Log raw input bytes for debugging modifier issues
        if data.iter().any(|&b| b == ESC) {
            let hex: Vec<String> = data.iter().map(|b| format!("{:02x}", b)).collect();
            info!(
                inner_flags = self.inner_flags,
                raw_hex = %hex.join(" "),
                "kkp input"
            );
        }
        let mut out = Vec::with_capacity(data.len());

        for &b in data {
            match self.state {
                State::Normal => {
                    if b == ESC {
                        self.seq_buf.clear();
                        self.seq_buf.push(ESC);
                        self.state = State::Esc;
                    } else {
                        out.push(b);
                    }
                }

                State::Esc => {
                    self.seq_buf.push(b);
                    if b == b'[' {
                        self.state = State::Csi;
                    } else {
                        // Not CSI — forward buffered
                        out.extend_from_slice(&self.seq_buf);
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    }
                }

                State::Csi => {
                    self.seq_buf.push(b);
                    if is_csi_param(b) {
                        if self.seq_buf.len() > MAX_SEQ_LEN {
                            out.extend_from_slice(&self.seq_buf);
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if is_csi_intermediate(b) {
                        // Intermediate bytes — keep accumulating
                        if self.seq_buf.len() > MAX_SEQ_LEN {
                            out.extend_from_slice(&self.seq_buf);
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if b == b'u' {
                        // CSI-u sequence complete
                        let params = &self.seq_buf[2..self.seq_buf.len() - 1];
                        // Drop KKP management sequences that arrive on stdin
                        // (terminal responses to queries, echoed push/pop commands):
                        //   \x1b[?...u  — KKP mode query response
                        //   \x1b[>...u  — KKP push echo
                        //   \x1b[<...u  — KKP pop echo
                        if params.first().map_or(false, |&b| matches!(b, b'?' | b'>' | b'<')) {
                            // Silently drop — terminal responses (KKP query/push/pop), not user input
                        } else if self.inner_flags == 0 {
                            // No KKP translation active — pass through unchanged
                            out.extend_from_slice(&self.seq_buf);
                        } else if let Some(ev) = parse_csi_u(params) {
                            let before_len = out.len();
                            reencode_csi_u(&ev, self.inner_flags, &mut out);
                            let raw_seq = String::from_utf8_lossy(&self.seq_buf);
                            let encoded = String::from_utf8_lossy(&out[before_len..]);
                            info!(
                                raw = %raw_seq,
                                keycode = ev.keycode,
                                shifted_key = ?ev.shifted_key,
                                base_key = ?ev.base_key,
                                modifiers = ev.modifiers,
                                event_type = ev.event_type,
                                text = ?ev.text_codepoints,
                                inner_flags = self.inner_flags,
                                output = %encoded,
                                "kkp translate csi-u"
                            );
                        } else {
                            // Couldn't parse — pass through
                            out.extend_from_slice(&self.seq_buf);
                        }
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    } else if is_csi_final(b) {
                        // Other CSI sequence
                        let params = &self.seq_buf[2..self.seq_buf.len() - 1];
                        let has_private_marker = params
                            .first()
                            .map_or(false, |&p| matches!(p, b'?' | b'>' | b'='));
                        if has_private_marker && b == b'c' {
                            // DA response (DA1: \x1b[?...c, DA2: \x1b[>...c) — drop
                        } else if is_kkp_functional_final(b) {
                            reencode_functional(&self.seq_buf, self.inner_flags, &mut out);
                        } else {
                            out.extend_from_slice(&self.seq_buf);
                        }
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    } else {
                        // Unexpected byte — forward everything
                        out.extend_from_slice(&self.seq_buf);
                        self.seq_buf.clear();
                        self.state = State::Normal;
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
    fn modified_key_preserved_at_flags_1() {
        // \x1b[46;10u = period with Shift+Super (modifier param 10)
        // At flags=1, modified keys must stay as CSI-u
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[46;10u");
        assert_eq!(result, b"\x1b[46;10u");
    }

    #[test]
    fn unmodified_printable_to_plain_text() {
        // \x1b[97;1u = 'a' with no modifiers → plain 'a' at flags=1
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[97;1u");
        assert_eq!(result, b"a");
    }

    #[test]
    fn release_event_dropped_at_flags_1() {
        // \x1b[97;1:3u = 'a' release event → dropped at flags=1
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[97;1:3u");
        assert!(result.is_empty());
    }

    #[test]
    fn alternate_key_stripped_at_flags_1() {
        // \x1b[97:65;2u = 'a' with shifted_key=65('A'), Shift modifier
        // At flags=1 (no REPORT_ALTERNATE_KEYS), strip alternate key info
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[97:65;2u");
        assert_eq!(result, b"\x1b[97;2u");
    }

    #[test]
    fn non_kkp_sequences_pass_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Cursor position, mode set, etc.
        let result = t.translate(b"\x1b[1;2H\x1b[?25h");
        assert_eq!(result, b"\x1b[1;2H\x1b[?25h");
    }

    #[test]
    fn partial_sequence_across_chunks() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Split \x1b[46;10u across two chunks
        let r1 = t.translate(b"\x1b[46");
        assert!(r1.is_empty()); // buffered
        let r2 = t.translate(b";10u");
        assert_eq!(r2, b"\x1b[46;10u");
    }

    #[test]
    fn mixed_kkp_and_plain_text() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // plain text + unmodified KKP + plain text
        let result = t.translate(b"hello\x1b[97;1uworld");
        assert_eq!(result, b"helloaworld");
    }

    #[test]
    fn mixed_modified_and_unmodified() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[97;1u\x1b[97;5u");
        // first is unmodified 'a' → plain text; second is Ctrl+a → CSI-u
        assert_eq!(result, b"a\x1b[97;5u");
    }

    #[test]
    fn functional_key_tilde_pass_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Page Up: \x1b[5~
        let result = t.translate(b"\x1b[5~");
        assert_eq!(result, b"\x1b[5~");
    }

    #[test]
    fn functional_key_release_dropped() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Page Up with release event: \x1b[5;1:3~
        let result = t.translate(b"\x1b[5;1:3~");
        assert!(result.is_empty());
    }

    #[test]
    fn functional_key_repeat_passes_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Arrow up repeat: \x1b[1;1:2A → strip event_type → \x1b[1;1A
        let result = t.translate(b"\x1b[1;1:2A");
        assert_eq!(result, b"\x1b[1;1A");
    }

    #[test]
    fn functional_key_event_type_stripped() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Arrow up press: \x1b[1;1:1A → strip event_type → \x1b[1;1A
        let result = t.translate(b"\x1b[1;1:1A");
        assert_eq!(result, b"\x1b[1;1A");
    }

    #[test]
    fn event_type_preserved_when_inner_wants_it() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_EVENT_TYPES); // flags=3
        let result = t.translate(b"\x1b[97;1:1u");
        // Inner wants event types, so preserve. Unmodified but has event_type.
        // modifiers=1, event_type=1 → need to keep since inner wants events
        // But it's a printable unmodified key with no REPORT_ALL_KEYS flag...
        // Actually at flag 3 (DISAMBIGUATE + EVENT_TYPES), unmodified printable
        // without REPORT_ALL_KEYS should still become plain text for press.
        // event_type=1 is press, which is default behavior, so emit plain 'a'.
        assert_eq!(result, b"a");
    }

    #[test]
    fn release_preserved_when_inner_wants_it() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_EVENT_TYPES | REPORT_ALL_KEYS_AS_ESCAPE_CODES); // flags=11
        let result = t.translate(b"\x1b[97;1:3u");
        assert_eq!(result, b"\x1b[97;1:3u");
    }

    #[test]
    fn all_keys_mode_emits_csi_u_for_unmodified() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_ALL_KEYS_AS_ESCAPE_CODES); // flags=9
        let result = t.translate(b"\x1b[97;1u");
        assert_eq!(result, b"\x1b[97u");
    }

    #[test]
    fn alternate_keys_preserved_when_inner_wants_them() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_ALTERNATE_KEYS); // flags=5
        let result = t.translate(b"\x1b[97:65;2u");
        assert_eq!(result, b"\x1b[97:65;2u");
    }

    #[test]
    fn text_codepoints_stripped_when_not_wanted() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // 'a' with Shift, text='A': \x1b[97;2;65u
        let result = t.translate(b"\x1b[97;2;65u");
        assert_eq!(result, b"\x1b[97;2u");
    }

    #[test]
    fn command_c_wezterm_style() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Command-c (Super+c) WezTerm style: \x1b[99::99;9;99u
        // keycode=99, base_key=99, modifiers=9 (Super), text=99
        // Should strip alternate keys and text, output \x1b[99;9u
        let result = t.translate(b"\x1b[99::99;9;99u");
        assert_eq!(result, b"\x1b[99;9u");
    }

    #[test]
    fn text_codepoints_preserved_when_wanted() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_ASSOCIATED_TEXT); // flags=17
        let result = t.translate(b"\x1b[97;2;65u");
        assert_eq!(result, b"\x1b[97;2;65u");
    }

    #[test]
    fn plain_text_passes_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"hello world");
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn esc_non_csi_passes_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // ESC O P (F1 in SS3 encoding)
        let result = t.translate(b"\x1bOP");
        assert_eq!(result, b"\x1bOP");
    }

    #[test]
    fn mouse_sequences_pass_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // SGR mouse: \x1b[<0;10;20M
        let result = t.translate(b"\x1b[<0;10;20M");
        assert_eq!(result, b"\x1b[<0;10;20M");
    }

    #[test]
    fn bare_keycode_no_modifiers() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // \x1b[97u = 'a' with implicit modifiers=1
        let result = t.translate(b"\x1b[97u");
        assert_eq!(result, b"a");
    }

    #[test]
    fn ctrl_letter_stays_csi_u() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+a (Kitty style, keycode=97): \x1b[97;5u → stays CSI-u
        let result = t.translate(b"\x1b[97;5u");
        assert_eq!(result, b"\x1b[97;5u");
    }

    #[test]
    fn ctrl_letter_wezterm_uses_base_key() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+a WezTerm style: \x1b[1::97;5;1u
        // keycode=1 (effective SOH), base_key=97 ('a')
        // Should use base_key → \x1b[97;5u
        let result = t.translate(b"\x1b[1::97;5;1u");
        assert_eq!(result, b"\x1b[97;5u");
    }

    #[test]
    fn ctrl_space_stays_csi_u() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+Space (Kitty style, keycode=32): stays CSI-u
        let result = t.translate(b"\x1b[32;5u");
        assert_eq!(result, b"\x1b[32;5u");
    }

    #[test]
    fn ctrl_space_wezterm_uses_base_key() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+Space WezTerm: \x1b[0::32;5;0u
        // keycode=0 (NUL), base_key=32 (space) → \x1b[32;5u
        let result = t.translate(b"\x1b[0::32;5;0u");
        assert_eq!(result, b"\x1b[32;5u");
    }

    #[test]
    fn ctrl_shift_a_wezterm_uses_base_key() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+Shift+a WezTerm: keycode=1 (C0), base_key=97, modifiers=6 (Shift+Ctrl)
        let result = t.translate(b"\x1b[1::97;6;1u");
        assert_eq!(result, b"\x1b[97;6u");
    }

    #[test]
    fn ctrl_d_wezterm_uses_base_key() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+d WezTerm: keycode=4 (EOT), base_key=100 ('d')
        let result = t.translate(b"\x1b[4::100;5;4u");
        assert_eq!(result, b"\x1b[100;5u");
    }

    #[test]
    fn enter_not_remapped() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Enter (no Ctrl): keycode=13, modifiers=1 → stays \x1b[13u
        let result = t.translate(b"\x1b[13;1u");
        assert_eq!(result, b"\x1b[13u");
    }

    #[test]
    fn ctrl_enter_keeps_keycode() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Ctrl+Enter: keycode=13, base_key=13, modifiers=5
        // keycode < 32 and Ctrl active, but base_key=13 (same), so no change
        let result = t.translate(b"\x1b[13::13;5u");
        assert_eq!(result, b"\x1b[13;5u");
    }

    #[test]
    fn arrow_key_passes_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[A");
        assert_eq!(result, b"\x1b[A");
    }

    #[test]
    fn modified_arrow_key_passes_through() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Shift+Up: \x1b[1;2A
        let result = t.translate(b"\x1b[1;2A");
        assert_eq!(result, b"\x1b[1;2A");
    }

    #[test]
    fn repeat_event_emits_key_at_flags_1() {
        // Repeat events should produce output (key repeat), just strip event_type
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // 'a' repeat: \x1b[97;1:2u → plain 'a' (unmodified printable)
        let result = t.translate(b"\x1b[97;1:2u");
        assert_eq!(result, b"a");
    }

    #[test]
    fn modifier_key_dropped_at_flags_1() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        // Left Control press: \x1b[57442;1u
        let result = t.translate(b"\x1b[57442;1u");
        assert!(result.is_empty());
    }

    #[test]
    fn modifier_key_preserved_when_inner_wants_all_keys() {
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1 | REPORT_ALL_KEYS_AS_ESCAPE_CODES); // flags=9
        // Left Control press: \x1b[57442;1u
        let result = t.translate(b"\x1b[57442;1u");
        assert_eq!(result, b"\x1b[57442u");
    }

    #[test]
    fn kkp_query_response_dropped() {
        // Terminal sends KKP mode report: \x1b[?31u — must be silently dropped
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[?31u");
        assert!(result.is_empty());
    }

    #[test]
    fn kkp_query_response_multi_flags_dropped() {
        // Some terminals may list individual flags: \x1b[?1;2;4;8;16u
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[?1;2;4;8;16u");
        assert!(result.is_empty());
    }

    #[test]
    fn kkp_push_echo_dropped() {
        // If \x1b[>31u somehow arrives on stdin, drop it
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[>31u");
        assert!(result.is_empty());
    }

    #[test]
    fn kkp_pop_echo_dropped() {
        // If \x1b[<u somehow arrives on stdin, drop it
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[<u");
        assert!(result.is_empty());
    }

    #[test]
    fn kkp_response_mixed_with_real_input() {
        // KKP response sandwiched between real keypresses
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[97;1u\x1b[?31u\x1b[98;1u");
        assert_eq!(result, b"ab"); // response dropped, keys preserved
    }

    #[test]
    fn da1_response_dropped() {
        // DA1 response: \x1b[?65;1;9c — terminal response, not user input
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[?65;1;9c");
        assert!(result.is_empty());
    }

    #[test]
    fn da2_response_dropped() {
        // DA2 response: \x1b[>65;6003;1c
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[>65;6003;1c");
        assert!(result.is_empty());
    }

    #[test]
    fn kkp_response_dropped_when_inactive() {
        // KKP query response arrives before KKP is active (inner_flags=0)
        let mut t = KkpTranslator::new();
        // inner_flags defaults to 0
        let result = t.translate(b"\x1b[?31u");
        assert!(result.is_empty());
    }

    #[test]
    fn normal_input_passes_through_when_inactive() {
        // Regular input when KKP is not active should pass through unchanged
        let mut t = KkpTranslator::new();
        let result = t.translate(b"hello\x1b[A\x1b[5~");
        assert_eq!(result, b"hello\x1b[A\x1b[5~");
    }

    #[test]
    fn dec_mode_on_stdin_passes_through() {
        // DEC private mode sequences (\x1b[?25h) should pass through if on stdin
        let mut t = KkpTranslator::new();
        t.set_inner_flags(1);
        let result = t.translate(b"\x1b[?25h");
        assert_eq!(result, b"\x1b[?25h");
    }

    #[test]
    fn unmodified_printable_all_keys_no_modifier_field() {
        // When inner wants all-keys but key has modifiers=1, emit minimal CSI-u
        let mut t = KkpTranslator::new();
        t.set_inner_flags(REPORT_ALL_KEYS_AS_ESCAPE_CODES | 1); // flags=9
        let result = t.translate(b"\x1b[97;1u");
        // Should emit \x1b[97u (no modifier field since it's 1)
        assert_eq!(result, b"\x1b[97u");
    }
}
