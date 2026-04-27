//! Device pairing flow.
//!
//! `device.rs` runs SPAKE2 over a plaintext bootstrap UDS socket — the only
//! plaintext surface in the daemon, and only enabled while there's at least
//! one active pairing code. After PAKE succeeds, the daemon issues a
//! CA-signed client cert and the connection moves to the main mTLS listener.

pub mod device;

pub use device::{run_pairing_handshake, BootstrapListener};
