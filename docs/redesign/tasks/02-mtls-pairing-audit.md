# Task 02 — Seamless mTLS + PAKE pairing + audit chain

> Read first: [`auth-and-onboarding.md`](../auth-and-onboarding.md), [`threat-model.md`](../threat-model.md) (A2, A6, A7, A11).
> Depends on: Task 01 (daemon + gRPC server exists).

## Goal

Make the daemon's gRPC surface require client mTLS, with K8s-style seamless certificate lifecycle: zero CA paperwork, OS-keychain-stored keys, silent renewal. Pair new clients with a one-time PAKE code (no shared bearer tokens). Hash-chain every security-relevant event in an append-only audit log so tampering is detectable.

## Scope

### Internal CA (daemon-managed)

- On first start, daemon generates a self-signed root CA (Ed25519, 5y validity) and a server cert (90d, auto-renewed at 14d remaining).
- CA private key wrapped by an OS-keychain-held KEK:
  - macOS Keychain (Secure Enclave when available)
  - Linux Secret Service / `tpm2-tss` when present
  - Windows DPAPI / TPM
- New `internal_ca_certs` table tracks issued/renewed/revoked certs (cert PEM, fingerprint, subject, not_before, not_after, status).
- `crates/borg-pki` (new) owns CA ops: issue, renew, revoke, list. Used by daemon at startup + on pairing + on cert-renewal tick.

### Client cert lifecycle

- Each paired device gets a client cert (Ed25519, 90d, CN = device name, SAN includes device fingerprint).
- Daemon renews silently when a client connects with a cert ≤14d from expiry: sign a fresh cert, hand back via gRPC `Admin.RenewCert`, client re-stores in OS keychain.
- Lost device: `borg revoke <fingerprint>` writes to `revoked_fingerprints` (CRL-equivalent). Daemon checks on every handshake.

### PAKE pairing

- SPAKE2 (or CPace) over short-lived pairing code shown by daemon: `borg pair` prints a 6-word phrase + QR.
- New client runs `borg pair <code>`; PAKE establishes a one-time encrypted channel; daemon issues client cert; client stores cert + key in OS keychain.
- Pairing codes single-use, 5-min TTL, rate-limited (5 attempts / 10 min / source).
- Schemas: `pairing_codes`, `paired_devices`.

### Verifier trait

```rust
trait Verifier {
    fn verify(&self, peer_cert: &Certificate, conn: &ConnContext) -> Result<PrincipalId>;
}
```

v1 implementations:

- `TofuVerifier` — first connection from a fingerprint is recorded; subsequent connections must match. Default.
- `TailscaleVerifier` — opt-in. Cross-checks peer IP against `tailscale status` and binds the cert to the Tailscale node identity. Off by default; enabled via `[security] verifier = "tailscale"`.

External-CA verifiers (corporate PKI, SCEP, EST, ACME) are explicitly v3-deferred. Do **not** add stubs.

### Hash-chained audit log

- `audit_log_chain` table: `(seq INTEGER PK, prev_hash BLOB, event_type TEXT, payload_json TEXT, ts INTEGER, hash BLOB)`. `hash = blake3(prev_hash || event_type || payload_json || ts)`.
- Events on by default in v1: pairing requested/approved/denied, cert issued/renewed/revoked, posture transition, budget hit, sandbox denial, gRPC auth failure, settings change, daemon start/stop.
- `borg audit verify` recomputes the chain; reports `Ok | Tampered(seq)`. Task 05 extends the outcome enum with `PurgedTombstone` for retention work.
- Replay-safe — verification is pure (no clock or network).

### Connection enforcement

- After this task, `--insecure-uds` from Task 01 goes away. UDS still allowed but requires a client cert.
- TLS 1.3 only. No bearer tokens, no shared secrets in headers.
- Mutual auth on every gRPC connection; principal extracted from cert SAN, attached to gRPC context, threaded through to agent loop as `principal_id` (still always the single v1 principal, but the plumbing is in place for v3).

## Out of scope

- SCIM/LDAP, Derived PIV credentials, FIPS 140-3, M-of-N quorum, Shamir KEK escrow, external-CA verifiers — all v3.
- Mobile pairing UX (v2).

## Done when

- Fresh daemon start auto-creates CA + server cert; CA key wrapped by OS keychain on each platform.
- `borg pair` flow works end-to-end: code shown, second client pairs, cert issued, future connections succeed without re-pairing.
- Cert ≤14d from expiry triggers silent renewal during a normal client connection — observable via `audit_log_chain` event.
- `borg revoke <fp>` blocks future connections from that fingerprint.
- `borg audit verify` returns Ok on a clean chain; tampering with a row makes it return `Tampered(seq)` pointing at the row.
- All gRPC connections fail without a valid client cert.
- Tests cover: pairing happy path, expired pairing code, revoked cert handshake refusal, silent renewal, audit chain tamper detection, TOFU first-vs-second connection.

## Risks to watch

- **OS-keychain availability.** Linux without Secret Service / TPM needs a documented fallback (file-encrypted-by-passphrase prompted at daemon start). Don't silently degrade.
- **Pairing-code phishing.** Codes are short-lived and rate-limited; do not log them. Show only on the daemon's controlling TTY (or via `borg pair --show` with a confirm).
- **Audit chain write amplification.** Batch within a turn but never across turns — replay must be deterministic.
