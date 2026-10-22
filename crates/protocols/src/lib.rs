//! gRPC schema and client/server stubs for the borgd daemon.
//!
//! This crate is generated from `proto/*.proto` via `tonic-prost-build` at
//! compile time. Re-export the generated modules under stable paths so
//! downstream crates (`borgd`, `borg` CLI) don't depend on the generator's
//! naming.

#![warn(missing_docs)]

pub mod capabilities;
#[cfg(windows)]
pub mod pipe;
#[cfg(unix)]
pub mod uds;

/// Connect to borgd's local listener at `path`: its unix socket, or on
/// Windows the named pipe whose name borgd writes there.
#[cfg(windows)]
pub use pipe::connect as connect_local;
/// Connect to borgd's local listener at `path`: its unix socket, or on
/// Windows the named pipe whose name borgd writes there.
#[cfg(unix)]
pub use uds::connect as connect_local;

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

/// Telemetry streaming RPCs.
pub mod telemetry {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.telemetry.v1");
}

/// PKI / pairing / audit RPCs.
pub mod pki {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.pki.v1");
}

/// Voice and phone provider catalog plus transcription.
pub mod voice {
    #![allow(missing_docs, clippy::all)]
    tonic::include_proto!("borg.voice.v1");
}

/// Re-export tonic for downstream crates so they don't have to pin a version.
pub use tonic;

/// Placeholder URI used when constructing a tonic `Endpoint` that will be
/// driven by a custom connector (UDS or pre-paired TCP). The string itself
/// is never dialed — tonic requires *some* URI to satisfy its `try_from`,
/// and `.connect_with_connector(...)` supplies the actual transport.
pub const TONIC_UDS_PLACEHOLDER_URL: &str = "http://[::]:50051";
