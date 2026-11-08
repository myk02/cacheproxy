//! Storage policy: may this response be cached, and for how long?
//!
//! This is a *shared* cache (a proxy in front of many clients), so
//! `private` responses are never stored.
//!
//! Decisions, in order:
//! 1. only some status codes are cacheable by default
//! 2. `no-store` and `private` mean "do not store"
//! 3. responses with `Set-Cookie` are not stored (conservative: they are
//!    often per-user)
//! 4. `Vary: *` means "never matches", so it is not stored
//! 5. freshness lifetime = `s-maxage`, else `max-age`, else `Expires - Date`,
//!    else 0 (we do not guess with heuristic freshness)
//! 6. a response that is always stale and has no validator (`ETag` or
//!    `Last-Modified`) can never be reused, so it is not stored

use crate::cache_control::CacheControl;
use crate::http::{header_get, header_get_all, Headers};
use crate::httpdate;

/// Status codes that may be stored (RFC 9110 section 15.1, cacheable by default).
pub const CACHEABLE_STATUSES: [u16; 7] = [200, 203, 204, 301, 308, 404, 410];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePolicy {
    /// Freshness lifetime in seconds.
    pub lifetime: u64,
    /// The `Age` header from the origin (0 if absent).
    pub initial_age: u64,
    /// Store it, but revalidate before every reuse.
    pub no_cache: bool,
    /// Lowercase names of the request headers this response varies on.
    pub vary: Vec<String>,
}

/// Parse all `Cache-Control` header lines of a message as one value.
pub fn cache_control_of(headers: &Headers) -> CacheControl {
    CacheControl::parse(&header_get_all(headers, "cache-control").join(","))
}

/// `None` means "do not store this response".
pub fn evaluate(status: u16, headers: &Headers) -> Option<StorePolicy> {
    if !CACHEABLE_STATUSES.contains(&status) {
        return None;
    }
    let cc = cache_control_of(headers);
    if cc.no_store || cc.private {
        return None;
    }
    if header_get(headers, "set-cookie").is_some() {
        return None;
    }
    let mut vary = Vec::new();
    for value in header_get_all(headers, "vary") {
        for name in value.split(',') {
            let name = name.trim();
            if name == "*" {
                return None;
            }
            if !name.is_empty() {
                vary.push(name.to_ascii_lowercase());
            }
        }
    }

    let lifetime = freshness_lifetime(&cc, headers);
    let has_validator =
        header_get(headers, "etag").is_some() || header_get(headers, "last-modified").is_some();
    if (lifetime == 0 || cc.no_cache) && !has_validator {
        return None;
    }

    let initial_age = header_get(headers, "age")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);

    Some(StorePolicy {
        lifetime,
        initial_age,
        no_cache: cc.no_cache,
        vary,
    })
}

fn freshness_lifetime(cc: &CacheControl, headers: &Headers) -> u64 {
    if let Some(s) = cc.s_maxage {
        return s;
    }
    if let Some(m) = cc.max_age {
        return m;
    }
    if let Some(expires) = header_get(headers, "expires") {
        // an invalid Expires value means "already expired" (RFC 9111 5.3)
        let Some(expires) = httpdate::parse(expires) else {
            return 0;
        };
        let date = header_get(headers, "date")
            .and_then(httpdate::parse)
            .unwrap_or_else(httpdate::now_unix);
        return expires.saturating_sub(date);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn max_age_is_stored() {
        let p = evaluate(200, &h(&[("Cache-Control", "max-age=60")])).unwrap();
        assert_eq!(p.lifetime, 60);
        assert_eq!(p.initial_age, 0);
        assert!(!p.no_cache);
    }

    #[test]
    fn s_maxage_beats_max_age() {
        let p = evaluate(200, &h(&[("Cache-Control", "max-age=10, s-maxage=99")])).unwrap();
        assert_eq!(p.lifetime, 99);
    }

    #[test]
    fn expires_minus_date() {
        let p = evaluate(
            200,
            &h(&[
                ("Date", "Sun, 06 Nov 1994 08:49:37 GMT"),
                ("Expires", "Sun, 06 Nov 1994 08:50:37 GMT"),
            ]),
        )
        .unwrap();
        assert_eq!(p.lifetime, 60);
    }

    #[test]
    fn max_age_beats_expires() {
        let p = evaluate(
            200,
            &h(&[
                ("Cache-Control", "max-age=5"),
                ("Date", "Sun, 06 Nov 1994 08:49:37 GMT"),
                ("Expires", "Sun, 06 Nov 1994 09:49:37 GMT"),
            ]),
        )
        .unwrap();
        assert_eq!(p.lifetime, 5);
    }

    #[test]
    fn invalid_or_past_expires_means_stale() {
        assert_eq!(
            evaluate(200, &h(&[("Expires", "0"), ("ETag", "\"a\"")]))
                .unwrap()
                .lifetime,
            0
        );
        let past = h(&[
            ("Date", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("Expires", "Sun, 06 Nov 1994 08:00:00 GMT"),
            ("ETag", "\"a\""),
        ]);
        assert_eq!(evaluate(200, &past).unwrap().lifetime, 0);
    }

    #[test]
    fn no_store_and_private_are_never_stored() {
        assert!(evaluate(200, &h(&[("Cache-Control", "no-store, max-age=60")])).is_none());
        assert!(evaluate(200, &h(&[("Cache-Control", "private, max-age=60")])).is_none());
    }

    #[test]
    fn set_cookie_is_not_stored() {
        assert!(evaluate(
            200,
            &h(&[("Cache-Control", "max-age=60"), ("Set-Cookie", "a=b")])
        )
        .is_none());
    }

    #[test]
    fn vary_star_is_not_stored_and_names_are_collected() {
        assert!(evaluate(200, &h(&[("Cache-Control", "max-age=60"), ("Vary", "*")])).is_none());
        let p = evaluate(
            200,
            &h(&[
                ("Cache-Control", "max-age=60"),
                ("Vary", "Accept-Encoding, Accept-Language"),
            ]),
        )
        .unwrap();
        assert_eq!(p.vary, vec!["accept-encoding", "accept-language"]);
    }

    #[test]
    fn nothing_to_reuse_is_not_stored() {
        assert!(evaluate(200, &h(&[])).is_none());
        assert!(evaluate(200, &h(&[("Cache-Control", "max-age=0")])).is_none());
        assert!(evaluate(200, &h(&[("Cache-Control", "no-cache")])).is_none());
    }

    #[test]
    fn validator_only_is_stored_with_zero_lifetime() {
        let p = evaluate(200, &h(&[("ETag", "\"v1\"")])).unwrap();
        assert_eq!(p.lifetime, 0);
        let p = evaluate(
            200,
            &h(&[("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT")]),
        )
        .unwrap();
        assert_eq!(p.lifetime, 0);
    }

    #[test]
    fn no_cache_with_validator_is_stored_but_flagged() {
        let p = evaluate(
            200,
            &h(&[("Cache-Control", "no-cache, max-age=60"), ("ETag", "\"v\"")]),
        )
        .unwrap();
        assert!(p.no_cache);
    }

    #[test]
    fn status_codes() {
        let cc = h(&[("Cache-Control", "max-age=60")]);
        for s in [200, 203, 204, 301, 308, 404, 410] {
            assert!(evaluate(s, &cc).is_some(), "status {s}");
        }
        for s in [
            201, 202, 206, 302, 303, 304, 307, 400, 401, 403, 500, 502, 503,
        ] {
            assert!(evaluate(s, &cc).is_none(), "status {s}");
        }
    }

    #[test]
    fn age_header_is_read() {
        let p = evaluate(200, &h(&[("Cache-Control", "max-age=60"), ("Age", "20")])).unwrap();
        assert_eq!(p.initial_age, 20);
        let p = evaluate(200, &h(&[("Cache-Control", "max-age=60"), ("Age", "junk")])).unwrap();
        assert_eq!(p.initial_age, 0);
    }

    #[test]
    fn multiple_cache_control_lines_are_combined() {
        let headers = h(&[
            ("Cache-Control", "max-age=60"),
            ("Cache-Control", "no-store"),
        ]);
        assert!(evaluate(200, &headers).is_none());
    }
}
