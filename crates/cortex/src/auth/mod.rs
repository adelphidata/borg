//! Authentication primitives for non-mTLS ingress surfaces.
//!
//! The Borg daemon's gRPC surface uses mTLS. The HTTP ingress surfaces
//! (OpenAI-compat, generic webhook, MCP HTTP) cannot always require
//! mTLS — many third-party clients (the `openai` Python SDK, ChatBox,
//! Claude Desktop) only speak bearer auth. This module is the
//! supporting credential type for those surfaces: GitHub-PAT-style
//! opaque bearer tokens, hashed at rest.
//!
//! Tokens here are NOT a substitute for mTLS on the gRPC kernel —
//! they only authenticate HTTP ingress.

pub mod tokens;
