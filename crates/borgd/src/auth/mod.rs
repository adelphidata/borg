//! Auth: mTLS server config + peer-cert verifier + principal extraction.
//!
//! Every gRPC connection — UDS or TCP — goes through this layer. The auth
//! interceptor extracts the peer certificate from the TLS session, runs the
//! configured `Verifier`, checks the revocation list, and attaches a
//! `Principal` to the request extensions.

pub mod ca_bootstrap;
pub mod interceptor;
pub mod kek_factory;
pub mod san;
pub mod tailscale_verifier;
pub mod tls;
pub mod verifier;

use std::sync::Arc;
use tokio::sync::Mutex;

use cortex::config::VerifierKind;
use cortex::db::Database;

pub use ca_bootstrap::{ensure_root_ca, ensure_server_cert, CaMaterial};
pub use interceptor::{auth_interceptor, Principal};
pub use san::derive_server_san;
pub use tailscale_verifier::{
    TailscaleCliSource, TailscaleStatusSource, TailscaleVerifier, STATUS_CACHE_TTL,
};
pub use tls::{build_server_tls_config, ServerCertMaterial};
// `TofuVerifier` intentionally NOT re-exported — its `new()` is `pub(crate)`
// so the type is unconstructable from outside `borgd::auth`. External callers
// always go through `build_verifier`.
use verifier::TofuVerifier;
pub use verifier::{PeerCtx, Verifier, VerifierError};

/// Construct the configured `Verifier` for the daemon. Fails-closed when
/// the user selected `Tailscale` but the binary is absent — never silently
/// downgrades to TOFU.
///
/// `tailscale_source` is `None` in production (built-in `TailscaleCliSource`
/// is used); tests inject a stub. This separation keeps the factory pure
/// over its inputs and exercisable without a Tailscale install.
///
/// **Wiring contract.** This is the single entry point for constructing the
/// `Arc<dyn Verifier>` that gets passed to `auth_interceptor`. The verifier
/// handed to `auth_interceptor` MUST come from this factory — calling
/// `TofuVerifier::new` directly bypasses the `security.verifier` config
/// and silently downgrades the operator's security choice.
pub async fn build_verifier(
    kind: VerifierKind,
    db: Arc<Mutex<Database>>,
    tailscale_source: Option<Arc<dyn TailscaleStatusSource>>,
) -> anyhow::Result<Arc<dyn Verifier>> {
    match kind {
        VerifierKind::Tofu => Ok(Arc::new(TofuVerifier::new(db))),
        VerifierKind::Tailscale => {
            let source = tailscale_source.unwrap_or_else(|| Arc::new(TailscaleCliSource));
            // Probe at construction time so missing-binary errors surface
            // at daemon startup instead of on the first paired connection.
            source.probe().map_err(|e| {
                anyhow::anyhow!("security.verifier = tailscale but tailscale unavailable: {e}")
            })?;
            let inner = TofuVerifier::new(db);
            Ok(Arc::new(TailscaleVerifier::new(inner, source)))
        }
    }
}
