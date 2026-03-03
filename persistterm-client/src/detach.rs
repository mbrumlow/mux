const PREFIX_RAW: u8 = 0x1C; // Ctrl+backslash as raw byte
const DETACH_RAW: u8 = b'd';
const KILL_RAW: u8 = b'k';
const REFRESH_RAW: u8 = b'r';
const INFO_RAW: u8 = b'i';
const ESC: u8 = 0x1b;
const MAX_SEQ_LEN: usize = 64;

// KKP: backslash is keycode 92, Ctrl modifier param is 5 (1 + 4)
const KKP_BACKSLASH: u32 = 92;
const KKP_D: u32 = 100;
const KKP_K: u32 = 107;
const KKP_R: u32 = 114;
const KKP_I: u32 = 105;

pub struct FilterResult {
    pub forward: Vec<u8>,
    pub detach: bool,
    pub kill: bool,
    pub refresh: bool,
    pub info: bool,
}

enum State {
    Normal,
    GotPrefix,
    Esc,        // saw ESC in Normal context
    Csi,        // saw ESC [ in Normal context, accumulating params
    PrefixEsc,  // saw ESC after GotPrefix
    PrefixCsi,  // saw ESC [ after GotPrefix, accumulating params
}

pub struct DetachFilter {
    state: State,
    seq_buf: Vec<u8>, // buffered bytes of current escape sequence
}

/// Parse CSI u parameters (bytes between '[' and 'u').
/// Returns (keycode, modifier_param) where modifier_param defaults to 1 (none).
fn parse_csi_u(params: &[u8]) -> Option<(u32, u32)> {
    let s = std::str::from_utf8(params).ok()?;
    let mut parts = s.split(';');

    let keycode: u32 = parts.next()?.split(':').next()?.parse().ok()?;

    let modifiers: u32 = match parts.next() {
        Some(mod_part) => mod_part.split(':').next()?.parse().ok().unwrap_or(1),
        None => 1,
    };

    Some((keycode, modifiers))
}

fn is_ctrl_backslash(keycode: u32, modifiers: u32) -> bool {
    // modifier_param = 1 + bits; Ctrl = bit 2 = 4; exactly Ctrl = param 5
    keycode == KKP_BACKSLASH && modifiers == 5
}

fn is_plain_d(keycode: u32, modifiers: u32) -> bool {
    keycode == KKP_D && modifiers == 1
}

fn is_plain_k(keycode: u32, modifiers: u32) -> bool {
    keycode == KKP_K && modifiers == 1
}

fn is_plain_r(keycode: u32, modifiers: u32) -> bool {
    keycode == KKP_R && modifiers == 1
}

fn is_plain_i(keycode: u32, modifiers: u32) -> bool {
    keycode == KKP_I && modifiers == 1
}

/// True for bytes that are CSI parameter characters (digits, ; :)
fn is_csi_param(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b';' | b':')
}

/// True for CSI final bytes
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7E).contains(&b)
}

impl DetachFilter {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            seq_buf: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> FilterResult {
        let mut forward = Vec::with_capacity(chunk.len());

        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            i += 1;

            match self.state {
                State::Normal => {
                    if b == PREFIX_RAW {
                        self.state = State::GotPrefix;
                    } else if b == ESC {
                        self.seq_buf.clear();
                        self.seq_buf.push(ESC);
                        self.state = State::Esc;
                    } else {
                        forward.push(b);
                    }
                }

                State::Esc => {
                    self.seq_buf.push(b);
                    if b == b'[' {
                        self.state = State::Csi;
                    } else {
                        // Not a CSI sequence, forward buffered bytes
                        forward.extend_from_slice(&self.seq_buf);
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    }
                }

                State::Csi => {
                    self.seq_buf.push(b);
                    if is_csi_param(b) {
                        if self.seq_buf.len() > MAX_SEQ_LEN {
                            // Too long, forward and bail
                            forward.extend_from_slice(&self.seq_buf);
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if b == b'u' {
                        // CSI u sequence complete — check if it's Ctrl+backslash
                        // params are between '[' and 'u': seq_buf = [ESC, '[', ...params..., 'u']
                        let params = &self.seq_buf[2..self.seq_buf.len() - 1];
                        if let Some((kc, mods)) = parse_csi_u(params) {
                            if is_ctrl_backslash(kc, mods) {
                                // This is our prefix key — consume it
                                self.seq_buf.clear();
                                self.state = State::GotPrefix;
                            } else {
                                forward.extend_from_slice(&self.seq_buf);
                                self.seq_buf.clear();
                                self.state = State::Normal;
                            }
                        } else {
                            forward.extend_from_slice(&self.seq_buf);
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if is_csi_final(b) {
                        // Some other CSI sequence, forward it
                        forward.extend_from_slice(&self.seq_buf);
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    } else {
                        // Unexpected byte in CSI, forward everything
                        forward.extend_from_slice(&self.seq_buf);
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    }
                }

                State::GotPrefix => {
                    if b == DETACH_RAW {
                        self.state = State::Normal;
                        return FilterResult {
                            forward,
                            detach: true,
                            kill: false,
                            refresh: false,
                            info: false,
                        };
                    } else if b == KILL_RAW {
                        self.state = State::Normal;
                        return FilterResult {
                            forward,
                            detach: false,
                            kill: true,
                            refresh: false,
                            info: false,
                        };
                    } else if b == REFRESH_RAW {
                        self.state = State::Normal;
                        return FilterResult {
                            forward,
                            detach: false,
                            kill: false,
                            refresh: true,
                            info: false,
                        };
                    } else if b == INFO_RAW {
                        self.state = State::Normal;
                        return FilterResult {
                            forward,
                            detach: false,
                            kill: false,
                            refresh: false,
                            info: true,
                        };
                    } else if b == PREFIX_RAW {
                        // Raw escape: forward one literal 0x1C
                        forward.push(PREFIX_RAW);
                        self.state = State::Normal;
                    } else if b == ESC {
                        self.seq_buf.clear();
                        self.seq_buf.push(ESC);
                        self.state = State::PrefixEsc;
                    } else {
                        // Unknown after prefix — swallow both
                        self.state = State::Normal;
                    }
                }

                State::PrefixEsc => {
                    self.seq_buf.push(b);
                    if b == b'[' {
                        self.state = State::PrefixCsi;
                    } else {
                        // Not CSI after prefix — swallow everything
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    }
                }

                State::PrefixCsi => {
                    self.seq_buf.push(b);
                    if is_csi_param(b) {
                        if self.seq_buf.len() > MAX_SEQ_LEN {
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if b == b'u' {
                        let params = &self.seq_buf[2..self.seq_buf.len() - 1];
                        if let Some((kc, mods)) = parse_csi_u(params) {
                            if is_plain_d(kc, mods) {
                                self.seq_buf.clear();
                                self.state = State::Normal;
                                return FilterResult {
                                    forward,
                                    detach: true,
                                    kill: false,
                                    refresh: false,
                                    info: false,
                                };
                            } else if is_plain_k(kc, mods) {
                                self.seq_buf.clear();
                                self.state = State::Normal;
                                return FilterResult {
                                    forward,
                                    detach: false,
                                    kill: true,
                                    refresh: false,
                                    info: false,
                                };
                            } else if is_plain_r(kc, mods) {
                                self.seq_buf.clear();
                                self.state = State::Normal;
                                return FilterResult {
                                    forward,
                                    detach: false,
                                    kill: false,
                                    refresh: true,
                                    info: false,
                                };
                            } else if is_plain_i(kc, mods) {
                                self.seq_buf.clear();
                                self.state = State::Normal;
                                return FilterResult {
                                    forward,
                                    detach: false,
                                    kill: false,
                                    refresh: false,
                                    info: true,
                                };
                            } else if is_ctrl_backslash(kc, mods) {
                                // KKP escape: forward one KKP Ctrl+\ sequence
                                forward.extend_from_slice(&self.seq_buf);
                                self.seq_buf.clear();
                                self.state = State::Normal;
                            } else {
                                // Unknown KKP key after prefix — swallow
                                self.seq_buf.clear();
                                self.state = State::Normal;
                            }
                        } else {
                            self.seq_buf.clear();
                            self.state = State::Normal;
                        }
                    } else if is_csi_final(b) {
                        // Non-u CSI after prefix — swallow
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    } else {
                        self.seq_buf.clear();
                        self.state = State::Normal;
                    }
                }
            }
        }

        FilterResult {
            forward,
            detach: false,
            kill: false,
            refresh: false,
            info: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Raw byte (non-KKP) tests ──────────────────────────────

    #[test]
    fn pass_through() {
        let mut f = DetachFilter::new();
        let r = f.feed(b"hello world");
        assert_eq!(r.forward, b"hello world");
        assert!(!r.detach);
    }

    #[test]
    fn raw_detach() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, DETACH_RAW]);
        assert!(r.forward.is_empty());
        assert!(r.detach);
    }

    #[test]
    fn raw_escape_sends_literal() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, PREFIX_RAW]);
        assert_eq!(r.forward, vec![PREFIX_RAW]);
        assert!(!r.detach);
    }

    #[test]
    fn raw_swallow_unknown() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, b'x']);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
    }

    #[test]
    fn raw_split_across_chunks() {
        let mut f = DetachFilter::new();
        let r1 = f.feed(&[PREFIX_RAW]);
        assert!(r1.forward.is_empty());
        assert!(!r1.detach);
        let r2 = f.feed(&[DETACH_RAW]);
        assert!(r2.forward.is_empty());
        assert!(r2.detach);
    }

    #[test]
    fn raw_mid_chunk_detach() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[b'a', b'b', PREFIX_RAW, DETACH_RAW, b'c']);
        assert_eq!(r.forward, b"ab");
        assert!(r.detach);
    }

    #[test]
    fn raw_trailing_prefix() {
        let mut f = DetachFilter::new();
        let r1 = f.feed(b"abc\x1C");
        assert_eq!(r1.forward, b"abc");
        assert!(!r1.detach);
        let r2 = f.feed(b"z");
        assert!(r2.forward.is_empty());
        assert!(!r2.detach);
    }

    #[test]
    fn raw_normal_after_swallow() {
        let mut f = DetachFilter::new();
        f.feed(&[PREFIX_RAW, b'x']);
        let r = f.feed(b"hello");
        assert_eq!(r.forward, b"hello");
        assert!(!r.detach);
    }

    // ── KKP (CSI u) tests ─────────────────────────────────────

    // Ctrl+\ in KKP: ESC [ 92 ; 5 u
    const KKP_CTRL_BSLASH: &[u8] = b"\x1b[92;5u";
    // 'd' in KKP: ESC [ 100 ; 1 u  (report-all-keys mode)
    const KKP_D_KEY: &[u8] = b"\x1b[100;1u";
    // 'd' in KKP with no modifier field: ESC [ 100 u
    const KKP_D_BARE: &[u8] = b"\x1b[100u";

    #[test]
    fn kkp_detach() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_D_KEY);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.detach);
    }

    #[test]
    fn kkp_detach_bare_d() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_D_BARE);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.detach);
    }

    #[test]
    fn kkp_detach_raw_d_after_kkp_prefix() {
        // KKP prefix followed by raw 'd' (non-report-all-keys mode)
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.push(b'd');
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.detach);
    }

    #[test]
    fn kkp_escape_forwards_one() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_CTRL_BSLASH);
        let r = f.feed(&input);
        assert_eq!(r.forward, KKP_CTRL_BSLASH);
        assert!(!r.detach);
    }

    #[test]
    fn kkp_swallow_unknown() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(b"\x1b[120;1u"); // some other key
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
    }

    #[test]
    fn kkp_split_across_chunks() {
        let mut f = DetachFilter::new();
        let r1 = f.feed(KKP_CTRL_BSLASH);
        assert!(r1.forward.is_empty());
        assert!(!r1.detach);
        let r2 = f.feed(KKP_D_KEY);
        assert!(r2.forward.is_empty());
        assert!(r2.detach);
    }

    #[test]
    fn kkp_prefix_split_mid_sequence() {
        // Split the CSI sequence itself across chunks
        let mut f = DetachFilter::new();
        let r1 = f.feed(b"\x1b[92");
        assert!(r1.forward.is_empty());
        assert!(!r1.detach);
        let r2 = f.feed(b";5u");
        assert!(r2.forward.is_empty()); // prefix consumed
        assert!(!r2.detach);
        let r3 = f.feed(b"d");
        assert!(r3.forward.is_empty());
        assert!(r3.detach);
    }

    #[test]
    fn kkp_with_event_type() {
        // Ctrl+\ with event type: ESC [ 92 ; 5:1 u  (press event)
        let mut f = DetachFilter::new();
        let r = f.feed(b"\x1b[92;5:1u\x1b[100;1:1u");
        assert!(r.forward.is_empty());
        assert!(r.detach);
    }

    #[test]
    fn other_csi_sequences_pass_through() {
        let mut f = DetachFilter::new();
        // Arrow key, cursor position, etc.
        let r = f.feed(b"\x1b[A\x1b[1;2H\x1b[?25h");
        assert_eq!(r.forward, b"\x1b[A\x1b[1;2H\x1b[?25h");
        assert!(!r.detach);
    }

    #[test]
    fn mixed_kkp_and_normal_text() {
        let mut f = DetachFilter::new();
        let r = f.feed(b"abc\x1b[65;1udef");
        // \x1b[65;1u is KKP for 'A' (keycode 65), should pass through
        assert_eq!(r.forward, b"abc\x1b[65;1udef");
        assert!(!r.detach);
    }

    // ── Kill (C-\ k) tests ──────────────────────────────────────

    #[test]
    fn raw_kill() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, KILL_RAW]);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
        assert!(r.kill);
    }

    #[test]
    fn raw_kill_mid_chunk() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[b'a', b'b', PREFIX_RAW, KILL_RAW, b'c']);
        assert_eq!(r.forward, b"ab");
        assert!(!r.detach);
        assert!(r.kill);
    }

    // 'k' in KKP: ESC [ 107 ; 1 u
    const KKP_K_KEY: &[u8] = b"\x1b[107;1u";
    // 'k' in KKP with no modifier field: ESC [ 107 u
    const KKP_K_BARE: &[u8] = b"\x1b[107u";

    #[test]
    fn kkp_kill() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_K_KEY);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
        assert!(r.kill);
    }

    #[test]
    fn kkp_kill_bare_k() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_K_BARE);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
        assert!(r.kill);
    }

    #[test]
    fn kkp_kill_raw_k_after_kkp_prefix() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.push(b'k');
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(!r.detach);
        assert!(r.kill);
    }

    #[test]
    fn raw_detach_not_kill() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, DETACH_RAW]);
        assert!(r.detach);
        assert!(!r.kill);
    }

    #[test]
    fn raw_kill_not_detach() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, KILL_RAW]);
        assert!(!r.detach);
        assert!(r.kill);
    }

    // ── Info (C-\ i) tests ──────────────────────────────────────

    #[test]
    fn raw_info() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[PREFIX_RAW, INFO_RAW]);
        assert!(r.forward.is_empty());
        assert!(r.info);
        assert!(!r.detach);
        assert!(!r.kill);
        assert!(!r.refresh);
    }

    #[test]
    fn raw_info_mid_chunk() {
        let mut f = DetachFilter::new();
        let r = f.feed(&[b'a', b'b', PREFIX_RAW, INFO_RAW, b'c']);
        assert_eq!(r.forward, b"ab");
        assert!(r.info);
    }

    // 'i' in KKP: ESC [ 105 ; 1 u
    const KKP_I_KEY: &[u8] = b"\x1b[105;1u";
    // 'i' in KKP with no modifier field: ESC [ 105 u
    const KKP_I_BARE: &[u8] = b"\x1b[105u";

    #[test]
    fn kkp_info() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_I_KEY);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.info);
        assert!(!r.detach);
    }

    #[test]
    fn kkp_info_bare_i() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.extend_from_slice(KKP_I_BARE);
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.info);
    }

    #[test]
    fn kkp_info_raw_i_after_kkp_prefix() {
        let mut f = DetachFilter::new();
        let mut input = Vec::new();
        input.extend_from_slice(KKP_CTRL_BSLASH);
        input.push(b'i');
        let r = f.feed(&input);
        assert!(r.forward.is_empty());
        assert!(r.info);
    }
}
