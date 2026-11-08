//! A minimal HTTP/1.1 reader and writer.
//!
//! Just enough to parse requests from clients and responses from the origin,
//! with limits on line length, header count and body size. Conflicting
//! message framing (both `Content-Length` and `Transfer-Encoding`, or two
//! different `Content-Length` values) is rejected, which blocks request
//! smuggling tricks.

use std::io::{self, BufRead, Read, Write};
use std::sync::Arc;

pub type Headers = Vec<(String, String)>;
/// Bodies are reference counted so a cache HIT does not copy the payload.
pub type Body = Arc<Vec<u8>>;

const MAX_LINE: usize = 16 * 1024;
const MAX_HEADERS: usize = 128;

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Hop-by-hop headers belong to one connection and must not be forwarded.
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

pub fn header_get<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub fn header_get_all<'a>(headers: &'a Headers, name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

pub fn header_remove(headers: &mut Headers, name: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
}

/// Replace every header called `name` with a single one.
pub fn header_set(headers: &mut Headers, name: &str, value: &str) {
    header_remove(headers, name);
    headers.push((name.to_string(), value.to_string()));
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    /// Handy for tests and for building upstream requests.
    pub fn new(method: &str, target: &str) -> Self {
        Request {
            method: method.to_string(),
            target: target.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header_get(&self.headers, name)
    }

    /// Did the client ask to close the connection after this request?
    pub fn wants_close(&self) -> bool {
        let conn = header_get_all(&self.headers, "connection").join(",");
        let has = |t: &str| conn.split(',').any(|p| p.trim().eq_ignore_ascii_case(t));
        if self.version == "HTTP/1.0" {
            !has("keep-alive")
        } else {
            has("close")
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
    pub body: Body,
}

impl Response {
    pub fn new(status: u16, reason: &str, headers: Headers, body: Vec<u8>) -> Self {
        Response {
            status,
            reason: reason.to_string(),
            headers,
            body: Arc::new(body),
        }
    }

    /// A small plain-text response (used for errors).
    pub fn text(status: u16, reason: &str, message: &str) -> Self {
        Response::new(
            status,
            reason,
            vec![(
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            message.as_bytes().to_vec(),
        )
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header_get(&self.headers, name)
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn eof(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg.to_string())
}

/// Read one line. `Ok(None)` means a clean end of stream before any byte.
fn read_line<R: BufRead>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        return Err(invalid("line too long"));
    }
    if buf.last() != Some(&b'\n') {
        return Err(eof("truncated line"));
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn read_headers<R: BufRead>(r: &mut R) -> io::Result<Headers> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(r)?.ok_or_else(|| eof("end of stream inside headers"))?;
        if line.is_empty() {
            return Ok(headers);
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(invalid("obsolete line folding is not supported"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed header line"))?;
        if name.is_empty() || name.contains(|c: char| c.is_whitespace()) {
            return Err(invalid("malformed header name"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
        if headers.len() > MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
    }
}

fn is_chunked(headers: &Headers) -> bool {
    header_get_all(headers, "transfer-encoding")
        .iter()
        .any(|v| v.to_ascii_lowercase().contains("chunked"))
}

/// All `Content-Length` values must agree. `Ok(None)` if there is none.
fn content_length(headers: &Headers) -> io::Result<Option<usize>> {
    let mut found: Option<usize> = None;
    for value in header_get_all(headers, "content-length") {
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("invalid Content-Length"));
            }
            let n: usize = part
                .parse()
                .map_err(|_| invalid("invalid Content-Length"))?;
            match found {
                Some(prev) if prev != n => return Err(invalid("conflicting Content-Length")),
                _ => found = Some(n),
            }
        }
    }
    Ok(found)
}

fn read_fixed<R: BufRead>(r: &mut R, len: usize, max: usize) -> io::Result<Vec<u8>> {
    if len > max {
        return Err(invalid("body too large"));
    }
    let mut buf = Vec::with_capacity(len);
    r.by_ref().take(len as u64).read_to_end(&mut buf)?;
    if buf.len() != len {
        return Err(eof("body shorter than Content-Length"));
    }
    Ok(buf)
}

fn read_chunked<R: BufRead>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(r)?.ok_or_else(|| eof("end of stream inside chunked body"))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("bad chunk size"))?;
        if size == 0 {
            // skip optional trailers up to the blank line
            loop {
                let t = read_line(r)?.ok_or_else(|| eof("end of stream inside trailers"))?;
                if t.is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len().checked_add(size).is_none_or(|total| total > max) {
            return Err(invalid("body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
        if read_line(r)?.as_deref() != Some("") {
            return Err(invalid("missing chunk terminator"));
        }
    }
}

/// Read one request. `Ok(None)` means the client closed the connection.
pub fn read_request<R: BufRead>(r: &mut R, max_body: usize) -> io::Result<Option<Request>> {
    let mut line = match read_line(r)? {
        Some(l) => l,
        None => return Ok(None),
    };
    if line.is_empty() {
        // RFC 9112 allows ignoring one empty line before the request line
        line = match read_line(r)? {
            Some(l) => l,
            None => return Ok(None),
        };
    }
    let mut parts = line.split_whitespace();
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) => (m, t, v),
        _ => return Err(invalid("malformed request line")),
    };
    if !version.starts_with("HTTP/1.") {
        return Err(invalid("unsupported HTTP version"));
    }
    let headers = read_headers(r)?;
    let chunked = is_chunked(&headers);
    let length = content_length(&headers)?;
    if chunked && length.is_some() {
        return Err(invalid("both Transfer-Encoding and Content-Length"));
    }
    let body = if chunked {
        read_chunked(r, max_body)?
    } else if let Some(n) = length {
        read_fixed(r, n, max_body)?
    } else {
        Vec::new()
    };
    Ok(Some(Request {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        body,
    }))
}

/// Read one response. `head_only` says the request was HEAD (no body follows).
/// Interim `1xx` responses are skipped.
pub fn read_response<R: BufRead>(
    r: &mut R,
    head_only: bool,
    max_body: usize,
) -> io::Result<Response> {
    loop {
        let line = read_line(r)?.ok_or_else(|| eof("origin closed without a response"))?;
        let mut parts = line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        if !version.starts_with("HTTP/1.") {
            return Err(invalid("origin sent a bad status line"));
        }
        let status: u16 = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| invalid("origin sent a bad status code"))?;
        let reason = parts.next().unwrap_or("").trim().to_string();
        let headers = read_headers(r)?;
        let interim = (100..200).contains(&status);
        if interim && status != 101 {
            continue;
        }
        let no_body = head_only || interim || status == 204 || status == 304;
        let chunked = is_chunked(&headers);
        let length = content_length(&headers)?;
        if chunked && length.is_some() {
            return Err(invalid(
                "origin sent both Transfer-Encoding and Content-Length",
            ));
        }
        let body = if no_body {
            Vec::new()
        } else if chunked {
            read_chunked(r, max_body)?
        } else if let Some(n) = length {
            read_fixed(r, n, max_body)?
        } else {
            // no framing: the body runs until the origin closes the connection
            let mut buf = Vec::new();
            r.by_ref().take(max_body as u64 + 1).read_to_end(&mut buf)?;
            if buf.len() > max_body {
                return Err(invalid("body too large"));
            }
            buf
        };
        return Ok(Response {
            status,
            reason,
            headers,
            body: Arc::new(body),
        });
    }
}

fn clean(s: &str) -> bool {
    !s.contains(['\r', '\n'])
}

/// Write a response. `Content-Length` is always recomputed from the body, and
/// hop-by-hop headers are dropped. With `head_only` the headers are sent but
/// the body is left out.
pub fn write_response<W: Write>(
    w: &mut W,
    resp: &Response,
    head_only: bool,
    keep_alive: bool,
) -> io::Result<()> {
    let bodyless = resp.status == 204 || resp.status == 304 || (100..200).contains(&resp.status);
    let mut out = Vec::with_capacity(256 + if head_only { 0 } else { resp.body.len() });
    let reason = if clean(&resp.reason) {
        &resp.reason[..]
    } else {
        ""
    };
    write!(out, "HTTP/1.1 {} {}\r\n", resp.status, reason)?;
    for (k, v) in &resp.headers {
        if is_hop_by_hop(k) || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if !clean(k) || !clean(v) {
            continue; // never let a header value inject a new line
        }
        write!(out, "{k}: {v}\r\n")?;
    }
    if !bodyless {
        write!(out, "Content-Length: {}\r\n", resp.body.len())?;
    }
    let conn = if keep_alive { "keep-alive" } else { "close" };
    write!(out, "Connection: {conn}\r\n\r\n")?;
    if !head_only && !bodyless {
        out.extend_from_slice(&resp.body);
    }
    w.write_all(&out)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn req(text: &str) -> io::Result<Option<Request>> {
        read_request(&mut Cursor::new(text.as_bytes().to_vec()), 1024)
    }

    fn resp(text: &str, head_only: bool) -> io::Result<Response> {
        read_response(&mut Cursor::new(text.as_bytes().to_vec()), head_only, 1024)
    }

    #[test]
    fn parses_a_get_request() {
        let r = req("GET /a?b=1 HTTP/1.1\r\nHost: x\r\nAccept: */*\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.target, "/a?b=1");
        assert_eq!(r.header("host"), Some("x"));
        assert_eq!(r.header("ACCEPT"), Some("*/*"));
        assert!(r.body.is_empty());
    }

    #[test]
    fn parses_a_post_body() {
        let r = req("POST /p HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap()
            .unwrap();
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn clean_close_is_none() {
        assert!(req("").unwrap().is_none());
    }

    #[test]
    fn malformed_request_lines_are_errors() {
        assert!(req("GET /\r\n\r\n").is_err());
        assert!(req("GET / HTTP/2.0\r\n\r\n").is_err());
        assert!(req("GET / HTTP/1.1 extra\r\n\r\n").is_err());
        assert!(req("GET / HTTP/1.1\r\nNoColonHere\r\n\r\n").is_err());
        assert!(req("GET / HTTP/1.1\r\n folded: x\r\n\r\n").is_err());
    }

    #[test]
    fn rejects_conflicting_framing() {
        assert!(
            req("POST / HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd").is_err()
        );
        assert!(req(
            "POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        )
        .is_err());
    }

    #[test]
    fn same_content_length_twice_is_fine() {
        let r = req("POST / HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\nhi")
            .unwrap()
            .unwrap();
        assert_eq!(r.body, b"hi");
    }

    #[test]
    fn body_limit_is_enforced() {
        assert!(req("POST / HTTP/1.1\r\nContent-Length: 99999\r\n\r\n").is_err());
    }

    #[test]
    fn truncated_body_is_an_error() {
        assert!(req("POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc").is_err());
    }

    #[test]
    fn response_with_content_length() {
        let r = resp(
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nX: y\r\n\r\nabcEXTRA",
            false,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.reason, "OK");
        assert_eq!(&r.body[..], b"abc");
    }

    #[test]
    fn chunked_response_is_decoded() {
        let r = resp(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\nTrailer: x\r\n\r\n",
            false,
        )
        .unwrap();
        assert_eq!(&r.body[..], b"abcdefg");
    }

    #[test]
    fn response_without_framing_reads_to_eof() {
        let r = resp("HTTP/1.0 200 OK\r\n\r\nall of it", false).unwrap();
        assert_eq!(&r.body[..], b"all of it");
    }

    #[test]
    fn head_304_and_204_have_no_body() {
        assert!(resp("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", true)
            .unwrap()
            .body
            .is_empty());
        assert!(
            resp("HTTP/1.1 304 Not Modified\r\nETag: \"x\"\r\n\r\n", false)
                .unwrap()
                .body
                .is_empty()
        );
        assert!(resp("HTTP/1.1 204 No Content\r\n\r\n", false)
            .unwrap()
            .body
            .is_empty());
    }

    #[test]
    fn interim_100_is_skipped() {
        let r = resp(
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            false,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(&r.body[..], b"ok");
    }

    #[test]
    fn bad_origin_status_line() {
        assert!(resp("garbage\r\n\r\n", false).is_err());
        assert!(resp("HTTP/1.1 abc OK\r\n\r\n", false).is_err());
    }

    #[test]
    fn write_response_shape() {
        let r = Response::new(
            200,
            "OK",
            vec![
                ("Content-Type".into(), "text/plain".into()),
                ("Content-Length".into(), "999".into()),
                ("Transfer-Encoding".into(), "chunked".into()),
                ("Connection".into(), "upgrade".into()),
            ],
            b"hello".to_vec(),
        );
        let mut out = Vec::new();
        write_response(&mut out, &r, false, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(!text.contains("999"));
        assert!(!text.to_lowercase().contains("transfer-encoding"));
        assert!(!text.contains("upgrade"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.ends_with("\r\n\r\nhello"));
    }

    #[test]
    fn head_only_keeps_length_but_drops_body() {
        let r = Response::new(200, "OK", vec![], b"hello".to_vec());
        let mut out = Vec::new();
        write_response(&mut out, &r, true, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn not_modified_has_no_content_length() {
        let r = Response::new(
            304,
            "Not Modified",
            vec![("ETag".into(), "\"x\"".into())],
            vec![],
        );
        let mut out = Vec::new();
        write_response(&mut out, &r, false, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Content-Length"));
    }

    #[test]
    fn header_injection_is_blocked() {
        let r = Response::new(
            200,
            "OK",
            vec![("X-Evil".into(), "a\r\nSet-Cookie: pwned=1".into())],
            vec![],
        );
        let mut out = Vec::new();
        write_response(&mut out, &r, false, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("pwned"));
    }

    #[test]
    fn wants_close_rules() {
        let mut r = Request::new("GET", "/");
        assert!(!r.wants_close());
        r.headers.push(("Connection".into(), "close".into()));
        assert!(r.wants_close());
        let mut old = Request::new("GET", "/");
        old.version = "HTTP/1.0".into();
        assert!(old.wants_close());
        old.headers.push(("Connection".into(), "Keep-Alive".into()));
        assert!(!old.wants_close());
    }

    #[test]
    fn header_helpers() {
        let mut h: Headers = vec![("A".into(), "1".into()), ("a".into(), "2".into())];
        assert_eq!(header_get(&h, "A"), Some("1"));
        assert_eq!(header_get_all(&h, "a"), vec!["1", "2"]);
        header_set(&mut h, "a", "3");
        assert_eq!(h.len(), 1);
        assert_eq!(header_get(&h, "A"), Some("3"));
        header_remove(&mut h, "A");
        assert!(h.is_empty());
    }
}
