//! The brain of the proxy.
//!
//! For every request the proxy answers one question: can I answer this
//! without bothering the origin?
//!
//! ```text
//!  request ──► not GET/HEAD, or Authorization / Range / no-store?
//!                 │ yes ──► BYPASS: forward untouched (unsafe methods also
//!                 │                 evict the cached copy of that URL)
//!                 ▼ no
//!             entry in cache and fresh (and client did not say no-cache)?
//!                 │ yes ──► HIT
//!                 ▼ no
//!             someone already fetching this URL right now?
//!                 │ yes ──► wait and share their answer: COALESCED
//!                 ▼ no
//!             ask the origin (conditionally, if we hold a stale copy)
//!                 ├─ 304 Not Modified ──► REVALIDATED (copy refreshed)
//!                 └─ anything else    ──► MISS (stored if the rules allow)
//! ```
//!
//! Every response carries an `X-Cache` header with the label above.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use crate::cache_control::CacheControl;
use crate::entry::Entry;
use crate::http::{
    header_get, header_get_all, header_remove, header_set, is_hop_by_hop, Headers, Request,
    Response,
};
use crate::lru::LruCache;
use crate::policy::{self, StorePolicy};
use crate::stats::Stats;
use crate::upstream::Upstream;

pub struct Config {
    /// Cache budget in bytes.
    pub max_cache_bytes: usize,
    /// Value of the `Host` header sent to the origin.
    pub origin_host_header: String,
}

type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// The result of one origin fetch, shared with every request waiting on it.
struct Outcome {
    response: Response,
    label: &'static str,
}

struct Flight {
    result: Mutex<Option<Arc<Outcome>>>,
    cv: Condvar,
}

pub struct Proxy {
    upstream: Box<dyn Upstream>,
    clock: Clock,
    origin_host_header: String,
    cache: Mutex<LruCache<String, Arc<Entry>>>,
    inflight: Mutex<HashMap<String, Arc<Flight>>>,
    pub stats: Stats,
}

/// Lock a mutex, carrying on even if another thread panicked while holding it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Proxy {
    pub fn new(upstream: Box<dyn Upstream>, config: Config) -> Self {
        Self::with_clock(upstream, config, Box::new(Instant::now))
    }

    /// Like [`Proxy::new`] but with your own clock, so tests can jump forward
    /// in time instead of sleeping.
    pub fn with_clock(upstream: Box<dyn Upstream>, config: Config, clock: Clock) -> Self {
        Proxy {
            upstream,
            clock,
            origin_host_header: config.origin_host_header,
            cache: Mutex::new(LruCache::new(config.max_cache_bytes)),
            inflight: Mutex::new(HashMap::new()),
            stats: Stats::default(),
        }
    }

    /// Handle one request. The returned response is always the full `GET`
    /// view; for `HEAD` the server leaves the body out when writing it.
    pub fn handle(&self, req: &Request) -> Response {
        self.stats.requests.fetch_add(1, Relaxed);

        if !req.target.starts_with('/') {
            return Response::text(
                400,
                "Bad Request",
                "Only origin-form targets are supported.\n",
            );
        }
        let path = req.target.split('?').next().unwrap_or("");
        if path == "/__stats" && req.method == "GET" {
            return self.stats_response();
        }

        let req_cc = request_cache_control(&req.headers);
        let cacheable_method = req.method == "GET" || req.method == "HEAD";
        let bypass = !cacheable_method
            || header_get(&req.headers, "authorization").is_some()
            || header_get(&req.headers, "range").is_some()
            || req_cc.no_store;
        if bypass {
            return self.pass_through(req);
        }

        let key = req.target.clone();
        let now = (self.clock)();
        let existing = lock(&self.cache)
            .get(&key)
            .cloned()
            .filter(|e| e.vary_matches(&req.headers));
        let force = req_cc.no_cache || req_cc.max_age == Some(0);

        if let Some(entry) = &existing {
            if !force && entry.is_fresh(now) {
                self.stats.hits.fetch_add(1, Relaxed);
                return self.finish(req, self.response_from_entry(entry, now), "HIT");
            }
        }

        let (outcome, follower) = self.fetch_coalesced(req, key, existing);
        let label = if follower && outcome.label != "ERROR" {
            "COALESCED"
        } else {
            outcome.label
        };
        self.finish(req, outcome.response.clone(), label)
    }

    /// Forward a request untouched. Nothing is stored, but a successful
    /// unsafe method (POST, PUT, DELETE...) evicts the cached copy of the URL.
    fn pass_through(&self, req: &Request) -> Response {
        self.stats.bypass.fetch_add(1, Relaxed);
        self.stats.origin_requests.fetch_add(1, Relaxed);
        let oreq = self.origin_request(req, None, false);
        match self.upstream.fetch(&oreq) {
            Ok(mut resp) => {
                let safe = matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
                if !safe && resp.status < 400 {
                    lock(&self.cache).remove(&req.target);
                }
                header_set(&mut resp.headers, "X-Cache", "BYPASS");
                resp
            }
            Err(_) => {
                self.stats.origin_errors.fetch_add(1, Relaxed);
                bad_gateway()
            }
        }
    }

    /// Fetch from the origin, but if another request is already fetching the
    /// same thing, wait for it and share the result instead of making a
    /// second trip. Returns the outcome and whether we were the follower.
    fn fetch_coalesced(
        &self,
        req: &Request,
        key: String,
        existing: Option<Arc<Entry>>,
    ) -> (Arc<Outcome>, bool) {
        let flight_key = coalesce_key(req);
        let (flight, leader) = {
            let mut map = lock(&self.inflight);
            match map.get(&flight_key) {
                Some(f) => (f.clone(), false),
                None => {
                    let f = Arc::new(Flight {
                        result: Mutex::new(None),
                        cv: Condvar::new(),
                    });
                    map.insert(flight_key.clone(), f.clone());
                    (f, true)
                }
            }
        };

        if leader {
            let mut guard = FlightGuard {
                proxy: self,
                key: &flight_key,
                flight: &flight,
                done: false,
            };
            let outcome = Arc::new(self.fetch_and_store(req, &key, existing));
            guard.complete(outcome.clone());
            (outcome, false)
        } else {
            self.stats.coalesced.fetch_add(1, Relaxed);
            let mut slot = lock(&flight.result);
            while slot.is_none() {
                slot = flight.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
            (slot.clone().expect("result was published"), true)
        }
    }

    /// The only place that talks to the origin for cacheable requests.
    fn fetch_and_store(&self, req: &Request, key: &str, existing: Option<Arc<Entry>>) -> Outcome {
        self.stats.origin_requests.fetch_add(1, Relaxed);
        let oreq = self.origin_request(req, existing.as_deref(), true);
        let resp = match self.upstream.fetch(&oreq) {
            Ok(r) => r,
            Err(_) => {
                self.stats.origin_errors.fetch_add(1, Relaxed);
                return error_outcome();
            }
        };
        let now = (self.clock)();

        if resp.status == 304 {
            // We only send conditional requests when we hold a stale copy.
            let Some(old) = existing else {
                self.stats.origin_errors.fetch_add(1, Relaxed);
                return error_outcome();
            };
            self.stats.revalidated.fetch_add(1, Relaxed);
            let mut merged = merge_headers(&old.headers, &resp.headers);
            let response = match policy::evaluate(old.status, &merged) {
                Some(p) => {
                    header_remove(&mut merged, "age");
                    let mut entry = (*old).clone();
                    entry.headers = merged;
                    entry.stored_at = now;
                    entry.initial_age = p.initial_age;
                    entry.lifetime = p.lifetime;
                    entry.no_cache = p.no_cache;
                    let response = self.response_from_entry(&entry, now);
                    self.store(key, entry);
                    response
                }
                None => {
                    // the 304 changed the rules (for example no-store): the
                    // content is still right for this client, but forget it
                    lock(&self.cache).remove(&key.to_string());
                    self.response_from_entry(&old, now)
                }
            };
            return Outcome {
                response,
                label: "REVALIDATED",
            };
        }

        self.stats.misses.fetch_add(1, Relaxed);
        match policy::evaluate(resp.status, &resp.headers) {
            Some(p) => {
                let entry = build_entry(&resp, p, req, now);
                self.store(key, entry);
            }
            None if existing.is_some() && resp.status < 400 => {
                // the origin now says do-not-store: drop the old copy
                lock(&self.cache).remove(&key.to_string());
            }
            None => {}
        }
        Outcome {
            response: resp,
            label: "MISS",
        }
    }

    fn store(&self, key: &str, entry: Entry) {
        let size = entry.size(key.len());
        lock(&self.cache).insert(key.to_string(), Arc::new(entry), size);
    }

    /// Build the request we send to the origin.
    ///
    /// * hop-by-hop headers and the client's `Host` are dropped
    /// * on the cacheable path the client's conditional headers and cache
    ///   directives are dropped too, since the proxy handles those itself,
    ///   and our own validators (`If-None-Match`...) are added from `existing`
    /// * cacheable requests (and HEAD) always go out as `GET`
    fn origin_request(&self, req: &Request, existing: Option<&Entry>, cacheable: bool) -> Request {
        let mut headers = Headers::new();
        for (k, v) in &req.headers {
            if is_hop_by_hop(k)
                || k.eq_ignore_ascii_case("host")
                || k.eq_ignore_ascii_case("content-length")
            {
                continue;
            }
            if cacheable
                && (is_conditional(k)
                    || k.eq_ignore_ascii_case("cache-control")
                    || k.eq_ignore_ascii_case("pragma"))
            {
                continue;
            }
            headers.push((k.clone(), v.clone()));
        }
        headers.push(("Host".to_string(), self.origin_host_header.clone()));
        if let Some(e) = existing {
            if let Some(etag) = e.etag() {
                headers.push(("If-None-Match".to_string(), etag.to_string()));
            }
            if let Some(lm) = e.last_modified() {
                headers.push(("If-Modified-Since".to_string(), lm.to_string()));
            }
        }
        let as_get = cacheable || req.method == "HEAD";
        Request {
            method: if as_get {
                "GET".to_string()
            } else {
                req.method.clone()
            },
            target: req.target.clone(),
            version: "HTTP/1.1".to_string(),
            headers,
            body: if as_get { Vec::new() } else { req.body.clone() },
        }
    }

    fn response_from_entry(&self, entry: &Entry, now: Instant) -> Response {
        let mut headers = entry.headers.clone();
        header_set(&mut headers, "Age", &entry.current_age(now).to_string());
        Response {
            status: entry.status,
            reason: entry.reason.clone(),
            headers,
            body: entry.body.clone(),
        }
    }

    /// Final touches: answer `304` if the client's `If-None-Match` matches,
    /// and add the `X-Cache` label.
    fn finish(&self, req: &Request, mut resp: Response, label: &str) -> Response {
        if resp.status == 200 {
            if let (Some(inm), Some(etag)) = (
                header_get(&req.headers, "if-none-match"),
                header_get(&resp.headers, "etag"),
            ) {
                if etag_matches(inm, etag) {
                    resp = not_modified(&resp);
                }
            }
        }
        header_set(&mut resp.headers, "X-Cache", label);
        resp
    }

    fn stats_response(&self) -> Response {
        let (entries, bytes, max_bytes, evictions) = {
            let c = lock(&self.cache);
            (c.len(), c.bytes(), c.max_bytes(), c.evictions())
        };
        let s = &self.stats;
        let hits = s.hits.load(Relaxed);
        let misses = s.misses.load(Relaxed);
        let revalidated = s.revalidated.load(Relaxed);
        let coalesced = s.coalesced.load(Relaxed);
        let lookups = hits + misses + revalidated + coalesced;
        let ratio = if lookups == 0 {
            0.0
        } else {
            hits as f64 / lookups as f64
        };
        let body = format!(
            "{{\"requests\":{},\"hits\":{},\"misses\":{},\"revalidated\":{},\"coalesced\":{},\
             \"bypass\":{},\"origin_requests\":{},\"origin_errors\":{},\"hit_ratio\":{:.4},\
             \"entries\":{},\"bytes\":{},\"max_bytes\":{},\"evictions\":{}}}\n",
            s.requests.load(Relaxed),
            hits,
            misses,
            revalidated,
            coalesced,
            s.bypass.load(Relaxed),
            s.origin_requests.load(Relaxed),
            s.origin_errors.load(Relaxed),
            ratio,
            entries,
            bytes,
            max_bytes,
            evictions
        );
        Response::new(
            200,
            "OK",
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Cache-Control".to_string(), "no-store".to_string()),
            ],
            body.into_bytes(),
        )
    }
}

/// Publishes the leader's result to the followers. If the leader panics
/// before finishing, `Drop` still releases everyone with an error, so
/// followers never wait forever.
struct FlightGuard<'a> {
    proxy: &'a Proxy,
    key: &'a str,
    flight: &'a Flight,
    done: bool,
}

impl FlightGuard<'_> {
    fn complete(&mut self, outcome: Arc<Outcome>) {
        lock(&self.proxy.inflight).remove(self.key);
        *lock(&self.flight.result) = Some(outcome);
        self.flight.cv.notify_all();
        self.done = true;
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.complete(Arc::new(error_outcome()));
        }
    }
}

fn bad_gateway() -> Response {
    Response::text(
        502,
        "Bad Gateway",
        "Bad Gateway: the origin did not give a usable answer.\n",
    )
}

fn error_outcome() -> Outcome {
    Outcome {
        response: bad_gateway(),
        label: "ERROR",
    }
}

/// Request-side `Cache-Control`; an old-style `Pragma: no-cache` counts too
/// when there is no `Cache-Control`.
fn request_cache_control(headers: &Headers) -> CacheControl {
    let mut cc = policy::cache_control_of(headers);
    if header_get(headers, "cache-control").is_none()
        && header_get_all(headers, "pragma")
            .iter()
            .any(|v| v.to_ascii_lowercase().contains("no-cache"))
    {
        cc.no_cache = true;
    }
    cc
}

/// Requests may share one origin fetch only if they agree on the headers
/// that most often change the response.
fn coalesce_key(req: &Request) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        req.target,
        header_get(&req.headers, "accept-encoding").unwrap_or(""),
        header_get(&req.headers, "accept-language").unwrap_or(""),
        header_get(&req.headers, "accept").unwrap_or("")
    )
}

fn is_conditional(name: &str) -> bool {
    [
        "if-none-match",
        "if-modified-since",
        "if-match",
        "if-unmodified-since",
        "if-range",
    ]
    .iter()
    .any(|c| name.eq_ignore_ascii_case(c))
}

fn build_entry(resp: &Response, policy: StorePolicy, req: &Request, now: Instant) -> Entry {
    let headers: Headers = resp
        .headers
        .iter()
        .filter(|(k, _)| {
            !is_hop_by_hop(k)
                && !k.eq_ignore_ascii_case("content-length")
                && !k.eq_ignore_ascii_case("age")
        })
        .cloned()
        .collect();
    let vary = policy
        .vary
        .iter()
        .map(|name| {
            (
                name.clone(),
                header_get(&req.headers, name).unwrap_or("").to_string(),
            )
        })
        .collect();
    Entry {
        status: resp.status,
        reason: resp.reason.clone(),
        headers,
        body: resp.body.clone(),
        stored_at: now,
        initial_age: policy.initial_age,
        lifetime: policy.lifetime,
        no_cache: policy.no_cache,
        vary,
    }
}

/// Apply the headers of a `304` to a stored header list.
fn merge_headers(stored: &Headers, update: &Headers) -> Headers {
    let mut merged = stored.clone();
    for (k, v) in update {
        if is_hop_by_hop(k)
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("content-encoding")
        {
            continue;
        }
        header_set(&mut merged, k, v);
    }
    merged
}

/// Weak comparison of an `If-None-Match` list against one ETag.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    if if_none_match.trim() == "*" {
        return true;
    }
    let strip = |s: &str| s.trim().trim_start_matches("W/").to_string();
    let target = strip(etag);
    if_none_match
        .split(',')
        .any(|candidate| strip(candidate) == target)
}

fn not_modified(resp: &Response) -> Response {
    const KEEP: [&str; 8] = [
        "etag",
        "cache-control",
        "expires",
        "date",
        "vary",
        "last-modified",
        "age",
        "content-location",
    ];
    let headers = resp
        .headers
        .iter()
        .filter(|(k, _)| KEEP.iter().any(|n| k.eq_ignore_ascii_case(n)))
        .cloned()
        .collect();
    Response::new(304, "Not Modified", headers, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    type Handler = Box<dyn Fn(&Request, usize) -> io::Result<Response> + Send + Sync>;

    /// A fake origin: counts calls, remembers requests, can be slow.
    struct Mock {
        calls: AtomicUsize,
        seen: Mutex<Vec<Request>>,
        handler: Handler,
        delay: Duration,
    }

    impl Upstream for Arc<Mock> {
        fn fetch(&self, req: &Request) -> io::Result<Response> {
            let n = self.calls.fetch_add(1, SeqCst) + 1;
            lock(&self.seen).push(req.clone());
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            (self.handler)(req, n)
        }
    }

    struct Rig {
        proxy: Proxy,
        mock: Arc<Mock>,
        now: Arc<Mutex<Instant>>,
    }

    impl Rig {
        fn new(max_bytes: usize, delay: Duration, handler: Handler) -> Rig {
            let mock = Arc::new(Mock {
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                handler,
                delay,
            });
            let now = Arc::new(Mutex::new(Instant::now()));
            let clock_now = now.clone();
            let proxy = Proxy::with_clock(
                Box::new(mock.clone()),
                Config {
                    max_cache_bytes: max_bytes,
                    origin_host_header: "origin.test".to_string(),
                },
                Box::new(move || *lock(&clock_now)),
            );
            Rig { proxy, mock, now }
        }

        fn simple(handler: Handler) -> Rig {
            Rig::new(1 << 20, Duration::ZERO, handler)
        }

        fn advance(&self, secs: u64) {
            *lock(&self.now) += Duration::from_secs(secs);
        }

        fn calls(&self) -> usize {
            self.mock.calls.load(SeqCst)
        }

        fn get(&self, path: &str) -> Response {
            self.proxy.handle(&Request::new("GET", path))
        }

        fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> Response {
            let mut r = Request::new("GET", path);
            for (k, v) in headers {
                r.headers.push((k.to_string(), v.to_string()));
            }
            self.proxy.handle(&r)
        }
    }

    fn resp(status: u16, body: &str, headers: &[(&str, &str)]) -> io::Result<Response> {
        Ok(Response::new(
            status,
            "X",
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body.as_bytes().to_vec(),
        ))
    }

    fn cacheable(body: &'static str) -> Handler {
        Box::new(move |_, _| {
            resp(
                200,
                body,
                &[("Cache-Control", "max-age=60"), ("ETag", "\"v1\"")],
            )
        })
    }

    fn label(r: &Response) -> &str {
        r.header("x-cache").unwrap_or("")
    }

    fn body(r: &Response) -> String {
        String::from_utf8_lossy(&r.body).into_owned()
    }

    #[test]
    fn miss_then_hit_with_one_origin_call() {
        let rig = Rig::simple(cacheable("hello"));
        let first = rig.get("/a");
        assert_eq!(label(&first), "MISS");
        assert_eq!(body(&first), "hello");
        let second = rig.get("/a");
        assert_eq!(label(&second), "HIT");
        assert_eq!(body(&second), "hello");
        assert_eq!(second.header("age"), Some("0"));
        assert_eq!(rig.calls(), 1);
    }

    #[test]
    fn age_grows_while_fresh() {
        let rig = Rig::simple(cacheable("hello"));
        rig.get("/a");
        rig.advance(30);
        let r = rig.get("/a");
        assert_eq!(label(&r), "HIT");
        assert_eq!(r.header("age"), Some("30"));
        assert_eq!(rig.calls(), 1);
    }

    #[test]
    fn different_urls_are_cached_separately() {
        let rig = Rig::simple(Box::new(|req, _| {
            resp(200, &req.target, &[("Cache-Control", "max-age=60")])
        }));
        assert_eq!(body(&rig.get("/a")), "/a");
        assert_eq!(body(&rig.get("/b")), "/b");
        assert_eq!(body(&rig.get("/a?x=1")), "/a?x=1");
        assert_eq!(label(&rig.get("/a")), "HIT");
        assert_eq!(rig.calls(), 3);
    }

    #[test]
    fn stale_entry_is_revalidated_with_conditional_headers() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.header("if-none-match") == Some("\"v1\"") {
                resp(
                    304,
                    "",
                    &[("Cache-Control", "max-age=60"), ("ETag", "\"v1\"")],
                )
            } else {
                resp(
                    200,
                    "hello",
                    &[("Cache-Control", "max-age=60"), ("ETag", "\"v1\"")],
                )
            }
        }));
        rig.get("/a");
        rig.advance(61);
        let r = rig.get("/a");
        assert_eq!(label(&r), "REVALIDATED");
        assert_eq!(r.status, 200);
        assert_eq!(body(&r), "hello");
        assert_eq!(rig.calls(), 2);
        let seen = lock(&rig.mock.seen);
        assert_eq!(seen[1].header("if-none-match"), Some("\"v1\""));
        drop(seen);
        // the 304 made the copy fresh again
        let again = rig.get("/a");
        assert_eq!(label(&again), "HIT");
        assert_eq!(again.header("age"), Some("0"));
        assert_eq!(rig.calls(), 2);
    }

    #[test]
    fn last_modified_is_used_for_revalidation() {
        let lm = "Sun, 06 Nov 1994 08:49:37 GMT";
        let rig = Rig::simple(Box::new(move |req, _| {
            if req.header("if-modified-since") == Some(lm) {
                resp(304, "", &[("Cache-Control", "max-age=10")])
            } else {
                resp(
                    200,
                    "x",
                    &[("Cache-Control", "max-age=10"), ("Last-Modified", lm)],
                )
            }
        }));
        rig.get("/a");
        rig.advance(11);
        assert_eq!(label(&rig.get("/a")), "REVALIDATED");
    }

    #[test]
    fn changed_content_replaces_the_stale_entry() {
        let rig = Rig::simple(Box::new(|req, n| {
            if n == 1 {
                resp(
                    200,
                    "old",
                    &[("Cache-Control", "max-age=10"), ("ETag", "\"v1\"")],
                )
            } else {
                assert_eq!(req.header("if-none-match"), Some("\"v1\""));
                resp(
                    200,
                    "new",
                    &[("Cache-Control", "max-age=10"), ("ETag", "\"v2\"")],
                )
            }
        }));
        rig.get("/a");
        rig.advance(11);
        let r = rig.get("/a");
        assert_eq!(label(&r), "MISS");
        assert_eq!(body(&r), "new");
        let r = rig.get("/a");
        assert_eq!(label(&r), "HIT");
        assert_eq!(body(&r), "new");
    }

    #[test]
    fn stale_without_validator_is_refetched() {
        let rig = Rig::simple(Box::new(|_, n| {
            resp(200, &format!("v{n}"), &[("Cache-Control", "max-age=10")])
        }));
        rig.get("/a");
        rig.advance(11);
        let r = rig.get("/a");
        assert_eq!(label(&r), "MISS");
        assert_eq!(body(&r), "v2");
        let seen = lock(&rig.mock.seen);
        assert_eq!(seen[1].header("if-none-match"), None);
    }

    #[test]
    fn no_store_is_never_cached() {
        let rig = Rig::simple(Box::new(|_, _| {
            resp(200, "secret", &[("Cache-Control", "no-store, max-age=60")])
        }));
        assert_eq!(label(&rig.get("/a")), "MISS");
        assert_eq!(label(&rig.get("/a")), "MISS");
        assert_eq!(rig.calls(), 2);
    }

    #[test]
    fn private_and_set_cookie_are_never_cached() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.target == "/p" {
                resp(200, "x", &[("Cache-Control", "private, max-age=60")])
            } else {
                resp(
                    200,
                    "x",
                    &[("Cache-Control", "max-age=60"), ("Set-Cookie", "s=1")],
                )
            }
        }));
        rig.get("/p");
        rig.get("/p");
        rig.get("/c");
        rig.get("/c");
        assert_eq!(rig.calls(), 4);
    }

    #[test]
    fn no_cache_response_is_revalidated_every_time() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.header("if-none-match").is_some() {
                resp(
                    304,
                    "",
                    &[("Cache-Control", "no-cache"), ("ETag", "\"v1\"")],
                )
            } else {
                resp(
                    200,
                    "x",
                    &[("Cache-Control", "no-cache"), ("ETag", "\"v1\"")],
                )
            }
        }));
        assert_eq!(label(&rig.get("/a")), "MISS");
        assert_eq!(label(&rig.get("/a")), "REVALIDATED");
        assert_eq!(label(&rig.get("/a")), "REVALIDATED");
        assert_eq!(rig.calls(), 3);
    }

    #[test]
    fn client_no_cache_forces_revalidation() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.header("if-none-match").is_some() {
                resp(
                    304,
                    "",
                    &[("Cache-Control", "max-age=60"), ("ETag", "\"v1\"")],
                )
            } else {
                resp(
                    200,
                    "x",
                    &[("Cache-Control", "max-age=60"), ("ETag", "\"v1\"")],
                )
            }
        }));
        rig.get("/a");
        let r = rig.get_with("/a", &[("Cache-Control", "no-cache")]);
        assert_eq!(label(&r), "REVALIDATED");
        let r = rig.get_with("/a", &[("Pragma", "no-cache")]);
        assert_eq!(label(&r), "REVALIDATED");
        let r = rig.get_with("/a", &[("Cache-Control", "max-age=0")]);
        assert_eq!(label(&r), "REVALIDATED");
        assert_eq!(rig.calls(), 4);
        // the client's own cache directives are not forwarded to the origin
        let seen = lock(&rig.mock.seen);
        assert!(seen.iter().all(|r| r.header("cache-control").is_none()));
    }

    #[test]
    fn client_no_store_bypasses() {
        let rig = Rig::simple(cacheable("x"));
        rig.get("/a");
        let r = rig.get_with("/a", &[("Cache-Control", "no-store")]);
        assert_eq!(label(&r), "BYPASS");
        assert_eq!(rig.calls(), 2);
    }

    #[test]
    fn matching_if_none_match_gets_304_from_cache() {
        let rig = Rig::simple(cacheable("hello"));
        rig.get("/a");
        let r = rig.get_with("/a", &[("If-None-Match", "\"v1\"")]);
        assert_eq!(r.status, 304);
        assert!(r.body.is_empty());
        assert_eq!(label(&r), "HIT");
        assert_eq!(r.header("etag"), Some("\"v1\""));
        assert_eq!(rig.calls(), 1);
        // weak comparison, lists and * all count
        assert_eq!(
            rig.get_with("/a", &[("If-None-Match", "W/\"v1\"")]).status,
            304
        );
        assert_eq!(
            rig.get_with("/a", &[("If-None-Match", "\"zzz\", \"v1\"")])
                .status,
            304
        );
        assert_eq!(rig.get_with("/a", &[("If-None-Match", "*")]).status, 304);
        // a different tag gets the full body
        let r = rig.get_with("/a", &[("If-None-Match", "\"other\"")]);
        assert_eq!(r.status, 200);
        assert_eq!(body(&r), "hello");
    }

    #[test]
    fn post_bypasses_and_evicts_the_cached_copy() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.method == "POST" {
                resp(201, "made", &[])
            } else {
                resp(200, "x", &[("Cache-Control", "max-age=60")])
            }
        }));
        rig.get("/a");
        assert_eq!(label(&rig.get("/a")), "HIT");
        let mut post = Request::new("POST", "/a");
        post.body = b"payload".to_vec();
        let r = rig.proxy.handle(&post);
        assert_eq!(label(&r), "BYPASS");
        assert_eq!(r.status, 201);
        {
            let seen = lock(&rig.mock.seen);
            let sent = seen.last().unwrap();
            assert_eq!(sent.method, "POST");
            assert_eq!(sent.body, b"payload");
        }
        assert_eq!(label(&rig.get("/a")), "MISS");
    }

    #[test]
    fn failed_post_does_not_evict() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.method == "POST" {
                resp(500, "boom", &[])
            } else {
                resp(200, "x", &[("Cache-Control", "max-age=60")])
            }
        }));
        rig.get("/a");
        rig.proxy.handle(&Request::new("POST", "/a"));
        assert_eq!(label(&rig.get("/a")), "HIT");
    }

    #[test]
    fn authorization_and_range_bypass_without_storing() {
        let rig = Rig::simple(cacheable("x"));
        let r = rig.get_with("/a", &[("Authorization", "Bearer t")]);
        assert_eq!(label(&r), "BYPASS");
        // nothing was stored, so the next anonymous request is a miss
        assert_eq!(label(&rig.get("/a")), "MISS");
        let r = rig.get_with("/a", &[("Range", "bytes=0-1")]);
        assert_eq!(label(&r), "BYPASS");
        // Authorization is forwarded to the origin untouched
        let seen = lock(&rig.mock.seen);
        assert_eq!(seen[0].header("authorization"), Some("Bearer t"));
    }

    #[test]
    fn bypass_does_not_serve_cached_data_to_authorized_requests() {
        let rig = Rig::simple(cacheable("public"));
        rig.get("/a");
        let r = rig.get_with("/a", &[("Authorization", "Bearer t")]);
        assert_eq!(label(&r), "BYPASS");
        assert_eq!(rig.calls(), 2);
    }

    #[test]
    fn head_is_served_from_a_cached_get() {
        let rig = Rig::simple(cacheable("hello"));
        rig.get("/a");
        let r = rig.proxy.handle(&Request::new("HEAD", "/a"));
        assert_eq!(label(&r), "HIT");
        assert_eq!(rig.calls(), 1);
    }

    #[test]
    fn head_miss_goes_to_the_origin_as_get() {
        let rig = Rig::simple(cacheable("hello"));
        let r = rig.proxy.handle(&Request::new("HEAD", "/a"));
        assert_eq!(label(&r), "MISS");
        assert_eq!(lock(&rig.mock.seen)[0].method, "GET");
        assert_eq!(label(&rig.get("/a")), "HIT");
    }

    #[test]
    fn vary_keeps_variants_apart() {
        let rig = Rig::simple(Box::new(|req, _| {
            let enc = req.header("accept-encoding").unwrap_or("none").to_string();
            resp(
                200,
                &format!("body-for-{enc}"),
                &[("Cache-Control", "max-age=60"), ("Vary", "Accept-Encoding")],
            )
        }));
        let gz = [("Accept-Encoding", "gzip")];
        let id = [("Accept-Encoding", "identity")];
        assert_eq!(body(&rig.get_with("/a", &gz)), "body-for-gzip");
        assert_eq!(label(&rig.get_with("/a", &gz)), "HIT");
        let r = rig.get_with("/a", &id);
        assert_eq!(label(&r), "MISS");
        assert_eq!(body(&r), "body-for-identity");
        // this cache keeps one variant per URL: the newest replaced the first
        assert_eq!(label(&rig.get_with("/a", &id)), "HIT");
        assert_eq!(label(&rig.get_with("/a", &gz)), "MISS");
    }

    #[test]
    fn lru_evicts_when_the_byte_budget_is_full() {
        let big = "x".repeat(700);
        let rig = Rig::new(
            1000,
            Duration::ZERO,
            Box::new(move |_, _| resp(200, &big, &[("Cache-Control", "max-age=60")])),
        );
        rig.get("/a");
        assert_eq!(label(&rig.get("/a")), "HIT");
        rig.get("/b"); // does not fit next to /a
        assert_eq!(label(&rig.get("/a")), "MISS");
        let stats = body(&rig.get("/__stats"));
        assert!(stats.contains("\"evictions\":"), "{stats}");
        assert!(!stats.contains("\"evictions\":0"), "{stats}");
    }

    #[test]
    fn entries_bigger_than_the_cache_are_not_stored() {
        let rig = Rig::new(
            100,
            Duration::ZERO,
            Box::new(|_, _| resp(200, &"y".repeat(500), &[("Cache-Control", "max-age=60")])),
        );
        assert_eq!(label(&rig.get("/a")), "MISS");
        assert_eq!(label(&rig.get("/a")), "MISS");
    }

    #[test]
    fn origin_errors_become_502() {
        let rig = Rig::simple(Box::new(|_, _| {
            Err(io::Error::new(io::ErrorKind::ConnectionRefused, "down"))
        }));
        let r = rig.get("/a");
        assert_eq!(r.status, 502);
        assert_eq!(label(&r), "ERROR");
        let stats = body(&rig.get("/__stats"));
        assert!(stats.contains("\"origin_errors\":1"), "{stats}");
    }

    #[test]
    fn errors_from_the_origin_are_not_cached() {
        let rig = Rig::simple(Box::new(|_, n| {
            if n == 1 {
                resp(500, "oops", &[("Cache-Control", "max-age=60")])
            } else {
                resp(200, "fine", &[("Cache-Control", "max-age=60")])
            }
        }));
        assert_eq!(rig.get("/a").status, 500);
        let r = rig.get("/a");
        assert_eq!(r.status, 200);
        assert_eq!(label(&r), "MISS");
    }

    #[test]
    fn host_header_is_rewritten_for_the_origin() {
        let rig = Rig::simple(cacheable("x"));
        rig.get_with(
            "/a",
            &[("Host", "proxy.local:8080"), ("Connection", "keep-alive")],
        );
        let seen = lock(&rig.mock.seen);
        assert_eq!(seen[0].header("host"), Some("origin.test"));
        assert_eq!(seen[0].header("connection"), None);
    }

    #[test]
    fn stats_endpoint_reports_counters() {
        let rig = Rig::simple(cacheable("hello"));
        rig.get("/a");
        rig.get("/a");
        rig.get("/a");
        let r = rig.get("/__stats");
        assert_eq!(r.header("content-type"), Some("application/json"));
        let s = body(&r);
        assert!(s.contains("\"hits\":2"), "{s}");
        assert!(s.contains("\"misses\":1"), "{s}");
        assert!(s.contains("\"hit_ratio\":0.6667"), "{s}");
        assert!(s.contains("\"entries\":1"), "{s}");
        assert!(s.contains("\"origin_requests\":1"), "{s}");
        // asking for stats is not a cache lookup
        assert_eq!(rig.calls(), 1);
    }

    #[test]
    fn non_origin_form_targets_are_rejected() {
        let rig = Rig::simple(cacheable("x"));
        let r = rig.proxy.handle(&Request::new("GET", "http://evil.test/"));
        assert_eq!(r.status, 400);
        assert_eq!(rig.calls(), 0);
    }

    #[test]
    fn a_304_that_adds_no_store_drops_the_entry() {
        let rig = Rig::simple(Box::new(|req, _| {
            if req.header("if-none-match").is_some() {
                resp(304, "", &[("Cache-Control", "no-store")])
            } else {
                resp(
                    200,
                    "x",
                    &[("Cache-Control", "max-age=10"), ("ETag", "\"v1\"")],
                )
            }
        }));
        rig.get("/a");
        rig.advance(11);
        assert_eq!(label(&rig.get("/a")), "REVALIDATED");
        assert_eq!(label(&rig.get("/a")), "MISS");
    }

    #[test]
    fn many_simultaneous_requests_make_one_origin_call() {
        let rig = Arc::new(Rig::new(
            1 << 20,
            Duration::from_millis(150),
            Box::new(|_, _| resp(200, "shared", &[("Cache-Control", "max-age=60")])),
        ));
        let mut handles = Vec::new();
        for _ in 0..12 {
            let rig = rig.clone();
            handles.push(std::thread::spawn(move || {
                let r = rig.get("/slow");
                (body(&r), label(&r).to_string())
            }));
        }
        let results: Vec<(String, String)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(rig.calls(), 1, "the origin must be asked exactly once");
        assert!(results.iter().all(|(b, _)| b == "shared"));
        let misses = results.iter().filter(|(_, l)| l == "MISS").count();
        assert_eq!(misses, 1, "exactly one request led the fetch: {results:?}");
        assert!(results
            .iter()
            .all(|(_, l)| matches!(l.as_str(), "MISS" | "COALESCED" | "HIT")));
        let coalesced = rig.proxy.stats.coalesced.load(Relaxed);
        assert!(coalesced >= 1, "expected followers to share the fetch");
        // the leader's flight is cleaned up
        assert!(lock(&rig.proxy.inflight).is_empty());
    }

    #[test]
    fn followers_get_the_error_when_the_leader_fails() {
        let rig = Arc::new(Rig::new(
            1 << 20,
            Duration::from_millis(100),
            Box::new(|_, _| Err(io::Error::new(io::ErrorKind::TimedOut, "slow"))),
        ));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let rig = rig.clone();
                std::thread::spawn(move || rig.get("/x").status)
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 502);
        }
        assert!(lock(&rig.proxy.inflight).is_empty());
    }

    #[test]
    fn a_panicking_leader_releases_its_followers() {
        let rig = Arc::new(Rig::new(
            1 << 20,
            Duration::from_millis(100),
            Box::new(|_, n| {
                if n == 1 {
                    panic!("origin handler exploded");
                }
                resp(200, "ok", &[("Cache-Control", "max-age=60")])
            }),
        ));
        let leader = {
            let rig = rig.clone();
            std::thread::spawn(move || rig.get("/boom"))
        };
        std::thread::sleep(Duration::from_millis(30));
        let follower = {
            let rig = rig.clone();
            std::thread::spawn(move || rig.get("/boom").status)
        };
        assert!(leader.join().is_err(), "the leader thread panicked");
        assert_eq!(follower.join().unwrap(), 502);
        assert!(lock(&rig.proxy.inflight).is_empty());
    }

    #[test]
    fn etag_matching_rules() {
        assert!(etag_matches("\"a\"", "\"a\""));
        assert!(etag_matches("W/\"a\"", "\"a\""));
        assert!(etag_matches("\"a\"", "W/\"a\""));
        assert!(etag_matches("\"x\", \"a\"", "\"a\""));
        assert!(etag_matches("*", "\"a\""));
        assert!(!etag_matches("\"b\"", "\"a\""));
    }
}
