//! Benchmark: the same traffic sent straight to a slow origin, then through
//! the caching proxy.
//!
//! ```text
//! cargo run --release --example bench
//! cargo run --release --example bench -- --requests 4000 --threads 32 --latency-ms 30
//! ```
//!
//! The origin is simulated in this process and sleeps `--latency-ms` before
//! each answer (like a real API with a database behind it). Clients open a
//! new connection per request, so the numbers compare the two paths fairly
//! but are not a claim about absolute speed.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cacheproxy::http::{read_request, read_response};
use cacheproxy::proxy::{Config, Proxy};
use cacheproxy::server::serve;
use cacheproxy::upstream::{HttpUpstream, Origin};

struct Opts {
    requests: usize,
    threads: usize,
    latency_ms: u64,
    urls: usize,
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        requests: 2000,
        threads: 16,
        latency_ms: 20,
        urls: 10,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut num = |name: &str| -> u64 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{name} needs a number"))
        };
        match flag.as_str() {
            "--requests" => o.requests = num("--requests") as usize,
            "--threads" => o.threads = num("--threads") as usize,
            "--latency-ms" => o.latency_ms = num("--latency-ms"),
            "--urls" => o.urls = num("--urls") as usize,
            other => panic!("unknown option {other}"),
        }
    }
    o
}

fn start_origin(latency: Duration, hits: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let hits = hits.clone();
            thread::spawn(move || {
                let mut stream = conn;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if let Ok(Some(_req)) = read_request(&mut reader, 1 << 20) {
                    hits.fetch_add(1, SeqCst);
                    thread::sleep(latency);
                    let body = "x".repeat(2048);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nCache-Control: max-age=60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(reply.as_bytes());
                }
            });
        }
    });
    port
}

fn start_proxy(origin_port: u16) -> SocketAddr {
    let origin = Origin {
        host: "127.0.0.1".to_string(),
        port: origin_port,
    };
    let host_header = origin.host_header();
    let proxy = Arc::new(Proxy::new(
        Box::new(HttpUpstream::new(origin, Duration::from_secs(10), 10 << 20)),
        Config {
            max_cache_bytes: 64 << 20,
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

struct Run {
    secs: f64,
    avg_ms: f64,
    p95_ms: f64,
}

fn run(addr: SocketAddr, o: &Opts) -> Run {
    let per_thread = o.requests / o.threads;
    let urls = o.urls;
    let start = Instant::now();
    let handles: Vec<_> = (0..o.threads)
        .map(|t| {
            thread::spawn(move || {
                let mut times = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let path = format!("/item/{}", (t + i) % urls);
                    let began = Instant::now();
                    let mut s = TcpStream::connect(addr).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                    write!(
                        s,
                        "GET {path} HTTP/1.1\r\nHost: bench\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    let mut r = BufReader::new(s);
                    let resp = read_response(&mut r, false, 1 << 20).unwrap();
                    assert_eq!(resp.status, 200);
                    times.push(began.elapsed().as_secs_f64() * 1000.0);
                }
                times
            })
        })
        .collect();
    let mut all: Vec<f64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let secs = start.elapsed().as_secs_f64();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg_ms = all.iter().sum::<f64>() / all.len() as f64;
    let p95_ms = all[(all.len() as f64 * 0.95) as usize - 1];
    Run {
        secs,
        avg_ms,
        p95_ms,
    }
}

fn main() {
    let o = parse_opts();
    let total = (o.requests / o.threads) * o.threads;
    let hits = Arc::new(AtomicUsize::new(0));
    let origin_port = start_origin(Duration::from_millis(o.latency_ms), hits.clone());
    let origin_addr: SocketAddr = ([127, 0, 0, 1], origin_port).into();
    let proxy_addr = start_proxy(origin_port);

    println!(
        "{} requests, {} client threads, {} distinct URLs, origin latency {} ms\n",
        total, o.threads, o.urls, o.latency_ms
    );

    let direct = run(origin_addr, &o);
    let direct_hits = hits.swap(0, SeqCst);
    let proxied = run(proxy_addr, &o);
    let proxied_hits = hits.load(SeqCst);

    println!(
        "| path           | total time | req/s  | avg latency | p95 latency | origin requests |"
    );
    println!(
        "|----------------|-----------:|-------:|------------:|------------:|----------------:|"
    );
    for (name, r, origin_hits) in [
        ("direct to API", &direct, direct_hits),
        ("via cacheproxy", &proxied, proxied_hits),
    ] {
        println!(
            "| {:<14} | {:>8.2} s | {:>6.0} | {:>8.2} ms | {:>8.2} ms | {:>15} |",
            name,
            r.secs,
            total as f64 / r.secs,
            r.avg_ms,
            r.p95_ms,
            origin_hits
        );
    }
    println!(
        "\nspeed-up: {:.1}x throughput, origin load cut by {:.1}%",
        direct.secs / proxied.secs,
        100.0 * (1.0 - proxied_hits as f64 / direct_hits as f64)
    );
}
