//! Provider trait, category enum, and registry — the v1 abstraction layer
//! over the existing per-provider transports in [`crate::llm`].
//!
//! Today, the actual HTTP calls still go through the monolithic
//! [`crate::llm::LlmClient`]. This module sits in front of that, providing:
//!
//! - **Category tagging**: every provider is `PublicCloud` / `Local` /
//!   `BYOEndpoint`. Air-gap mode (`providers.outbound_allowed = false`)
//!   refuses everything that is not `Local`.
//! - **Budget routing**: the budget hook resolves the active provider via
//!   the registry to look up its rate-card row.
//! - **A v1 BYO surface**: `borg providers add --base-url ...` inserts a
//!   row that the registry serves as `BYOEndpoint`, OpenAI-compatible.
//!
//! The full per-provider trait refactor (each transport in its own file
//! implementing `chat()` etc.) is deferred — the existing `LlmClient` is
//! still the transport, and we don't pay the rewrite cost until v1
//! ships.

pub mod category;
pub mod registry;
pub mod row;

#[cfg(test)]
mod tests;

pub use category::ProviderCategory;
pub use registry::{ProviderRegistry, ResolveError};
pub use row::ProviderRow;
