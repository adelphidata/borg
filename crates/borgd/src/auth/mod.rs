//! Auth: mTLS server config + peer-cert verifier + principal extraction.
//!
//! Every gRPC connection — UDS or TCP — goes through this layer. The auth
//! interceptor extracts the peer certificate from the TLS session, runs the
//! configured `Verifier`, checks the revocation list, and attaches a
//! `Principal` to the request extensions.

pub mod ca_bootstrap;
pub mod interceptor;
pub mod tls;
pub mod verifier;

pub use ca_bootstrap::{ensure_root_ca, ensure_server_cert, CaMaterial};
pub use interceptor::{auth_interceptor, Principal};
pub use tls::{build_server_tls_config, ServerCertMaterial};
pub use verifier::{TofuVerifier, Verifier, VerifierError};
