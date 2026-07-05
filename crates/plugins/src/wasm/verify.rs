//! Package verification, installation, and local HMAC sealing.
//!
//! Install gauntlet order (manifest parse → SHA-256 pin → Ed25519 signature
//! → static WASM walk → HMAC seal) lives in [`verify_package`]; post-install
//! integrity checks live in [`verify_installed_hmac`] and [`legacy_hmac`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::validate::{validate_manifest, validate_plugin_id, validate_wasm_module};
use super::{StaticValidation, WasmPluginManifest, SECRETS_DIR_MODE};

type HmacSha256 = Hmac<Sha256>;

/// Successful package verification output.
#[derive(Debug, Clone)]
pub struct VerifiedWasmPackage {
    /// Parsed manifest.
    pub manifest: WasmPluginManifest,
    /// Canonical manifest bytes used for signature verification.
    pub canonical_manifest: Vec<u8>,
    /// WASM bytes.
    pub wasm: Vec<u8>,
    /// Local HMAC seal for installed state.
    pub local_hmac: String,
    /// Static validation details.
    pub static_validation: StaticValidation,
}

/// Validate and verify a plugin package before installation.
///
/// Gauntlet order matters: manifest parse → SHA-256 pin → Ed25519 signature
/// → static WASM walk → HMAC seal. Each gate's failure mode is "refuse
/// install" with the most specific diagnostic; do not reorder.
pub fn verify_package(
    manifest_json: &[u8],
    signature_b64: &str,
    wasm: &[u8],
    trusted_publishers: &[String],
    local_hmac_key: &[u8],
) -> Result<VerifiedWasmPackage> {
    if local_hmac_key.is_empty() {
        bail!("local HMAC key is required");
    }
    let manifest: WasmPluginManifest =
        serde_json::from_slice(manifest_json).context("invalid plugin manifest JSON")?;
    validate_manifest(&manifest)?;
    verify_wasm_hash(&manifest, wasm)?;
    verify_publisher_signature(&manifest, signature_b64, trusted_publishers)?;
    let static_validation = validate_wasm_module(&manifest, wasm)?;
    let canonical_manifest = canonical_manifest_bytes(&manifest)?;
    let local_hmac = compute_local_hmac(local_hmac_key, &canonical_manifest, wasm)?;
    Ok(VerifiedWasmPackage {
        manifest,
        canonical_manifest,
        wasm: wasm.to_vec(),
        local_hmac,
        static_validation,
    })
}

/// Install a verified package under `data_dir/wasm_plugins/<safe id>/`.
pub fn install_verified_package(
    data_dir: &Path,
    package: &VerifiedWasmPackage,
) -> Result<InstalledWasmPackage> {
    let safe_id = safe_plugin_dir_name(&package.manifest.id)?;
    let dir = data_dir.join("wasm_plugins").join(safe_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    restrict_install_dir_permissions(&dir)?;
    let manifest_path = dir.join("manifest.json");
    let wasm_path = dir.join("plugin.wasm");
    std::fs::write(&manifest_path, &package.canonical_manifest)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    std::fs::write(&wasm_path, &package.wasm)
        .with_context(|| format!("failed to write {}", wasm_path.display()))?;
    Ok(InstalledWasmPackage {
        manifest_path,
        wasm_path,
        local_hmac: package.local_hmac.clone(),
    })
}

/// Paths and integrity seal for an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledWasmPackage {
    /// Installed manifest path.
    pub manifest_path: PathBuf,
    /// Installed WASM path.
    pub wasm_path: PathBuf,
    /// HMAC over canonical manifest and WASM bytes.
    pub local_hmac: String,
}

/// Verify an installed plugin's local HMAC before execution.
pub fn verify_installed_hmac(
    local_hmac_key: &[u8],
    manifest_bytes: &[u8],
    wasm: &[u8],
    expected_hmac: &str,
) -> Result<()> {
    // Mirror the bail in `verify_package` — without a key, both sides would
    // hash to the same constant and any tampering would still "verify". A
    // misconfigured boot (e.g. broken `derive_hmac_key`) must fail loudly here
    // rather than silently degrade to no integrity check.
    if local_hmac_key.is_empty() {
        bail!("local HMAC key is required");
    }
    let expected = hex_decode(expected_hmac)?;
    let actual_hex = compute_local_hmac(local_hmac_key, manifest_bytes, wasm)?;
    let actual = hex_decode(&actual_hex)?;
    if constant_time_eq(&actual, &expected) {
        Ok(())
    } else {
        bail!("installed WASM plugin integrity check failed")
    }
}

/// Compute lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Return canonical manifest bytes used for signing and local sealing.
pub fn canonical_manifest_bytes(manifest: &WasmPluginManifest) -> Result<Vec<u8>> {
    serde_json::to_vec(manifest).context("failed to canonicalize manifest")
}

fn verify_wasm_hash(manifest: &WasmPluginManifest, wasm: &[u8]) -> Result<()> {
    let actual = sha256_hex(wasm);
    if actual != manifest.wasm_sha256 {
        bail!("WASM hash mismatch: manifest does not match module bytes");
    }
    Ok(())
}

fn verify_publisher_signature(
    manifest: &WasmPluginManifest,
    signature_b64: &str,
    trusted_publishers: &[String],
) -> Result<()> {
    if !trusted_publishers
        .iter()
        .any(|k| k == &manifest.publisher.public_key)
    {
        bail!("publisher key is not trusted");
    }
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(&manifest.publisher.public_key)
        .context("publisher public key is not valid base64")?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow!("publisher public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array)?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .context("signature is not valid base64")?;
    let signature = Signature::from_slice(&sig_bytes)?;
    let canonical = canonical_manifest_bytes(manifest)?;
    verifying_key.verify_strict(&canonical, &signature)?;
    Ok(())
}

/// Recompute the current-scheme local HMAC seal. Public so cortex can rewrite
/// the row after auto-resealing a legacy install. The result is what
/// `verify_installed_hmac` will check on the next execution.
pub fn compute_local_hmac_current(
    key: &[u8],
    manifest_bytes: &[u8],
    wasm: &[u8],
) -> Result<String> {
    compute_local_hmac(key, manifest_bytes, wasm)
}

pub(super) fn compute_local_hmac(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid HMAC key length"))?;
    // Length-prefix framing so the two segments can't be ambiguously concatenated.
    // A NUL separator alone would let a manifest containing the right bytes shift
    // material across the boundary while keeping the same MAC; explicit u64-be
    // prefixes make that class of substitution impossible.
    mac.update(&(manifest_bytes.len() as u64).to_be_bytes());
    mac.update(manifest_bytes);
    mac.update(&(wasm.len() as u64).to_be_bytes());
    mac.update(wasm);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Legacy (pre-current) HMAC sealing — kept verification-only so installs
/// sealed by an older borg can be auto-resealed under the current scheme
/// without forcing the operator to reinstall every plugin.
///
/// Two differences from `compute_local_hmac`:
///   1. The derived-key domain on the cortex side was
///      `b"borg-wasm-plugin-local-hmac"` (no `-v2` suffix). Cortex passes that
///      legacy key when it falls back through this verifier.
///   2. Framing used a single NUL separator between manifest and wasm bytes —
///      no length prefix. The length-prefixed scheme is more robust against
///      framing-substitution mistakes if the manifest format ever changes,
///      but the legacy framing has no known exploit *without* the per-install
///      KEK-derived key. We accept a v1 MAC as a trust signal at parity with
///      a v2 MAC because forging either one requires the same secret. The
///      Ed25519 publisher signature is the remote-trust anchor and is not
///      touched by this path — the local HMAC is strictly a post-install
///      tamper signal. If a v1-class collision is ever discovered, this
///      module must be deleted (not loosened), forcing reinstall.
pub mod legacy_hmac {
    use super::{anyhow, bail, constant_time_eq, hex_decode, hex_encode, HmacSha256, Result};
    use hmac::Mac;

    /// Verify `expected_hmac` against the pre-current ("v1") sealing scheme.
    /// Callers MUST treat success as a signal to reseal under the current
    /// scheme (see `cortex::wasm_plugins`), not as a permanent integrity state.
    pub fn verify(
        legacy_key: &[u8],
        manifest_bytes: &[u8],
        wasm: &[u8],
        expected_hmac: &str,
    ) -> Result<()> {
        if legacy_key.is_empty() {
            bail!("legacy HMAC key is required");
        }
        let expected = hex_decode(expected_hmac)?;
        let actual_hex = compute(legacy_key, manifest_bytes, wasm)?;
        let actual = hex_decode(&actual_hex)?;
        if constant_time_eq(&actual, &expected) {
            Ok(())
        } else {
            bail!("installed WASM plugin legacy integrity check failed")
        }
    }

    /// Compute the legacy ("v1") HMAC over `manifest_bytes + wasm` under the
    /// legacy framing (NUL-separated, no length prefix). Exposed for tests
    /// that need to construct a legacy-sealed install to drive the auto-reseal
    /// path in cortex; production never seals under this framing.
    pub fn compute(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
        let mut mac =
            HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid HMAC key length"))?;
        mac.update(manifest_bytes);
        mac.update(b"\0");
        mac.update(wasm);
        Ok(hex_encode(&mac.finalize().into_bytes()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn legacy_verify_round_trip() {
            // Real failure mode: a future framing tweak that silently breaks
            // verification of in-the-wild v1 seals — every existing install
            // would be force-quarantined on upgrade. Round-trip a known input
            // to lock the framing down.
            let key = b"legacy-key-bytes-2026";
            let manifest = b"{\"id\":\"dev/echo\"}";
            let wasm = b"\0asm\x01\x00\x00\x00";
            let mac = compute(key, manifest, wasm).expect("compute legacy");
            verify(key, manifest, wasm, &mac).expect("verify legacy");

            // Tamper either side, expect failure.
            let mut tampered = wasm.to_vec();
            *tampered.last_mut().unwrap() ^= 0xFF;
            assert!(verify(key, manifest, &tampered, &mac).is_err());

            let mut tampered_manifest = manifest.to_vec();
            tampered_manifest[0] ^= 0xFF;
            assert!(verify(key, &tampered_manifest, wasm, &mac).is_err());
        }

        #[test]
        fn legacy_verify_empty_key_rejected() {
            // A misconfigured boot must fail loudly here rather than silently
            // accept everything via a zero-length key — mirrors the v2 path.
            let err = verify(b"", b"manifest", b"wasm", &"0".repeat(64))
                .expect_err("empty key must be rejected");
            assert!(
                err.to_string().contains("legacy HMAC key"),
                "error must point at the key, got: {err}",
            );
        }
    }
}

pub(super) fn safe_plugin_dir_name(id: &str) -> Result<String> {
    validate_plugin_id(id)?;
    Ok(sha256_hex(id.as_bytes()))
}

/// Restrict the per-plugin install directory to owner-only on Unix.
///
/// Matches the `~/.borg/secrets/` posture so an attacker with read access to
/// `~/.borg/` (e.g. via a misconfigured backup or a sibling tool running as a
/// less-privileged user) cannot enumerate installed plugin source bytes —
/// which is the moral equivalent of cleartext capability tokens once paired
/// with a quarantine bypass.
fn restrict_install_dir_permissions(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(SECRETS_DIR_MODE))
            .with_context(|| format!("failed to chmod {} to 0700", dir.display()))?;
    }
    let _ = dir;
    Ok(())
}

pub(super) fn is_hex_64(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        bail!("hex string has odd length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).context("invalid hex"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}
