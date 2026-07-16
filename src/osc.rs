//! OSC 通知扫描 + 标题 spinner 判定——GUI（`workspace/terminal.rs`）和守护
//! （`smeltd.rs` 的 `StateListener`）共用一份，跨 bin 用 `#[path]` 引入（跟
//! `remote_gateway.rs` 同一个套路），不复制第二份（CLAUDE.md 明令）。
//!
//! 两个信源都在这：
//! - `OscScan`：OSC 9 / 99 / 777 通知（alacritty 不解析，逐字节自己扫，跟 cmux 同协议）
//! - `title_starts_with_spinner`：OSC 0/2 标题的 Braille spinner 前缀猜测（可信度
//!   最低，纯猜——见 docs/state-channel-plan.md 的信源分层）

/// OSC 9 / 99 / 777 通知扫描：提取 `ESC ] … (BEL|ST)`，跨 `feed` 调用保持状态
/// （字节可能跨 PTY read 边界断开）。
#[derive(Default)]
pub struct OscScan {
    prev_esc: bool,
    in_osc: bool,
    buf: Vec<u8>,
}

impl OscScan {
    /// 喂一个字节；扫到一条完整的 OSC 9/99/777 通知就返回 `Some(消息文本)`，
    /// 调用方自己决定拿这条消息去做什么（GUI 弹通知 / 守护写进 SessionState）。
    pub fn feed(&mut self, b: u8) -> Option<String> {
        if self.in_osc {
            if b == 0x07 {
                return self.finish(); // BEL 结束
            }
            if self.prev_esc && b == 0x5c {
                self.buf.pop(); // 去掉刚推入的 ESC，ST（ESC \）结束
                return self.finish();
            }
            self.buf.push(b);
            self.prev_esc = b == 0x1b;
            if self.buf.len() > 4096 {
                self.reset(); // 异常超长，丢弃
            }
        } else if self.prev_esc && b == 0x5d {
            self.in_osc = true; // ESC ] 进入 OSC
            self.buf.clear();
            self.prev_esc = false;
        } else {
            self.prev_esc = b == 0x1b;
        }
        None
    }

    fn finish(&mut self) -> Option<String> {
        let msg = std::str::from_utf8(&self.buf).ok().and_then(|s| {
            let (ps, pt) = s.split_once(';')?;
            notify_text_from_osc(ps, pt)
        });
        self.reset();
        msg
    }

    fn reset(&mut self) {
        self.in_osc = false;
        self.prev_esc = false;
        self.buf.clear();
    }
}

/// 从 OSC 参数字符串抽出给人看的通知正文。
///
/// - **9**：`9;消息`
/// - **777**：`777;notify;title;body` → 取最后一段
/// - **99**（Kitty）：`99;metadata;payload`；`e=1` 时 payload 为 base64
pub fn notify_text_from_osc(ps: &str, pt: &str) -> Option<String> {
    match ps {
        "9" => {
            let msg = pt.trim();
            if msg.is_empty() {
                None
            } else {
                Some(msg.to_string())
            }
        }
        "777" => {
            let msg = pt.rsplit(';').next().unwrap_or(pt).trim();
            if msg.is_empty() {
                None
            } else {
                Some(msg.to_string())
            }
        }
        "99" => parse_osc99_payload(pt),
        _ => None,
    }
}

/// Kitty desktop notification（OSC 99）：`metadata;payload`。
fn parse_osc99_payload(pt: &str) -> Option<String> {
    let pt = pt.trim();
    if pt.is_empty() {
        return None;
    }

    // 首段含 `=` → 当 Kitty metadata；否则整段纯文本（部分工具简化写法）。
    let (meta, payload) = match pt.split_once(';') {
        Some((m, p)) if m.contains('=') => (Some(m), p),
        _ => (None, pt),
    };

    let mut b64 = false;
    if let Some(m) = meta {
        for kv in m.split(':') {
            if let Some((k, v)) = kv.split_once('=') {
                if k == "e" && v == "1" {
                    b64 = true;
                }
            }
        }
    }

    let payload = payload.trim();
    if payload.is_empty() {
        return None;
    }

    if b64 {
        let decoded = decode_base64_utf8(payload)?;
        let s = decoded.trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        Some(payload.to_string())
    }
}

/// 标准 base64（可含 `=` 填充、忽略空白）→ UTF-8 字符串。无外部 crate。
fn decode_base64_utf8(s: &str) -> Option<String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let a = bytes[i];
        let b = bytes[i + 1];
        let c = bytes[i + 2];
        let d = bytes[i + 3];
        i += 4;
        let va = val(a)?;
        let vb = val(b)?;
        out.push((va << 2) | (vb >> 4));
        if c != b'=' {
            let vc = val(c)?;
            out.push((vb << 4) | (vc >> 2));
            if d != b'=' {
                let vd = val(d)?;
                out.push((vc << 6) | vd);
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 标题是否以 Braille spinner（U+2801–U+28FF，盲文块非空白帧）开头——终端协议约定，
/// 任何遵守此约定的 agent（Claude Code 等）都能被识别，不是某家私有格式。
pub fn title_starts_with_spinner(title: &str) -> bool {
    title
        .chars()
        .next()
        .is_some_and(|c| ('\u{2801}'..='\u{28FF}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(bytes: &[u8]) -> Option<String> {
        let mut scan = OscScan::default();
        let mut got = None;
        for &b in bytes {
            if let Some(m) = scan.feed(b) {
                got = Some(m);
            }
        }
        got
    }

    #[test]
    fn scans_osc9_terminated_by_bel() {
        assert_eq!(
            scan_all(b"\x1b]9;hello world\x07").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn scans_osc777_terminated_by_st_and_takes_last_segment() {
        assert_eq!(
            scan_all(b"\x1b]777;notify;title;body text\x1b\\").as_deref(),
            Some("body text")
        );
    }

    #[test]
    fn scans_osc99_kitty_metadata_payload() {
        assert_eq!(
            notify_text_from_osc("99", "i=1:d=0:p=body;Waiting for approval").as_deref(),
            Some("Waiting for approval")
        );
        assert_eq!(
            scan_all(b"\x1b]99;i=1:p=body;Agent needs input\x1b\\").as_deref(),
            Some("Agent needs input")
        );
    }

    #[test]
    fn osc99_plain_and_base64() {
        assert_eq!(
            notify_text_from_osc("99", "just a plain note").as_deref(),
            Some("just a plain note")
        );
        let msg = notify_text_from_osc("99", "e=1:p=body;SGkg5om5").unwrap();
        assert_eq!(msg, "Hi 批");
    }

    #[test]
    fn ignores_unrelated_osc_codes() {
        assert_eq!(scan_all(b"\x1b]0;window title\x07"), None);
        assert!(notify_text_from_osc("52", "c;xxx").is_none());
    }

    #[test]
    fn state_persists_across_feed_calls_split_at_arbitrary_boundary() {
        let full = b"\x1b]9;split across reads\x07";
        let mut scan = OscScan::default();
        let mut got = None;
        for chunk in full.chunks(3) {
            for &b in chunk {
                if let Some(m) = scan.feed(b) {
                    got = Some(m);
                }
            }
        }
        assert_eq!(got.as_deref(), Some("split across reads"));
    }

    #[test]
    fn oversized_buffer_resets_instead_of_growing_forever() {
        let mut scan = OscScan::default();
        scan.feed(0x1b);
        scan.feed(b']');
        for _ in 0..5000 {
            scan.feed(b'x');
        }
        let mut got = None;
        for &b in b"\x1b]9;still works\x07" {
            if let Some(m) = scan.feed(b) {
                got = Some(m);
            }
        }
        assert_eq!(got.as_deref(), Some("still works"));
    }

    #[test]
    fn title_starts_with_spinner_matches_braille_range() {
        assert!(title_starts_with_spinner("⠋ doing something"));
        assert!(!title_starts_with_spinner("plain title"));
        assert!(!title_starts_with_spinner(""));
    }
}
