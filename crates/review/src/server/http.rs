//! A small HTTP/1.1 reader and writer: enough for the review page, with every size and time
//! bounded. Requests are hostile: a head over [`MAX_HEAD`], a body over the caller's cap, chunked
//! bodies, bad numbers and bad Range headers are refused, never trusted.

use std::io::{self, BufRead, Read, Write};

/// Largest request line plus headers.
pub const MAX_HEAD: usize = 16 * 1024;
/// Most headers in one request.
const MAX_HEADERS: usize = 64;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The path, percent-decoding not applied (the routes only use ASCII ids).
    pub path: String,
    pub query: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// HTTP/1.0, or `Connection: close`.
    pub close: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The client closed the connection (or went quiet) before a request started.
    Closed,
    /// 400: malformed.
    Bad(&'static str),
    /// 413 / 431: too large.
    TooLarge,
    /// 411 / 501: a body we don't take (chunked).
    Unsupported,
}

impl From<io::Error> for ReadError {
    fn from(_: io::Error) -> Self {
        ReadError::Closed
    }
}

/// Read one request. `max_body` caps `Content-Length` (bodies are only for comment posts).
pub fn read_request(r: &mut impl BufRead, max_body: usize) -> Result<Request, ReadError> {
    let mut head = Vec::with_capacity(512);
    let mut lines: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut line = Vec::new();
        let room = MAX_HEAD.saturating_sub(head.len()) as u64 + 1;
        let n = r.by_ref().take(room).read_until(b'\n', &mut line)?;
        if n == 0 {
            return Err(if lines.is_empty() { ReadError::Closed } else { ReadError::Bad("truncated request") });
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD {
            return Err(ReadError::TooLarge);
        }
        if line.last() != Some(&b'\n') {
            return Err(ReadError::Bad("truncated request"));
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            if lines.is_empty() {
                // a stray empty line before the request line is allowed (RFC 9112 §2.2)
                continue;
            }
            break;
        }
        if lines.len() > MAX_HEADERS {
            return Err(ReadError::TooLarge);
        }
        lines.push(line);
    }
    let first = std::str::from_utf8(&lines[0]).map_err(|_| ReadError::Bad("request line is not text"))?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(ReadError::Bad("bad request line"));
    };
    if !method.bytes().all(|b| b.is_ascii_uppercase()) || method.is_empty() || method.len() > 10 {
        return Err(ReadError::Bad("bad method"));
    }
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return Err(ReadError::Bad("unsupported HTTP version")),
    };
    if !target.starts_with('/') || target.bytes().any(|b| !(0x21..0x7f).contains(&b)) {
        return Err(ReadError::Bad("bad target"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = Vec::with_capacity(lines.len());
    for l in &lines[1..] {
        let l = std::str::from_utf8(l).map_err(|_| ReadError::Bad("header is not text"))?;
        let (name, value) = l.split_once(':').ok_or(ReadError::Bad("bad header"))?;
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b >= 0x7f) {
            return Err(ReadError::Bad("bad header name"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut req = Request { method: method.into(), path: path.into(), query: query.into(), headers, body: Vec::new(), close: http10 };
    if let Some(c) = req.header("connection") {
        if c.eq_ignore_ascii_case("close") {
            req.close = true;
        } else if c.eq_ignore_ascii_case("keep-alive") {
            req.close = false;
        }
    }
    if req.header("transfer-encoding").is_some() {
        return Err(ReadError::Unsupported);
    }
    let lens: Vec<&str> = req.headers.iter().filter(|(n, _)| n == "content-length").map(|(_, v)| v.as_str()).collect();
    if lens.len() > 1 {
        return Err(ReadError::Bad("several Content-Length headers"));
    }
    if let Some(v) = lens.first() {
        if v.is_empty() || v.len() > 12 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ReadError::Bad("bad Content-Length"));
        }
        let n: usize = v.parse().map_err(|_| ReadError::Bad("bad Content-Length"))?;
        if n > max_body {
            return Err(ReadError::TooLarge);
        }
        let mut body = vec![0u8; n];
        r.read_exact(&mut body).map_err(|_| ReadError::Bad("body cut short"))?;
        req.body = body;
    }
    Ok(req)
}

/// The byte range a `Range` header asks for, against a file of `len` bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Range {
    /// No (usable) Range header: the whole file.
    Full,
    /// Inclusive `start..=end`.
    Part(u64, u64),
    /// 416.
    Unsatisfiable,
}

/// Parse a `Range` header. Only one `bytes` range is served; a list or another unit gets the
/// whole file (allowed by RFC 9110 §14.2); garbage gets the whole file too.
pub fn parse_range(header: Option<&str>, len: u64) -> Range {
    let Some(h) = header else { return Range::Full };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Range::Full };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Range::Full };
    let num = |s: &str| -> Option<u64> { (!s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse().ok()).flatten() };
    match (a.trim(), b.trim()) {
        ("", suffix) => match num(suffix) {
            Some(0) => Range::Unsatisfiable,
            Some(_) if len == 0 => Range::Unsatisfiable,
            Some(n) => Range::Part(len.saturating_sub(n), len - 1),
            None => Range::Full,
        },
        (start, "") => match num(start) {
            Some(s) if s < len => Range::Part(s, len - 1),
            Some(_) => Range::Unsatisfiable,
            None => Range::Full,
        },
        (start, end) => match (num(start), num(end)) {
            (Some(s), Some(e)) if s <= e && s < len => Range::Part(s, e.min(len - 1)),
            (Some(s), Some(e)) if s <= e => Range::Unsatisfiable,
            _ => Range::Full,
        },
    }
}

/// Headers every response carries.
pub const SECURITY_HEADERS: &[(&str, &str)] =
    &[("X-Content-Type-Options", "nosniff"), ("Referrer-Policy", "no-referrer"), ("X-Frame-Options", "DENY"), ("Cross-Origin-Resource-Policy", "same-origin")];

/// The page's Content Security Policy: our own scripts only, no inline script, wasm allowed.
pub const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; media-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// [`CSP`] that also allows one inline script, by its SHA-256 (`'sha256-<base64>'`).
pub fn csp_with_script(script: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = base64(&Sha256::digest(script.as_bytes()));
    CSP.replacen("script-src 'self'", &format!("script-src 'self' 'sha256-{hash}'"), 1)
}

/// Standard base64 with padding.
fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let at = |i: usize| u32::from(c.get(i).copied().unwrap_or(0));
        let n = (at(0) << 16) | (at(1) << 8) | at(2);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= c.len() {
                out.push(char::from(A[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        303 => "See Other",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Unknown",
    }
}

/// Write a status line and headers (with the security headers and `Connection`).
pub fn write_head(w: &mut impl Write, status: u16, headers: &[(&str, String)], close: bool) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (n, v) in SECURITY_HEADERS {
        out.push_str(n);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    for (n, v) in headers {
        // a value never carries a line break (it would start a new header)
        let v: String = v.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
        out.push_str(n);
        out.push_str(": ");
        out.push_str(&v);
        out.push_str("\r\n");
    }
    out.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
    w.write_all(out.as_bytes())
}

/// A complete response with a body in memory.
pub fn respond(w: &mut impl Write, status: u16, content_type: &str, extra: &[(&str, String)], body: &[u8], head_only: bool, close: bool) -> io::Result<()> {
    let mut h: Vec<(&str, String)> = vec![("Content-Type", content_type.into()), ("Content-Length", body.len().to_string())];
    h.extend(extra.iter().cloned());
    write_head(w, status, &h, close)?;
    if !head_only {
        w.write_all(body)?;
    }
    w.flush()
}

/// `application/x-www-form-urlencoded` pairs (the no-script comment form). Bad escapes are kept
/// as typed; invalid UTF-8 is replaced.
pub fn form_pairs(body: &[u8]) -> Vec<(String, String)> {
    fn decode(s: &[u8]) -> String {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < s.len() => {
                    let hex = |c: u8| (c as char).to_digit(16);
                    match (hex(s[i + 1]), hex(s[i + 2])) {
                        (Some(a), Some(b)) => {
                            out.push((a * 16 + b) as u8);
                            i += 2;
                        }
                        _ => out.push(b'%'),
                    }
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    body.split(|&c| c == b'&')
        .filter(|p| !p.is_empty())
        .take(16)
        .map(|p| match p.iter().position(|&c| c == b'=') {
            Some(i) => (decode(&p[..i]), decode(&p[i + 1..])),
            None => (decode(p), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8]) -> Result<Request, ReadError> {
        read_request(&mut io::BufReader::new(raw), 1024)
    }

    #[test]
    fn requests_parse() {
        let r = read(b"GET /r/abc/v/hd.mp4?x=1 HTTP/1.1\r\nHost: a\r\nRange: bytes=0-1\r\n\r\n").unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str(), r.query.as_str(), r.close), ("GET", "/r/abc/v/hd.mp4", "x=1", false));
        assert_eq!(r.header("range"), Some("bytes=0-1"));
        let r = read(b"POST /c HTTP/1.0\r\nContent-Length: 3\r\n\r\nabcEXTRA").unwrap();
        assert_eq!((r.body.as_slice(), r.close), (&b"abc"[..], true));
    }

    #[test]
    fn hostile_requests_are_refused() {
        assert_eq!(read(b"").unwrap_err(), ReadError::Closed);
        assert_eq!(read(b"GET / HTTP/1.1\r\nHost").unwrap_err(), ReadError::Bad("truncated request"));
        assert!(matches!(read(b"GET / HTTP/2\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"GET  / HTTP/1.1\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"get / HTTP/1.1\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"GET http://x/ HTTP/1.1\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"GET /\x01 HTTP/1.1\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"GET / HTTP/1.1\r\nNoColon\r\n\r\n"), Err(ReadError::Bad(_))));
        assert_eq!(read(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err(), ReadError::Unsupported);
        assert_eq!(read(b"POST / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n").unwrap_err(), ReadError::TooLarge);
        assert!(matches!(read(b"POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"), Err(ReadError::Bad(_))));
        assert!(matches!(read(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nab"), Err(ReadError::Bad(_))));
        let mut huge = b"GET / HTTP/1.1\r\n".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_HEAD * 2));
        assert_eq!(read(&huge).unwrap_err(), ReadError::TooLarge);
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..200 {
            many.extend(format!("X-{i}: y\r\n").bytes());
        }
        many.extend(b"\r\n");
        assert_eq!(read(&many).unwrap_err(), ReadError::TooLarge);
        // bit flips and truncations of a good request never panic
        let good = b"POST /r/x/comments HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nRange: bytes=1-2\r\n\r\nhello".to_vec();
        for i in 0..good.len() {
            let _ = read(&good[..i]);
            for bit in 0..8 {
                let mut b = good.clone();
                b[i] ^= 1 << bit;
                let _ = read(&b);
            }
        }
    }

    #[test]
    fn ranges_are_bounded() {
        assert_eq!(parse_range(None, 100), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-1"), 100), Range::Part(0, 1));
        assert_eq!(parse_range(Some("bytes=10-"), 100), Range::Part(10, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Range::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-1000"), 100), Range::Part(0, 99));
        assert_eq!(parse_range(Some("bytes=50-5000"), 100), Range::Part(50, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=200-300"), 100), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 100), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-5"), 0), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-1"), 100), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Range::Full);
        assert_eq!(parse_range(Some("items=0-1"), 100), Range::Full);
        assert_eq!(parse_range(Some("bytes=99999999999999999999-"), 100), Range::Full);
        assert_eq!(parse_range(Some("bytes=a-b"), 100), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-18446744073709551615"), 100), Range::Part(0, 99));
    }

    #[test]
    fn form_bodies_decode() {
        assert_eq!(
            form_pairs(b"name=Ana+B&text=a%26b%3D%E2%9C%93&tc="),
            vec![("name".into(), "Ana B".into()), ("text".into(), "a&b=✓".into()), ("tc".into(), String::new())]
        );
        assert_eq!(form_pairs(b"x=%zz%4"), vec![("x".into(), "%zz%4".into())]);
        assert_eq!(form_pairs(b"x=%"), vec![("x".into(), "%".into())]);
        assert_eq!(form_pairs(&b"a=b&".repeat(100)).len(), 16);
    }

    #[test]
    fn header_values_cannot_inject_lines() {
        let mut out = Vec::new();
        write_head(&mut out, 200, &[("Location", "/x\r\nSet-Cookie: a".into())], true).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Location: /xSet-Cookie: a\r\n"));
        assert!(!s.contains("\r\nSet-Cookie"));
    }
    #[test]
    fn the_page_csp_allows_only_the_bootstrap_line() {
        for (input, out) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), out);
        }
        // sha256("") in base64
        assert!(csp_with_script("").contains("script-src 'self' 'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=' 'wasm-unsafe-eval'"));
        let csp = csp_with_script("import init from \"/assets/review_web.js?v=1\"; init();");
        assert!(!csp.contains("unsafe-inline"));
        assert_eq!(csp.matches("'sha256-").count(), 1);
    }
}
