//! Parsing of the `Cache-Control` header (RFC 9111 section 5.2).

/// Largest delta-seconds value we accept. RFC 9111 says caches may clamp
/// bigger values to 2^31.
pub const MAX_DELTA_SECONDS: u64 = 2_147_483_648;

/// The directives this proxy understands. Unknown directives are ignored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheControl {
    pub no_store: bool,
    pub no_cache: bool,
    pub private: bool,
    pub public: bool,
    pub must_revalidate: bool,
    pub max_age: Option<u64>,
    pub s_maxage: Option<u64>,
}

impl CacheControl {
    /// Parse one header value, or several values joined with commas.
    ///
    /// Notes on choices made here:
    /// * directive names are case-insensitive
    /// * `private="x"` and `no-cache="x"` (the field-name forms) are treated
    ///   like the plain forms. That is the safe, conservative reading.
    /// * a `max-age` with a missing or invalid value counts as `0` (stale)
    /// * if a directive is repeated, the smallest value wins
    pub fn parse(value: &str) -> Self {
        let mut cc = CacheControl::default();
        for directive in split_directives(value) {
            let (name, arg) = match directive.split_once('=') {
                Some((n, a)) => (n.trim(), Some(unquote(a.trim()))),
                None => (directive.trim(), None),
            };
            match name.to_ascii_lowercase().as_str() {
                "no-store" => cc.no_store = true,
                "no-cache" => cc.no_cache = true,
                "private" => cc.private = true,
                "public" => cc.public = true,
                "must-revalidate" => cc.must_revalidate = true,
                "max-age" => cc.max_age = Some(merge_min(cc.max_age, delta(arg))),
                "s-maxage" => cc.s_maxage = Some(merge_min(cc.s_maxage, delta(arg))),
                _ => {}
            }
        }
        cc
    }
}

fn merge_min(old: Option<u64>, new: u64) -> u64 {
    old.map_or(new, |o| o.min(new))
}

/// Parse delta-seconds. Invalid input becomes 0 (treated as already stale).
fn delta(arg: Option<&str>) -> u64 {
    let Some(s) = arg else { return 0 };
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    match s.parse::<u64>() {
        Ok(n) => n.min(MAX_DELTA_SECONDS),
        Err(_) => MAX_DELTA_SECONDS, // all digits but too big to fit
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(s)
}

/// Split on commas that are not inside a quoted string.
fn split_directives(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ',' if !in_quote => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_directives() {
        let cc = CacheControl::parse("public, max-age=60");
        assert!(cc.public);
        assert_eq!(cc.max_age, Some(60));
        assert!(!cc.no_store);
    }

    #[test]
    fn names_are_case_insensitive_and_spaces_are_ignored() {
        let cc = CacheControl::parse("  No-Store ,  MAX-AGE = 5 , S-MaxAge=9 ");
        assert!(cc.no_store);
        assert_eq!(cc.max_age, Some(5));
        assert_eq!(cc.s_maxage, Some(9));
    }

    #[test]
    fn quoted_values_and_commas_inside_quotes() {
        let cc = CacheControl::parse(r#"private="set-cookie, x-secret", max-age="30""#);
        assert!(cc.private);
        assert_eq!(cc.max_age, Some(30));
    }

    #[test]
    fn invalid_max_age_is_zero() {
        assert_eq!(CacheControl::parse("max-age=abc").max_age, Some(0));
        assert_eq!(CacheControl::parse("max-age=-5").max_age, Some(0));
        assert_eq!(CacheControl::parse("max-age").max_age, Some(0));
        assert_eq!(CacheControl::parse("max-age=").max_age, Some(0));
    }

    #[test]
    fn huge_values_are_clamped() {
        let cc = CacheControl::parse("max-age=99999999999999999999999");
        assert_eq!(cc.max_age, Some(MAX_DELTA_SECONDS));
        let cc = CacheControl::parse("max-age=4294967296");
        assert_eq!(cc.max_age, Some(MAX_DELTA_SECONDS));
    }

    #[test]
    fn repeated_directive_takes_smallest() {
        let cc = CacheControl::parse("max-age=100, max-age=10");
        assert_eq!(cc.max_age, Some(10));
    }

    #[test]
    fn unknown_directives_are_ignored() {
        let cc = CacheControl::parse("immutable, stale-while-revalidate=30, max-age=1");
        assert_eq!(cc.max_age, Some(1));
        assert!(!cc.no_cache);
    }

    #[test]
    fn empty_input() {
        assert_eq!(CacheControl::parse(""), CacheControl::default());
        assert_eq!(CacheControl::parse(" , ,"), CacheControl::default());
    }

    #[test]
    fn must_revalidate_and_no_cache() {
        let cc = CacheControl::parse("no-cache, must-revalidate");
        assert!(cc.no_cache && cc.must_revalidate);
    }
}
