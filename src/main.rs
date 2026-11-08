use std::net::TcpListener;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use cacheproxy::proxy::{Config, Proxy};
use cacheproxy::server::serve;
use cacheproxy::upstream::{parse_origin, HttpUpstream};

const HELP: &str = "\
cacheproxy - a small HTTP caching reverse proxy

USAGE:
    cacheproxy --origin <URL> [OPTIONS]

OPTIONS:
    --origin <URL>          Origin server, for example http://localhost:3000 (required)
    --port <N>              Port to listen on                      [default: 8080]
    --bind <ADDR>           Address to listen on                   [default: 127.0.0.1]
    --max-cache-mb <N>      Cache size in megabytes                [default: 64]
    --max-object-mb <N>     Largest origin response we accept (MB) [default: 10]
    --origin-timeout <S>    Seconds to wait for the origin         [default: 30]
    -h, --help              Show this help

Every response has an X-Cache header: HIT, MISS, REVALIDATED, COALESCED or BYPASS.
Statistics are available as JSON at /__stats.
";

struct Args {
    origin: String,
    port: u16,
    bind: String,
    max_cache_mb: usize,
    max_object_mb: usize,
    origin_timeout: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut origin = None;
    let mut port = 8080u16;
    let mut bind = "127.0.0.1".to_string();
    let mut max_cache_mb = 64usize;
    let mut max_object_mb = 10usize;
    let mut origin_timeout = 30u64;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "-h" || flag == "--help" {
            print!("{HELP}");
            exit(0);
        }
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--origin" => origin = Some(value("--origin")?),
            "--bind" => bind = value("--bind")?,
            "--port" => {
                port = value("--port")?
                    .parse()
                    .map_err(|_| "--port must be a number from 0 to 65535".to_string())?
            }
            "--max-cache-mb" => {
                max_cache_mb = value("--max-cache-mb")?
                    .parse()
                    .map_err(|_| "--max-cache-mb must be a whole number".to_string())?
            }
            "--max-object-mb" => {
                max_object_mb = value("--max-object-mb")?
                    .parse()
                    .map_err(|_| "--max-object-mb must be a whole number".to_string())?
            }
            "--origin-timeout" => {
                origin_timeout = value("--origin-timeout")?
                    .parse()
                    .map_err(|_| "--origin-timeout must be a whole number of seconds".to_string())?
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    let origin = origin.ok_or("--origin is required")?;
    if max_cache_mb == 0 || max_object_mb == 0 || origin_timeout == 0 {
        return Err("sizes and timeouts must be greater than zero".to_string());
    }
    Ok(Args {
        origin,
        port,
        bind,
        max_cache_mb,
        max_object_mb,
        origin_timeout,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\nRun with --help for usage.");
            exit(2);
        }
    };
    let origin = match parse_origin(&args.origin) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            exit(2);
        }
    };

    let host_header = origin.host_header();
    let upstream = HttpUpstream::new(
        origin.clone(),
        Duration::from_secs(args.origin_timeout),
        args.max_object_mb * 1024 * 1024,
    );
    let proxy = Arc::new(Proxy::new(
        Box::new(upstream),
        Config {
            max_cache_bytes: args.max_cache_mb * 1024 * 1024,
            origin_host_header: host_header,
        },
    ));

    let listener = match TcpListener::bind((args.bind.as_str(), args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}:{}: {e}", args.bind, args.port);
            exit(1);
        }
    };
    println!(
        "cacheproxy listening on http://{}:{}  ->  origin http://{}:{}  (cache {} MB)",
        args.bind, args.port, origin.host, origin.port, args.max_cache_mb
    );
    println!("stats: http://{}:{}/__stats", args.bind, args.port);

    if let Err(e) = serve(proxy, listener) {
        eprintln!("server stopped: {e}");
        exit(1);
    }
}
