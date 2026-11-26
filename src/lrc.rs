//! LRC lyrics file parsing.

use std::path::Path;

/// Time of a line that has no timestamp yet in a partly synced file. Sorts last and never becomes current.
pub const UNTIMED: i64 = i64::MAX;

pub struct Line {
    pub time_ms: i64,
    pub text: String,
}

pub struct Lyrics {
    pub lines: Vec<Line>,
    /// `[offset:]` tag value in ms. Positive = show lyrics earlier.
    pub offset_ms: i64,
    /// False for plain text without any timestamps: line times are then just 0, 1000, 2000...
    /// and get spread over the song duration (see `App::lyric_pos`).
    /// A partly synced file (timed lines, then plain ones) is `synced` with `UNTIMED` lines at the end.
    pub synced: bool,
}

impl Lyrics {
    /// Index of the line active at `pos_ms`, or None if before the first line.
    pub fn line_at(&self, pos_ms: i64) -> Option<usize> {
        let n = self.lines.partition_point(|l| l.time_ms <= pos_ms);
        n.checked_sub(1)
    }

    /// Number of lines with a real timestamp.
    pub fn timed_count(&self) -> usize {
        if self.synced { self.lines.iter().filter(|l| l.time_ms != UNTIMED).count() } else { 0 }
    }
}

pub fn load(path: &Path) -> Option<Lyrics> {
    let bytes = std::fs::read(path).ok()?;
    Some(parse(&decode(&bytes)))
}

/// Decode file bytes: UTF-8 (with/without BOM), UTF-16 LE/BE with BOM, else the system ANSI code page.
pub fn decode(bytes: &[u8]) -> String {
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(b).into_owned();
    }
    if bytes.len() >= 2 && (bytes[..2] == [0xFF, 0xFE] || bytes[..2] == [0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let u: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return String::from_utf16_lossy(&u);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => ansi_to_string(bytes),
    }
}

fn ansi_to_string(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut buf = vec![0u16; n as usize];
        MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut buf));
        String::from_utf16_lossy(&buf)
    }
}

pub fn parse(src: &str) -> Lyrics {
    let mut lines = Vec::new();
    let mut offset_ms = 0;
    let mut plain = Vec::new();
    for raw in src.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        // A line can carry several leading tags: [00:12.00][01:30.00]text
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            if let Some(t) = parse_time(tag) {
                times.push(t);
            } else if let Some(v) = tag.strip_prefix("offset:") {
                offset_ms = v.trim().parse().unwrap_or(0);
            } else if times.is_empty() {
                // Metadata tag like [ar:...]; skip the whole line.
                rest = "";
                times.push(-1);
                break;
            } else {
                break;
            }
            rest = stripped[end + 1..].trim_start();
        }
        if times.is_empty() {
            if !rest.is_empty() {
                plain.push(strip_word_times(rest));
            }
            continue;
        }
        if times == [-1] {
            continue; // metadata line
        }
        let text = strip_word_times(rest.trim());
        for t in times {
            lines.push(Line { time_ms: t, text: text.clone() });
        }
    }
    lines.sort_by_key(|l| l.time_ms); // stable: keeps file order for equal times
    if !lines.is_empty() {
        // Partly synced: plain lines come after the timed ones.
        lines.extend(plain.into_iter().map(|text| Line { time_ms: UNTIMED, text }));
    } else {
        let lines = plain.into_iter().enumerate().map(|(i, text)| Line { time_ms: i as i64 * 1000, text }).collect();
        return Lyrics { lines, offset_ms, synced: false };
    }
    Lyrics { lines, offset_ms, synced: true }
}

/// Parses `mm:ss`, `mm:ss.x`, `mm:ss.xx`, `mm:ss.xxx`, `mm:ss:xx`.
fn parse_time(tag: &str) -> Option<i64> {
    let (m, rest) = tag.split_once(':')?;
    let m: i64 = m.trim().parse().ok()?;
    let (s, frac) = match rest.find(['.', ':']) {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let s: i64 = s.trim().parse().ok()?;
    let frac = frac.trim();
    let frac_ms = if frac.is_empty() {
        0
    } else {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits = &frac[..frac.len().min(3)];
        let v: i64 = digits.parse().ok()?;
        v * 10i64.pow(3 - digits.len() as u32)
    };
    Some(m * 60_000 + s * 1000 + frac_ms)
}

/// Removes enhanced-LRC word timestamps like `<00:12.34>`.
fn strip_word_times(s: &str) -> String {
    if !s.contains('<') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        match rest[i..].find('>') {
            Some(j) if parse_time(&rest[i + 1..i + j]).is_some() => rest = &rest[i + j + 1..],
            _ => {
                out.push('<');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rewrites the `[offset:]` tag of a file in place (adds it if missing).
pub fn save_offset(path: &Path, offset_ms: i64) -> std::io::Result<()> {
    let text = decode(&std::fs::read(path)?);
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + 20);
    if offset_ms != 0 {
        out.push_str(&format!("[offset:{offset_ms}]{nl}"));
    }
    for line in text.lines() {
        if !line.trim_start().starts_with("[offset:") {
            out.push_str(line);
            out.push_str(nl);
        }
    }
    std::fs::write(path, out)
}

/// Builds LRC text: metadata tags from `original` (except `[offset:]`) followed by timed lines.
pub fn synced_text(original: &str, lines: &[(Option<i64>, &str)], nl: &str) -> String {
    let mut out = String::new();
    for l in original.lines().map(str::trim) {
        let is_meta = l.starts_with('[')
            && l.ends_with(']')
            && l[1..].split_once(':').is_some_and(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphabetic()))
            && !l.starts_with("[offset:");
        if is_meta {
            out.push_str(l);
            out.push_str(nl);
        }
    }
    for &(t, text) in lines {
        let Some(t) = t else {
            out.push_str(text); // not timed yet
            out.push_str(nl);
            continue;
        };
        let t = t.max(0);
        out.push_str(&format!("[{:02}:{:02}.{:02}]{text}{nl}", t / 60_000, t / 1000 % 60, t % 1000 / 10));
    }
    out
}

/// Rewrites `path` in place with tapped timings. Lines with `None` stay as plain text,
/// so a partly synced file can be resumed later.
pub fn save_synced(path: &Path, lines: &[(Option<i64>, &str)]) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let text = decode(&bytes);
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    std::fs::write(path, synced_text(&text, lines, nl))
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_synced() {
        let lines = [(Some(1234), "a"), (Some(61_005), "b"), (None, "c")];
        let s = super::synced_text("[ar:A]\n[offset:300]\nplain\n", &lines, "\n");
        assert_eq!(s, "[ar:A]\n[00:01.23]a\n[01:01.00]b\nc\n");
        // Partly synced file: timed lines first, then the untimed rest.
        let l = super::parse(&s);
        assert!(l.synced && l.lines.len() == 3 && l.offset_ms == 0 && l.timed_count() == 2);
        assert_eq!(l.lines[2].time_ms, super::UNTIMED);
        assert_eq!(l.line_at(100_000), Some(1));
    }

    use super::*;

    #[test]
    fn parses_basic() {
        let l = parse("[ar:x]\r\n[offset:+250]\r\n[00:01.50]a\r\n[00:00.15][01:00.123]b <00:02.00>c\r\n[00:03]\r\n");
        assert_eq!(l.offset_ms, 250);
        let v: Vec<_> = l.lines.iter().map(|l| (l.time_ms, l.text.as_str())).collect();
        assert_eq!(v, [(150, "b c"), (1500, "a"), (3000, ""), (60123, "b c")]);
        assert_eq!(l.line_at(100), None);
        assert_eq!(l.line_at(1500), Some(1));
    }
}

#[cfg(test)]
mod file_test {
    #[test]
    fn plain_file() {
        let l = super::parse("[ar:x]
foo

bar <00:01.00>baz
");
        assert!(!l.synced);
        let v: Vec<_> = l.lines.iter().map(|l| (l.time_ms, l.text.as_str())).collect();
        assert_eq!(v, [(0, "foo"), (1000, "bar baz")]);
    }
}
