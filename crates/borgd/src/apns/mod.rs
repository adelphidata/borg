//! APNs push pipeline — register iOS devices, wake them with silent
//! `content-available: 1` pushes when an inbound gateway message arrives.
//!
//! The App Attest verifier is intentionally a stub
//! ([`attest::verify_attestation`] returns `AttestError::NotImplemented`)
//! so the full vertical can land without committing to a CBOR + Apple-CA
//! implementation. Follow-up tracked in `docs/ROADMAP.md` under
//! "Networking & deployment follow-ups".

/// App Attest verifier (stub) — accepts the inputs the production
/// verifier will, but returns `NotImplemented` for now.
pub mod attest;
/// In-memory challenge nonce store (5-minute TTL) used by
/// `RegisterDevice` to bind a device's first registration to a
/// server-issued random challenge.
pub mod challenge_store;
/// APNs HTTP/2 client wrapper (token-based JWT-ES256 auth).
pub mod client;
/// Push trigger — `wake_paired_devices(db, recipient)`. Called from
/// the gateway's inbound-message handler.
pub mod trigger;

pub use attest::{verify_attestation, AttestError, VerifiedAttestation};
pub use challenge_store::ChallengeStore;
pub use client::ApnsClient;
pub use trigger::wake_paired_devices;
