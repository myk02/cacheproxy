//! End-to-end tests over real TCP sockets: a fake origin server, the proxy
//! and a raw client, all in this process.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cacheproxy::http::{read_request, read_response};
use cacheproxy::proxy::{Config, Proxy};
use cacheproxy::server::serve;
use cacheproxy::upstream::{HttpUpstream, Origin};

// ---------------------------------------------------------------- fake origin

fn origin_connection(mut stream: TcpStream, hits: Arc<AtomicUsize>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let req = match read_request(&mut reader, 1 << 20) {
        Ok(Some(r)) => r,
        _ => return,
    };
    hits.fetch_add(1, SeqCst);
    let conditional = |etag: &str| req.header("if-none-match") == Some(etag);
    let reply: Vec<u8> = match req.target.as_str() {
        "/cached" if conditional("\"v1\"") => b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n".to_vec(),
        "/cached" => b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nCache-Control: max-age=60\r\nContent-Length: 17\r\nConnection: close\r\n\r\nhello from origin".to_vec(),
        "/short" if conditional("\"s1\"") => b"HTTP/1.1 304 Not Modified\r\nETag: \"s1\"\r\nCache-Control: max-age=1\r\nConnection: close\r\n\r\n".to_vec(),
        "/short" => b"HTTP/1.1 200 OK\r\nETag: \"s1\"\r\nCache-Control: max-age=1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nshort".to_vec(),
        "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n6\r\nchunk1\r\n6\r\nchunk2\r\n0\r\n\r\n".to_vec(),
        "/slow" => {
            thread::sleep(Duration::from_millis(200));
            b"HTTP/1.1 200 OK\r\nCache-Control: max-age=60\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow".to_vec()
        }
        "/echo" => {
            let mut out = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                req.body.len()
            )
            .into_bytes();
            out.extend_from_slice(&req.body);
            out
        }
        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    };
    let _ = stream.write_all(&reply);
}

fn start_origin() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_thread = hits.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let hits = hits_for_thread.clone();
            thread::spawn(move || origin_connection(conn, hits));
        }
    });
    (port, hits)
}

fn start_proxy(origin_port: u16) -> SocketAddr {
    let origin = Origin {
        host: "127.0.0.1".to_string(),
        port: origin_port,
    };
    let host_header = origin.host_header();
    let upstream = HttpUpstream::new(origin, Duration::from_secs(5), 10 << 20);
    let proxy = Arc::new(Proxy::new(
        Box::new(upstream),
        Config {
            max_cache_bytes: 1 << 20,
            origin_host_header: host_header,
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(proxy, listener);
    });
    addr
}

fn setup() -> (SocketAddr, Arc<AtomicUsize>) {
    let (origin_port, hits) = start_origin();
    (start_proxy(origin_port), hits)
}

// ---------------------------------------------------------------- raw client

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn send(addr: SocketAddr, raw: &str, head_only: bool) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let resp = read_response(&mut reader, head_only, 1 << 20).unwrap();
    Reply {
        status: resp.status,
        headers: resp.headers,
        body: resp.body.to_vec(),
    }
}

fn get(addr: SocketAddr, path: &str) -> Reply {
    send(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"),
        false,
    )
}

// ---------------------------------------------------------------------- tests

#[test]
fn second_request_is_a_hit_and_the_origin_is_asked_once() {
    let (proxy, hits) = setup();
    let first = get(proxy, "/cached");
    assert_eq!(first.status, 200);
    assert_eq!(first.header("x-cache"), Some("MISS"));
    assert_eq!(first.text(), "hello from origin");

    let second = get(proxy, "/cached");
    assert_eq!(second.status, 200);
    assert_eq!(second.header("x-cache"), Some("HIT"));
    assert_eq!(second.text(), "hello from origin");
    assert_eq!(second.header("content-length"), Some("17"));
    assert!(second.header("age").is_some());
    assert_eq!(second.header("etag"), Some("\"v1\""));
    assert_eq!(hits.load(SeqCst), 1);
}

#[test]
fn expired_entry_is_revalidated_with_a_real_304() {
    let (proxy, hits) = setup();
    assert_eq!(get(proxy, "/short").header("x-cache"), Some("MISS"));
    assert_eq!(get(proxy, "/short").header("x-cache"), Some("HIT"));
    thread::sleep(Duration::from_millis(1300)); // max-age=1 runs out
    let r = get(proxy, "/short");
    assert_eq!(r.header("x-cache"), Some("REVALIDATED"));
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "short");
    assert_eq!(hits.load(SeqCst), 2);
    assert_eq!(get(proxy, "/short").header("x-cache"), Some("HIT"));
    assert_eq!(hits.load(SeqCst), 2);
}

#[test]
fn chunked_origin_responses_are_decoded_and_cached() {
    let (proxy, hits) = setup();
    let first = get(proxy, "/chunked");
    assert_eq!(first.text(), "chunk1chunk2");
    let second = get(proxy, "/chunked");
    assert_eq!(second.header("x-cache"), Some("HIT"));
    assert_eq!(second.text(), "chunk1chunk2");
    assert_eq!(second.header("content-length"), Some("12"));
    assert!(second.header("transfer-encoding").is_none());
    assert_eq!(hits.load(SeqCst), 1);
}

#[test]
fn post_is_forwarded_untouched_and_never_cached() {
    let (proxy, hits) = setup();
    let raw =
        "POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload";
    let r = send(proxy, raw, false);
    assert_eq!(r.header("x-cache"), Some("BYPASS"));
    assert_eq!(r.text(), "payload");
    let r = send(proxy, raw, false);
    assert_eq!(r.header("x-cache"), Some("BYPASS"));
    assert_eq!(hits.load(SeqCst), 2);
}

#[test]
fn head_has_the_length_but_no_body() {
    let (proxy, hits) = setup();
    get(proxy, "/cached");
    let r = send(
        proxy,
        "HEAD /cached HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        true,
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-cache"), Some("HIT"));
    assert_eq!(r.header("content-length"), Some("17"));
    assert!(r.body.is_empty());
    assert_eq!(hits.load(SeqCst), 1);
}

#[test]
fn keep_alive_serves_several_requests_on_one_connection() {
    let (proxy, hits) = setup();
    let stream = TcpStream::connect(proxy).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut labels = Vec::new();
    for _ in 0..3 {
        writer
            .write_all(b"GET /cached HTTP/1.1\r\nHost: t\r\n\r\n")
            .unwrap();
        let r = read_response(&mut reader, false, 1 << 20).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(&r.body[..], b"hello from origin");
        let label = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-cache"))
            .map(|(_, v)| v.clone())
            .unwrap();
        labels.push(label);
    }
    assert_eq!(labels, vec!["MISS", "HIT", "HIT"]);
    assert_eq!(hits.load(SeqCst), 1);
}

#[test]
fn conditional_request_gets_304_from_the_proxy() {
    let (proxy, hits) = setup();
    get(proxy, "/cached");
    let r = send(
        proxy,
        "GET /cached HTTP/1.1\r\nHost: t\r\nIf-None-Match: \"v1\"\r\nConnection: close\r\n\r\n",
        false,
    );
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
    assert_eq!(hits.load(SeqCst), 1);
}

#[test]
fn simultaneous_clients_cause_one_origin_request() {
    let (proxy, hits) = setup();
    let handles: Vec<_> = (0..8)
        .map(|_| thread::spawn(move || get(proxy, "/slow")))
        .collect();
    let replies: Vec<Reply> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(replies.iter().all(|r| r.text() == "slow"));
    assert_eq!(hits.load(SeqCst), 1, "origin must be asked once");
    let coalesced = replies
        .iter()
        .filter(|r| r.header("x-cache") == Some("COALESCED"))
        .count();
    assert!(
        coalesced >= 1,
        "at least one request should have shared a fetch"
    );
}

#[test]
fn stats_endpoint_returns_json() {
    let (proxy, _hits) = setup();
    get(proxy, "/cached");
    get(proxy, "/cached");
    get(proxy, "/cached");
    let r = get(proxy, "/__stats");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/json"));
    let s = r.text();
    assert!(s.contains("\"hits\":2"), "{s}");
    assert!(s.contains("\"misses\":1"), "{s}");
    assert!(s.contains("\"hit_ratio\":0.6667"), "{s}");
}

#[test]
fn unreachable_origin_gives_502() {
    // grab a free port, then close it so nothing is listening there
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let proxy = start_proxy(dead_port);
    let r = get(proxy, "/anything");
    assert_eq!(r.status, 502);
    let stats = get(proxy, "/__stats").text();
    assert!(stats.contains("\"origin_errors\":1"), "{stats}");
}

#[test]
fn garbage_request_gets_400() {
    let (proxy, hits) = setup();
    let r = send(proxy, "this is not http\r\n\r\n", false);
    assert_eq!(r.status, 400);
    assert_eq!(hits.load(SeqCst), 0);
}

#[test]
fn origin_404_without_freshness_info_is_not_cached() {
    let (proxy, hits) = setup();
    assert_eq!(get(proxy, "/nope").status, 404);
    assert_eq!(get(proxy, "/nope").status, 404);
    assert_eq!(hits.load(SeqCst), 2);
}
