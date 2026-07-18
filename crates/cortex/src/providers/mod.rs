//! Provider trait, category enum, and registry — a lookup/classification
//! layer in front of the per-provider transports in [`crate::llm`]. The
//! actual HTTP calls still go through the monolithic
//! [`crate::llm::LlmClient`]; this module does not implement transports.
//!
//! - **Category tagging**: every provider is `PublicCloud` / `Local` /
//!   `BYOEndpoint`. Air-gap mode (`providers.outbound_allowed = false`)
//!   refuses everything that is not `Local`.
//! - **Budget routing**: the budget hook resolves the active provider via
//!   the registry to look up its rate-card row.
//! - **BYO surface**: `borg providers add --base-url ...` inserts a row
//!   that the registry serves as `BYOEndpoint`, OpenAI-compatible.

pub mod category;
pub mod registry;
pub mod row;

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests;

pub use category::ProviderCategory;
pub use registry::{ProviderRegistry, ResolveError};
pub use row::ProviderRow;
