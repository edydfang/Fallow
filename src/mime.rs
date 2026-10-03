//! Minimal, allocation-light MIME handling: headers, parameters, RFC 2047/2231
//! decoding, and a structural walk that locates attachment parts by byte offset
//! (so they can be measured, extracted or replaced without re-serializing).

use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use memchr::{memchr, memmem};

const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

// ------------------------------------------------------------------ headers

pub struct Headers {
    list: Vec<(String, String)>,
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.list
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn has(&self, name: &str) -> bool {
        self.list.iter().any(|(n, _)| n == name)
    }
}

/// Offset just past the mbox "From " line (0 if there is none).
pub fn skip_from_line(m: &[u8]) -> usize {
    if m.starts_with(b"From ") {
        memchr(b'\n', m).map_or(m.len(), |i| i + 1)
    } else {
        0
    }
}

/// Split at the first blank line. Returns (header_end, body_start), relative to `p`.
pub fn split_head_body(p: &[u8]) -> (usize, usize) {
    if p.starts_with(b"\n") {
        return (0, 1);
    }
    if p.starts_with(b"\r\n") {
        return (0, 2);
    }
    let a = memmem::find(p, b"\n\n").map(|i| (i + 1, i + 2));
    let b = memmem::find(p, b"\n\r\n").map(|i| (i + 1, i + 3));
    match (a, b) {
        (Some(x), Some(y)) => {
            if x.0 <= y.0 {
                x
            } else {
                y
            }
        }
        (Some(x), None) => x,
        (None, Some(y)) => y,
        (None, None) => (p.len(), p.len()),
    }
}

/// Parse (unfolded) headers, keeping only `wanted` names (lowercase).
pub fn parse_headers(h: &[u8], wanted: &[&str]) -> Headers {
    let mut list: Vec<(String, String)> = Vec::new();
    let mut cur_wanted = false;
    for raw in h.split(|&c| c == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if cur_wanted {
                if let Some(last) = list.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(String::from_utf8_lossy(line).trim());
                }
            }
            continue;
        }
        let Some(colon) = memchr(b':', line) else {
            cur_wanted = false;
            continue;
        };
        let name = String::from_utf8_lossy(&line[..colon])
            .trim()
            .to_ascii_lowercase();
        cur_wanted = wanted.iter().any(|w| *w == name);
        if cur_wanted {
            let val = String::from_utf8_lossy(&line[colon + 1..])
                .trim()
                .to_string();
            list.push((name, val));
        }
    }
    Headers { list }
}

// --------------------------------------------------------------- parameters

fn split_params(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut in_q, mut esc, mut start) = (false, false, 0);
    for (i, c) in v.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_q => esc = true,
            '"' => in_q = !in_q,
            ';' if !in_q => {
                out.push(&v[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&v[start..]);
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        s.to_string()
    }
}

/// Header parameter (e.g. `boundary`, `filename`, `name`), with RFC 2231 and
/// RFC 2047 decoding.
pub fn param(v: &str, key: &str) -> Option<String> {
    let mut pieces: Vec<(u32, String, bool)> = Vec::new();
    for seg in split_params(v).into_iter().skip(1) {
        let Some(eq) = seg.find('=') else { continue };
        let name = seg[..eq].trim().to_ascii_lowercase();
        let val = unquote(&seg[eq + 1..]);
        if name == key {
            return Some(decode_words(&val));
        }
        if let Some(rest) = name.strip_prefix(key).and_then(|r| r.strip_prefix('*')) {
            let enc = rest.is_empty() || rest.ends_with('*');
            let idx: u32 = rest.trim_end_matches('*').parse().unwrap_or(0);
            pieces.push((idx, val, enc));
        }
    }
    if pieces.is_empty() {
        return None;
    }
    pieces.sort_by_key(|p| p.0);
    let mut charset = String::from("utf-8");
    let mut bytes = Vec::new();
    for (i, (_, val, enc)) in pieces.iter().enumerate() {
        if *enc {
            let mut s = val.as_str();
            if i == 0 {
                let parts: Vec<&str> = s.splitn(3, '\'').collect();
                if parts.len() == 3 {
                    if !parts[0].is_empty() {
                        charset = parts[0].to_string();
                    }
                    s = parts[2];
                }
            }
            bytes.extend(pct_decode(s.as_bytes()));
        } else {
            bytes.extend_from_slice(val.as_bytes());
        }
    }
    Some(decode_charset(&bytes, &charset))
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn pct_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            if let (Some(h), Some(l)) = (
                s.get(i + 1).and_then(|&c| hexval(c)),
                s.get(i + 2).and_then(|&c| hexval(c)),
            ) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

pub fn decode_charset(bytes: &[u8], label: &str) -> String {
    let enc =
        encoding_rs::Encoding::for_label(label.trim().as_bytes()).unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

// ------------------------------------------------------------ transfer codes

pub fn b64_decode(s: &[u8]) -> Vec<u8> {
    let mut clean: Vec<u8> = s
        .iter()
        .copied()
        .filter(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/')
        .collect();
    if clean.len() % 4 == 1 {
        clean.pop();
    }
    B64.decode(&clean).unwrap_or_default()
}

/// Quoted-printable (body) or RFC 2047 "Q" (header, `_` = space).
pub fn qp_decode(s: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'=' {
            if s.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if s.get(i + 1) == Some(&b'\r') && s.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if let (Some(h), Some(l)) = (
                s.get(i + 1).and_then(|&c| hexval(c)),
                s.get(i + 2).and_then(|&c| hexval(c)),
            ) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
            out.push(c);
        } else if header && c == b'_' {
            out.push(b' ');
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// RFC 2047 encoded-words ("=?UTF-8?B?...?=", "=?gb2312?Q?...?=") → text.
pub fn decode_words(s: &str) -> String {
    if !s.contains("=?") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut last_word = false;
    while let Some(i) = rest.find("=?") {
        let (before, after) = rest.split_at(i);
        match parse_word(after) {
            Some((text, used)) => {
                if !(last_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = &after[used..];
                last_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &after[2..];
                last_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_word(s: &str) -> Option<(String, usize)> {
    let b = &s[2..];
    let q1 = b.find('?')?;
    let charset = b[..q1].split('*').next().unwrap_or("utf-8");
    let b2 = &b[q1 + 1..];
    let q2 = b2.find('?')?;
    let enc = &b2[..q2];
    let b3 = &b2[q2 + 1..];
    let end = b3.find("?=")?;
    let text = &b3[..end];
    if text.contains(' ') {
        return None;
    }
    let bytes = match enc {
        "B" | "b" => b64_decode(text.as_bytes()),
        "Q" | "q" => qp_decode(text.as_bytes(), true),
        _ => return None,
    };
    Some((
        decode_charset(&bytes, charset),
        2 + q1 + 1 + q2 + 1 + end + 2,
    ))
}

// ----------------------------------------------------------------- addresses

fn is_local(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&c)
}
fn is_dom(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'.' || c == b'-'
}

fn push_addr(out: &mut Vec<String>, a: &str) {
    let a = a
        .trim()
        .trim_start_matches("mailto:")
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c.is_whitespace())
        .to_ascii_lowercase();
    if a.contains('@') && !out.contains(&a) {
        out.push(a);
    }
}

/// Email addresses in an address header. Prefers `<...>` forms; falls back to
/// scanning bare `local@domain` tokens.
pub fn extract_addrs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    if s.contains('<') {
        let mut rest = s;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let Some(j) = after.find('>') else { break };
            push_addr(&mut out, &after[..j]);
            rest = &after[j + 1..];
        }
        if !out.is_empty() {
            return out;
        }
    }
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c != b'@' {
            continue;
        }
        let mut l = i;
        while l > 0 && is_local(b[l - 1]) {
            l -= 1;
        }
        let mut r = i + 1;
        while r < b.len() && is_dom(b[r]) {
            r += 1;
        }
        if l < i && r > i + 1 {
            push_addr(&mut out, &s[l..r]);
        }
    }
    out
}

pub fn domain_of(addr: &str) -> &str {
    addr.rsplit_once('@').map_or(addr, |(_, d)| d)
}

// ------------------------------------------------------- message-level info

pub fn is_bulk(h: &Headers) -> bool {
    h.has("list-unsubscribe")
        || h.has("list-id")
        || h.get("precedence").is_some_and(|p| {
            matches!(
                p.trim().to_ascii_lowercase().as_str(),
                "bulk" | "list" | "junk"
            )
        })
}

/// Gmail Takeout's `X-Gmail-Labels: Inbox,Category Promotions,"a,b"`.
pub fn gmail_labels(h: &Headers) -> Vec<String> {
    let Some(v) = h.get("x-gmail-labels") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let (mut cur, mut in_q) = (String::new(), false);
    for c in v.chars() {
        match c {
            '"' => in_q = !in_q,
            ',' if !in_q => {
                let t = decode_words(cur.trim());
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = decode_words(cur.trim());
    if !t.is_empty() {
        out.push(t);
    }
    out
}

// ------------------------------------------------------------- MIME walking

/// A non-multipart MIME part, with byte offsets into the message.
pub struct Leaf {
    /// Start of the part's headers.
    pub start: usize,
    /// Start of the part's body.
    pub body_start: usize,
    /// End of the part (exclusive; excludes the line break before the next delimiter).
    pub end: usize,
    pub is_attachment: bool,
    pub top_level: bool,
    pub filename: Option<String>,
    pub ctype: String,
    pub encoding: String,
}

impl Leaf {
    /// Approximate decoded size without decoding.
    pub fn decoded_len(&self, msg: &[u8]) -> u64 {
        let body = &msg[self.body_start..self.end];
        if self.encoding == "base64" {
            let ws = memchr::memchr2_iter(b'\n', b'\r', body).count();
            ((body.len() - ws) as u64) * 3 / 4
        } else {
            body.len() as u64
        }
    }
    pub fn decode(&self, msg: &[u8]) -> Vec<u8> {
        let body = &msg[self.body_start..self.end];
        match self.encoding.as_str() {
            "base64" => b64_decode(body),
            "quoted-printable" => qp_decode(body, false),
            _ => body.to_vec(),
        }
    }
}

const PART_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "content-transfer-encoding",
];

/// All leaf parts of a message (`msg` includes the From_ line).
pub fn leaves(msg: &[u8]) -> Vec<Leaf> {
    let mut out = Vec::new();
    walk(msg, skip_from_line(msg), msg.len(), 0, true, &mut out);
    out
}

fn walk(msg: &[u8], start: usize, end: usize, depth: u32, top: bool, out: &mut Vec<Leaf>) {
    if start >= end {
        return;
    }
    let (he, bs) = split_head_body(&msg[start..end]);
    let (he, bs) = (start + he, start + bs);
    let h = parse_headers(&msg[start..he], PART_HEADERS);
    let ct_raw = h.get("content-type").unwrap_or("text/plain");
    let ctype = ct_raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if depth < 32 && ctype.starts_with("multipart/") {
        if let Some(boundary) = param(ct_raw, "boundary").filter(|b| !b.is_empty()) {
            let d = delimiters(msg, bs, end, &boundary);
            if !d.is_empty() {
                for w in d.windows(2) {
                    let (_, cs, close) = w[0];
                    if close {
                        break;
                    }
                    let (next_ls, _, _) = w[1];
                    let ce = if next_ls >= 2 && &msg[next_ls - 2..next_ls] == b"\r\n" {
                        next_ls - 2
                    } else {
                        next_ls.saturating_sub(1)
                    };
                    if ce > cs {
                        walk(msg, cs, ce, depth + 1, false, out);
                    }
                }
                return;
            }
        }
    }
    if depth < 32 && ctype == "message/rfc822" {
        let disp = h
            .get("content-disposition")
            .unwrap_or("")
            .to_ascii_lowercase();
        if !disp.starts_with("attachment") {
            walk(msg, bs, end, depth + 1, false, out);
            return;
        }
    }

    let disp = h.get("content-disposition").unwrap_or("");
    let filename = param(disp, "filename").or_else(|| param(ct_raw, "name"));
    let is_text = ctype == "text/plain" || ctype == "text/html";
    let is_attachment = disp
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("attachment")
        || (filename.is_some() && !is_text);
    let encoding = h
        .get("content-transfer-encoding")
        .unwrap_or("7bit")
        .trim()
        .to_ascii_lowercase();
    out.push(Leaf {
        start,
        body_start: bs,
        end,
        is_attachment,
        top_level: top,
        filename,
        ctype,
        encoding,
    });
}

/// Delimiter lines `--boundary` / `--boundary--` within [bs, be):
/// (line_start, line_end_after_newline, is_close), absolute offsets.
fn delimiters(msg: &[u8], bs: usize, be: usize, boundary: &str) -> Vec<(usize, usize, bool)> {
    let body = &msg[bs..be];
    let pat = format!("--{boundary}");
    let p = pat.as_bytes();
    let check = |ls: usize| -> Option<(usize, usize, bool)> {
        if !body[ls..].starts_with(p) {
            return None;
        }
        let after = ls + p.len();
        let close = body[after..].starts_with(b"--");
        let rs = if close { after + 2 } else { after };
        let le = memchr(b'\n', &body[rs..]).map_or(body.len(), |x| rs + x + 1);
        body[rs..le]
            .iter()
            .all(u8::is_ascii_whitespace)
            .then_some((bs + ls, bs + le, close))
    };
    let mut v = Vec::new();
    if let Some(d) = check(0) {
        v.push(d);
        if d.2 {
            return v;
        }
    }
    let mut nl_pat = Vec::with_capacity(p.len() + 1);
    nl_pat.push(b'\n');
    nl_pat.extend_from_slice(p);
    for off in memmem::find_iter(body, &nl_pat) {
        if let Some(d) = check(off + 1) {
            v.push(d);
            if d.2 {
                break;
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(
            decode_words("=?UTF-8?B?5L2g5aW9?= =?UTF-8?B?5LiW55WM?="),
            "你好世界"
        );
        assert_eq!(decode_words("=?gb2312?B?xOO6ww==?="), "你好");
        assert_eq!(
            decode_words("Re: =?utf-8?Q?caf=C3=A9_au_lait?="),
            "Re: café au lait"
        );
    }

    #[test]
    fn params() {
        assert_eq!(
            param("attachment; filename=\"a b.pdf\"", "filename").unwrap(),
            "a b.pdf"
        );
        assert_eq!(
            param("attachment; filename*=UTF-8''%E6%96%87.pdf", "filename").unwrap(),
            "文.pdf"
        );
        assert_eq!(
            param("multipart/mixed; boundary=XX", "boundary").unwrap(),
            "XX"
        );
    }

    #[test]
    fn addrs() {
        assert_eq!(
            extract_addrs("\"Doe, John\" <J@X.com>, b@y.org"),
            vec!["j@x.com"]
        );
        assert_eq!(
            extract_addrs("a@x.com, B@Y.org"),
            vec!["a@x.com", "b@y.org"]
        );
    }

    #[test]
    fn walk_parts() {
        let m = b"From x Mon Jan  1 00:00:00 2024\r\nContent-Type: multipart/mixed; boundary=\"B\"\r\n\r\npre\r\n--B\r\nContent-Type: text/plain\r\n\r\nhi\r\n--B\r\nContent-Type: application/pdf; name=a.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nQUFB\r\n--B--\r\n";
        let l = leaves(m);
        assert_eq!(l.len(), 2);
        assert!(!l[0].is_attachment);
        assert!(l[1].is_attachment);
        assert_eq!(&m[l[1].body_start..l[1].end], b"QUFB");
        assert_eq!(l[1].decode(m), b"AAA");
    }
}
