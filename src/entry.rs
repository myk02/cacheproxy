//! One cached response and its freshness maths (RFC 9111 section 4.2).

use std::time::Instant;

use crate::http::{header_get, Body, Headers};

#[derive(Debug, Clone)]
pub struct Entry {
    pub status: u16,
    pub reason: String,
    /// Stored headers. Hop-by-hop headers, `Content-Length` and `Age` are
    /// left out; they are recomputed when the entry is served.
    pub headers: Headers,
    pub body: Body,
    /// When we last received this response (or last revalidated it).
    pub stored_at: Instant,
    /// The `Age` the origin reported when we received it, in seconds.
    pub initial_age: u64,
    /// How long it stays fresh, in seconds, counted from `initial_age`.
    pub lifetime: u64,
    /// `Cache-Control: no-cache`: may be stored but must be revalidated
    /// before every reuse.
    pub no_cache: bool,
    /// The request header values this response varies on (`Vary`):
    /// (lowercase header name, value the request had, or "" if absent).
    pub vary: Vec<(String, String)>,
}

impl Entry {
    pub fn etag(&self) -> Option<&str> {
        header_get(&self.headers, "etag")
    }

    pub fn last_modified(&self) -> Option<&str> {
        header_get(&self.headers, "last-modified")
    }

    pub fn has_validator(&self) -> bool {
        self.etag().is_some() || self.last_modified().is_some()
    }

    /// Age in whole seconds: what the origin said plus the time we held it.
    pub fn current_age(&self, now: Instant) -> u64 {
        let held = now.saturating_duration_since(self.stored_at).as_secs();
        self.initial_age.saturating_add(held)
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        !self.no_cache && self.current_age(now) < self.lifetime
    }

    /// Does this stored variant match the request's `Vary` headers?
    pub fn vary_matches(&self, request_headers: &Headers) -> bool {
        self.vary
            .iter()
            .all(|(name, value)| header_get(request_headers, name).unwrap_or("") == value)
    }

    /// Approximate memory cost, used for the cache's byte budget.
    pub fn size(&self, key_len: usize) -> usize {
        let headers: usize = self
            .headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 4)
            .sum();
        let vary: usize = self.vary.iter().map(|(k, v)| k.len() + v.len()).sum();
        key_len + self.body.len() + headers + vary + 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn entry(lifetime: u64, initial_age: u64, no_cache: bool) -> Entry {
        Entry {
            status: 200,
            reason: "OK".into(),
            headers: vec![("ETag".into(), "\"v1\"".into())],
            body: Arc::new(b"hello".to_vec()),
            stored_at: Instant::now(),
            initial_age,
            lifetime,
            no_cache,
            vary: vec![],
        }
    }

    #[test]
    fn freshness_over_time() {
        let e = entry(60, 0, false);
        let t0 = e.stored_at;
        assert!(e.is_fresh(t0));
        assert!(e.is_fresh(t0 + Duration::from_secs(59)));
        assert!(!e.is_fresh(t0 + Duration::from_secs(60)));
        assert_eq!(e.current_age(t0 + Duration::from_secs(42)), 42);
    }

    #[test]
    fn origin_age_counts() {
        let e = entry(60, 50, false);
        let t0 = e.stored_at;
        assert!(e.is_fresh(t0 + Duration::from_secs(9)));
        assert!(!e.is_fresh(t0 + Duration::from_secs(10)));
        assert_eq!(e.current_age(t0), 50);
    }

    #[test]
    fn zero_lifetime_and_no_cache_are_never_fresh() {
        let e = entry(0, 0, false);
        assert!(!e.is_fresh(e.stored_at));
        let e = entry(600, 0, true);
        assert!(!e.is_fresh(e.stored_at));
    }

    #[test]
    fn validators() {
        let e = entry(1, 0, false);
        assert_eq!(e.etag(), Some("\"v1\""));
        assert_eq!(e.last_modified(), None);
        assert!(e.has_validator());
    }

    #[test]
    fn vary_matching() {
        let mut e = entry(1, 0, false);
        e.vary = vec![("accept-encoding".into(), "gzip".into())];
        let gz: Headers = vec![("Accept-Encoding".into(), "gzip".into())];
        let id: Headers = vec![("Accept-Encoding".into(), "identity".into())];
        assert!(e.vary_matches(&gz));
        assert!(!e.vary_matches(&id));
        assert!(!e.vary_matches(&vec![]));
        e.vary = vec![("accept-language".into(), String::new())];
        assert!(e.vary_matches(&vec![])); // absent matches an empty recorded value
    }

    #[test]
    fn size_grows_with_body() {
        let small = entry(1, 0, false);
        let mut big = entry(1, 0, false);
        big.body = Arc::new(vec![0; 1000]);
        assert!(big.size(10) >= small.size(10) + 995);
    }
}
