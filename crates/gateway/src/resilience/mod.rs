//! Fault-tolerance primitives shared across channels: backoff, retry,
//! circuit breakers, rate limiting, HTTP retry, and challenge-throttling.

pub mod backoff;
pub mod challenge_throttle;
pub mod circuit_breaker;
pub mod http_retry;
pub mod rate_limit;
pub mod retry;
