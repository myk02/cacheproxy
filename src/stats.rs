//! Counters behind the `/__stats` endpoint.

use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
pub struct Stats {
    /// Every request the proxy handled.
    pub requests: AtomicU64,
    /// Served from cache without asking the origin.
    pub hits: AtomicU64,
    /// Not usable from cache; fetched from the origin.
    pub misses: AtomicU64,
    /// Stale entry confirmed by the origin with `304 Not Modified`.
    pub revalidated: AtomicU64,
    /// Waited for another request's origin fetch instead of making its own.
    pub coalesced: AtomicU64,
    /// Passed straight through (POST, `Authorization`, `Range`, `no-store`...).
    pub bypass: AtomicU64,
    /// Requests sent to the origin.
    pub origin_requests: AtomicU64,
    /// Origin requests that failed (connection errors, bad responses).
    pub origin_errors: AtomicU64,
}
