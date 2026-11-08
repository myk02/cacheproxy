//! # cacheproxy
//!
//! A small HTTP caching reverse proxy with zero dependencies.
//!
//! Clients talk to the proxy instead of the origin API. The proxy keeps
//! responses in a byte-capped LRU cache and follows the HTTP caching rules
//! (`Cache-Control`, `Expires`, `ETag`, `Last-Modified`, `Vary`, `Age`).
//!
//! Module map:
//!
//! * [`cache_control`] - parses `Cache-Control` header values
//! * [`httpdate`]      - parses and formats HTTP dates
//! * [`lru`]           - byte-capped LRU cache with O(1) operations
//! * [`policy`]        - decides if a response may be stored, and for how long
//! * [`entry`]         - one cached response plus its freshness maths
//! * [`http`]          - minimal HTTP/1.1 reader and writer
//! * [`upstream`]      - how the proxy talks to the origin (a trait, so it is mockable)
//! * [`proxy`]         - the brain: HIT / MISS / REVALIDATED / COALESCED logic
//! * [`server`]        - the TCP listener, one thread per connection
//! * [`stats`]         - counters behind the `/__stats` endpoint

pub mod cache_control;
pub mod entry;
pub mod http;
pub mod httpdate;
pub mod lru;
pub mod policy;
pub mod proxy;
pub mod server;
pub mod stats;
pub mod upstream;
