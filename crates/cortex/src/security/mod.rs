//! Trust-boundary cluster: prompt-injection scoring (`sanitize`), input
//! validation, secret detection/resolution, and skill audit. Anything that
//! sits between untrusted input and the agent loop lives here.

pub mod advisory;
pub mod env_allowlist;
pub mod sanitize;
pub mod secrets;
pub mod secrets_resolve;
pub mod skill;
pub mod startup_audit;
pub mod validation;
