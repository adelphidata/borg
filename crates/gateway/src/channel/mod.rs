//! Shared channel infrastructure: the unified `Channel` trait, native-channel
//! initialization wiring, the discovery registry, and per-request routing /
//! binding resolution. Per-channel implementations live alongside in
//! `gateway::{slack, discord, telegram, …}`.

/// Native client initialization for all built-in channels.
pub(crate) mod channel_init;
/// `Channel` trait and shared lifecycle types.
pub mod channel_trait;
/// Channel discovery registry for the daemon.
pub mod registry;
/// Per-request channel routing and gateway-binding resolution.
pub mod routing;
