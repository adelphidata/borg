//! gRPC schema and client/server stubs for the borgd daemon.
//!
//! This crate is generated from `proto/*.proto` via `tonic-build` at compile
//! time. Re-export the generated modules under stable paths so downstream
//! crates (`borgd`, `borg` CLI) don't depend on tonic-build's naming.

#![warn(missing_docs)]

pub mod capabilities;

/// Session service — agent interaction.
pub mod session {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.session.v1");
}

/// Capability bidi-stream.
pub mod capability {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.capability.v1");
}

/// Status read-only RPCs.
pub mod status {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.status.v1");
}

/// Admin lifecycle / data-access RPCs.
pub mod admin {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.admin.v1");
}

/// Re-export tonic for downstream crates so they don't have to pin a version.
pub use tonic;
