//! Package verification, installation, and local HMAC sealing.
//!
//! Install gauntlet order (manifest parse → SHA-256 pin → Ed25519 signature
//! → static WASM walk → HMAC seal) lives in [`verify_package`]; post-install
//! integrity checks live in [`verify_installed_hmac`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, KeyInit, Mac};
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

/// Compute the local HMAC seal for an installed plugin — what
/// `verify_installed_hmac` checks on the next execution.
pub fn compute_local_hmac_current(
    key: &[u8],
    manifest_bytes: &[u8],
    wasm: &[u8],
) -> Result<String> {
    compute_local_hmac(key, manifest_bytes, wasm)
}

fn compute_local_hmac(key: &[u8], manifest_bytes: &[u8], wasm: &[u8]) -> Result<String> {
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

fn safe_plugin_dir_name(id: &str) -> Result<String> {
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
    // Non-unix: no equivalent permission model, so installs are NOT
    // mode-restricted on Windows — the ACL work lives in the installer, not here.
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
    if !hex.len().is_multiple_of(2) {
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

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
    use wasm_encoder::Instruction;

    use super::super::test_util::{abi_wasm_with_call_body, manifest_for, minimal_wasm};
    use super::*;

    #[test]
    fn package_verification_requires_trusted_signature() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());
        let verified = verify_package(
            &canonical,
            &sig,
            &wasm,
            &[manifest.publisher.public_key.clone()],
            b"local-key",
        )
        .expect("trusted signed package verifies");
        assert_eq!(verified.manifest.id, "dev/echo");

        let err = verify_package(&canonical, &sig, &wasm, &[], b"local-key")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not trusted"));
    }

    #[test]
    fn package_verification_requires_local_hmac_key() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());

        let err = verify_package(
            &canonical,
            &sig,
            &wasm,
            &[manifest.publisher.public_key.clone()],
            b"",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("local HMAC key is required"));
    }

    #[test]
    fn verify_installed_hmac_rejects_empty_key() {
        // Mirrors `package_verification_requires_local_hmac_key`. Without a key
        // the HMAC degenerates to a publicly recomputable value, so both sides
        // would "verify" after tampering. The empty-key path must bail before
        // we ever reach the constant-time compare.
        let err = verify_installed_hmac(b"", b"manifest", b"wasm", "0".repeat(64).as_str())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("local HMAC key is required"),
            "expected empty-key bail, got: {err}"
        );
    }

    #[test]
    fn safe_plugin_dir_name_cannot_collide_on_slashes() {
        let nested = safe_plugin_dir_name("dev/echo").unwrap();
        let literal = safe_plugin_dir_name("dev__echo").unwrap();
        assert_ne!(nested, literal);
        assert!(nested.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn verify_package_rejects_wasm_hash_mismatch() {
        let wasm = minimal_wasm();
        let other_wasm = abi_wasm_with_call_body(|call| {
            call.instruction(&Instruction::I64Const(0));
            call.instruction(&Instruction::End);
        });
        assert_ne!(sha256_hex(&wasm), sha256_hex(&other_wasm));

        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());

        let err = verify_package(
            &canonical,
            &sig,
            &other_wasm,
            &[manifest.publisher.public_key.clone()],
            b"local-key",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("WASM hash mismatch"),
            "expected hash-mismatch error, got: {err}"
        );
    }

    #[test]
    fn verify_package_rejects_post_signing_manifest_tamper() {
        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let trusted = vec![manifest.publisher.public_key.clone()];

        let canonical_original = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical_original).to_bytes());

        // Untampered: baseline must verify so the negative below can only fail
        // on the signature path.
        verify_package(&canonical_original, &sig, &wasm, &trusted, b"local-key")
            .expect("baseline signed package verifies");

        let mut tampered = manifest.clone();
        tampered.version = "9.9.9".to_string();
        let canonical_tampered = canonical_manifest_bytes(&tampered).unwrap();

        let err = verify_package(&canonical_tampered, &sig, &wasm, &trusted, b"local-key")
            .unwrap_err()
            .to_string();
        // Must be the signature-verification failure specifically — not an
        // earlier gate firing for an unrelated reason.
        let lower = err.to_lowercase();
        assert!(
            lower.contains("signature") || lower.contains("verif"),
            "expected signature verification failure, got: {err}"
        );
        assert!(
            !err.contains("WASM hash mismatch")
                && !err.contains("invalid plugin manifest JSON")
                && !err.contains("not trusted"),
            "signature path not exercised, got an earlier-gate error: {err}"
        );
    }

    #[test]
    fn installed_hmac_detects_single_byte_tamper() {
        let wasm = minimal_wasm();
        let manifest_bytes = canonical_manifest_bytes(&manifest_for(&wasm)).unwrap();
        let key = b"hmac-key-v1";
        let hmac = compute_local_hmac(key, &manifest_bytes, &wasm).unwrap();

        verify_installed_hmac(key, &manifest_bytes, &wasm, &hmac)
            .expect("untampered round-trip verifies");

        let mut wasm_tamper = wasm.clone();
        let last = wasm_tamper.len() - 1;
        wasm_tamper[last] ^= 0x01;
        let err = verify_installed_hmac(key, &manifest_bytes, &wasm_tamper, &hmac)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("integrity check failed"),
            "wasm byte flip must trip integrity check, got: {err}"
        );

        let mut manifest_tamper = manifest_bytes.clone();
        manifest_tamper[0] ^= 0x01;
        let err = verify_installed_hmac(key, &manifest_tamper, &wasm, &hmac)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("integrity check failed"),
            "manifest byte flip must trip integrity check, got: {err}"
        );
    }

    #[test]
    fn local_hmac_length_prefix_resists_boundary_shifting() {
        // With NUL-separator framing, shifting bytes across the manifest/wasm
        // boundary while keeping concatenation byte-identical would yield the
        // same MAC. Length-prefix framing must produce a different MAC if the
        // split moves, even when the concatenated bytes happen to match.
        let key = b"hmac-key-v1";

        // Build two (manifest_bytes, wasm) pairs that concatenate to the same
        // material but split at different lengths. NUL-separator framing
        // (`manifest || 0x00 || wasm`) would only be ambiguous when the
        // manifest itself contains a NUL — we just demonstrate the more general
        // property: differently-split inputs MUST yield distinct MACs.
        let mac_a = compute_local_hmac(key, b"AAAAA", b"BBBBBBBBBB").unwrap();
        let mac_b = compute_local_hmac(key, b"AAAAABBBBB", b"BBBBB").unwrap();
        assert_ne!(
            mac_a, mac_b,
            "moving the manifest/wasm boundary must change the HMAC"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_verified_package_sets_0700_on_unix() {
        // The install directory holds verified plugin source bytes that are
        // moral siblings to credentials once paired with a quarantine bypass —
        // we want the same 0700 posture as `~/.borg/secrets/`. Asserting on
        // the post-condition (`mode() & 0o777 == 0o700`) means a future change
        // that drops the chmod call fails this test for a real reason.
        use std::os::unix::fs::PermissionsExt;

        let wasm = minimal_wasm();
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let mut manifest = manifest_for(&wasm);
        manifest.publisher.public_key = STANDARD.encode(signing.verifying_key().to_bytes());
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        let sig = STANDARD.encode(signing.sign(&canonical).to_bytes());
        let trusted = vec![manifest.publisher.public_key.clone()];
        let package =
            verify_package(&canonical, &sig, &wasm, &trusted, b"local-key").expect("verify");

        let tmp = tempfile::tempdir().expect("tempdir");
        let installed = install_verified_package(tmp.path(), &package).expect("install");

        let dir = installed.manifest_path.parent().expect("manifest parent");
        let mode = std::fs::metadata(dir)
            .expect("stat install dir")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            SECRETS_DIR_MODE,
            "install dir {} has mode {:o}, expected 0o700",
            dir.display(),
            mode & 0o777
        );
    }

    #[test]
    fn compute_local_hmac_current_produces_verifiable_seal() {
        // The public wrapper is what callers persist as the seal, so it must
        // emit the real 64-hex MAC — a stubbed value would either fail hex
        // decoding or never match the recomputed MAC at verification time.
        let key = b"hmac-key-v1";
        let seal = compute_local_hmac_current(key, b"manifest", b"wasm").unwrap();
        assert!(
            is_hex_64(&seal),
            "seal must be a 64-char lowercase hex MAC, got: {seal:?}"
        );
        verify_installed_hmac(key, b"manifest", b"wasm", &seal)
            .expect("seal from compute_local_hmac_current must verify");
        assert!(
            verify_installed_hmac(key, b"manifest", b"tampered", &seal).is_err(),
            "seal must bind the actual input bytes"
        );
    }

    #[test]
    fn is_hex_64_requires_exact_length_and_lowercase_hex() {
        let cases: &[(String, bool, &str)] = &[
            ("a".repeat(64), true, "64 lowercase hex digits"),
            (
                "0123456789abcdef".repeat(4),
                true,
                "full lowercase alphabet",
            ),
            ("a".repeat(63), false, "one short of 64"),
            ("a".repeat(65), false, "one over 64"),
            ("A".repeat(64), false, "uppercase hex digits"),
            ("z".repeat(64), false, "non-hex characters"),
            (String::new(), false, "empty string"),
        ];
        for (input, expected, why) in cases {
            assert_eq!(
                is_hex_64(input),
                *expected,
                "[{why}] is_hex_64({input:?}) must be {expected}"
            );
        }
    }
}
