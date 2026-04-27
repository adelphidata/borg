# Authentication & Device Onboarding — v1

Status: design, in active rework. Companion to [`architecture-redesign.md`](architecture-redesign.md).

## Goal

Define how Borg authenticates clients (TUI, CLI, third-party API consumers, channel adapters) and how new devices are onboarded. **The mTLS experience is K8s-shaped: seamless, no manual cert ceremony, no PEM files for the user to manage.** The daemon runs its own internal CA; pairing is a one-tap PAKE flow; keys live in the OS keychain / Secure Enclave / TPM; renewal is silent.

This is the v1 single-mode design. Sentry-mode (agency PKI verifiers, FIPS, SCIM/LDAP, Derived PIV, M-of-N quorum) is v3, scoped at the bottom of this doc.

## Non-goals

- **Replacing channel auth.** Channels (Telegram/Slack/Discord/Teams/iMessage/etc.) keep their existing webhook-signature trust. mTLS does not retrofit onto third-party platforms.
- **Building an internal user database.** No passwords, no MFA prompts, no Borg-managed accounts. Identity in v1 = devices.
- **Cross-daemon delegated trust.** A daemon never authenticates to another daemon as a delegated principal. Cross-daemon operations always run as the human's identity, on each side independently.
- **External CA verifiers in v1.** `StaticX509Verifier` / `VaultPkiVerifier` / `StepCaVerifier` are v3.
- **Show-twice recovery secrets.** Recovery phrases display once; if lost, regenerate via local root path.

## Locked decisions (v1)

| # | Decision |
|---|---|
| 1 | First-party client auth = mTLS with daemon-issued per-device certs. Daemon is its own internal CA. Pairing via PAKE. |
| 2 | Trust model = Tailscale-or-TOFU. (Tailscale verifier or TOFU verifier on a per-listener basis.) |
| 3 | Per-listener auth backends — one daemon binds N listeners, each with its own backend. |
| 4 | TOFU is two-sided: client pins server fingerprint; daemon pins device public key. |
| 5 | Device-as-principal. Same scope/policy engine downstream. |
| 6 | Scope = tier (`Read` / `Write` / `Execute` / `Privileged`) + `allow`/`deny` overrides + per-listener ceiling. |
| 7 | Revocation = local-registry fast-path + live-connection drop on revoke (within 5s). |
| 8 | Recovery = L1 host-root reset (always) + L2 show-once 24-word phrase. |
| 9 | Pairing = SPAKE2/CPace PAKE + 8-char base32 code + already-paired admin can mint via TUI. |
| 10 | Key storage = OS keychain / Secure Enclave / TPM (best available); encrypted-file as last-resort fallback. |
| 11 | Cert/key rotation = silent renewal (14-day-to-expiry threshold). User never sees PEM files. |
| 12 | Multi-daemon support via named profiles. Last-paired wins as default; TUI toast surfaces the change. |
| 13 | Channel senders get a per-channel default tier with a hard ceiling. Channels can never grant `Privileged`. |
| 14 | Fresh-install default = one Enter-press → loopback only. Never `0.0.0.0`-by-default. |
| 15 | Auth state lives in the existing SQLite DB. Bootstrap exception: `BORG_HOME` env var only. No `auth.toml`. |
| 16 | TUI vocabulary reuses existing slash commands. Net new: `/devices`. Listener affordances under `/settings → Network`. |

## Architecture

### Layers

```
┌──────────────────────────────────────────────────────────────────┐
│ Listener (per-bind: loopback / tailnet / public-tofu)            │
│   • TLS termination (rustls + aws-lc-rs)                         │
│   • PAKE pairing endpoint                                        │
│   • Per-listener: auth_backend, max_tier ceiling                 │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│ Verifier trait (auth backend)                                    │
│   TofuVerifier / TailscaleVerifier                               │
│   → produces VerifiedPrincipal                                    │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│ Principal                                                        │
│   Device { id, fingerprint, owner_label, paired_at }             │
│   ChannelSender { channel, sender_id, approved_at }              │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│ Scope resolver                                                   │
│   tier + allow/deny + listener ceiling                           │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│ Authorization gate                                               │
│   per-request capability check, hash-chained audit emit,         │
│   live-connection invalidation on principal status change        │
└──────────────────────────────────────────────────────────────────┘
```

### Listener config (DB-only)

Listeners are rows in a `listeners` table. The `SettingsResolver` exposes a typed view; the TUI's `/settings → Network` is the editing UI; the CLI mirror is `borg listener {add,remove,list,update}`.

```sql
CREATE TABLE listeners (
  id           TEXT PRIMARY KEY,        -- "loopback", "tailnet", "public_tofu"
  bind         TEXT NOT NULL,           -- "127.0.0.1:18789", "tailnet:18789", ...
  auth_backend TEXT NOT NULL,           -- "none" | "tailscale" | "tofu"
  max_tier     TEXT NOT NULL,           -- "read" | "write" | "execute" | "privileged"
  fail_closed  INTEGER NOT NULL DEFAULT 1,
  enabled      INTEGER NOT NULL DEFAULT 1,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
```

Validation in `SettingsResolver::set_listener`: refuses public binds with `auth_backend = none | tailscale`. Loopback `auth_backend = none` is allowed.

#### Bootstrap exception
The daemon needs *something* to read before it can open the DB. Just one item: **`BORG_HOME` env var** (defaults to `~/.borg/`). Locates the SQLite file. Same as today. Everything else — listeners, scope, recovery hashes — lives in SQLite.

#### Client-side profiles
The `borg` CLI/TUI client stores its daemon profiles in SQLite — `~/.borg/clients.db` with `(profile, host, server_fingerprint, last_used_at)`. Bootstrap input on the client side is `BORG_HOME` and the optional `--profile <name>` flag.

### The internal CA

The daemon runs its own internal CA. There is no operator ceremony to set this up — first daemon start generates the CA root + server keypair into the OS keychain. The user never sees PEM files.

Rotation: the CA root has a 5-year lifetime; server certs are 90 days; device certs are 90 days. Renewal is silent at the 14-day-to-expiry threshold via an internal `/internal/renew` endpoint that accepts a CSR signed by the device's current valid key.

This is the K8s analog: kubeadm generates the cluster CA at init, kubelet bootstraps node certs against it, cert-manager handles rotation. No human reads a PEM file. Borg copies the pattern.

### The `Verifier` trait

```rust
pub trait Verifier: Send + Sync {
    fn verify_client(
        &self,
        presented: &ClientCert,
        ctx: &HandshakeCtx,
    ) -> Result<VerifiedPrincipal, VerifyError>;

    fn check_revocation(
        &self,
        principal: &VerifiedPrincipal,
    ) -> Result<RevocationStatus, VerifyError>;
}
```

v1 compiled-in implementations:

| Verifier | Listener types | Use case |
|---|---|---|
| `TofuVerifier` | `tofu`, `public_tofu` | The default. Pairing-by-PAKE, daemon's internal CA. |
| `TailscaleVerifier` | `tailnet` | Tailnet identity headers (`tailscale-user-login`) verified via `tailscale whois`. |

External-CA verifiers (`StaticX509Verifier`, `VaultPkiVerifier`, `StepCaVerifier`) are v3 — wired up only when an agency-PKI integration is in scope.

### Principal model

```rust
pub enum Principal {
    Device {
        id: DeviceId,
        fingerprint: PublicKeyFingerprint,
        owner_label: Option<String>,        // free-form, e.g. "alice-iphone"
        paired_at: DateTime<Utc>,
    },
    ChannelSender {
        channel: ChannelId,
        sender_id: String,
        approved_at: DateTime<Utc>,
        approved_by: PrincipalRef,
    },
}
```

Every event in the chain (sessions, messages, audit, evolution) carries a `principal_id` column from v1, even though there's only ever one human principal in v1. v3 multi-tenancy is additive against this column, not migrative.

`Principal::User { subject, groups }` and `Principal::Service { name }` variants are reserved for v3 (agency PKI / SCIM) and not constructed in v1.

### Scope

```rust
pub struct Scope {
    pub tier: Tier,                        // Read / Write / Execute / Privileged
    pub allow: Vec<Capability>,            // additive
    pub deny: Vec<Capability>,             // subtractive, wins
    pub channels: Vec<ChannelId>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

Effective scope at request time = `(principal.scope) ∩ (listener.max_tier ceiling)`. A privileged device on the public-tofu listener is capped to whatever the listener says, regardless of what the device record claims.

(Per-workspace scoping and `min_device_attestation` floor are v3 additions tied to multi-tenancy.)

## Default (and only) v1 mode — UX-first

### Goals
- Zero auth-admin burden. The user pairs devices and never thinks about CAs, CRLs, or PEM files.
- Secure-by-default-when-possible. Tailscale offered if available. TOFU pairing only when explicitly enabled.
- No internet-facing default. The daemon never binds `0.0.0.0` without an explicit toggle.

### First-run flow (one Enter-press)

The user makes **zero decisions** on day 1. The TUI shows:

```
Welcome to Borg.

  [Enter]  start with safe defaults
  [a]      advanced setup
```

Hitting Enter does all of this silently:

- Generates the daemon's internal CA root + server keypair in the OS keychain / Secure Enclave / TPM (best available).
- Inserts a `loopback` listener (`127.0.0.1:18789`, no auth, `max_tier = privileged`).
- Generates a 24-word BIP39 recovery phrase, shows it **once** in a follow-up screen, stores only the hash. The user can hit `[skip]` (logged warning, doctor surfaces it later).
- Inserts the local device as the first principal: tier=`Privileged`, recovery=host-root.

No tailnet detection, no `0.0.0.0` offer, no listener wizard. Tailscale and public-TOFU listeners are *later* opt-ins.

`[a] advanced setup` opens the same wizard but exposes listener choice, recovery-phrase ceremony, and key-storage tier display.

### Progressive disclosure

Nothing else surfaces until the user goes looking:

| Want to… | How it surfaces |
|---|---|
| Talk to Borg from a phone | `/channel add telegram` (existing flow, unchanged) |
| Pair a second device (laptop, mini) | `/devices` → "Pair new device" |
| Reach Borg from another machine on Tailscale | `/settings → Network` → "Add tailnet listener" (one toggle; daemon auto-detects Tailscale presence) |
| Expose Borg over the public internet | `/settings → Network` → "Add public TOFU listener" (gated behind a confirmation that explains the risk) |
| Re-show or rotate the recovery phrase | `/settings → Security` → "Recovery phrase" (rotate only — show-once is the security boundary) |
| Switch active daemon profile | `/profile <name>` or last-paired-wins toast |

### TUI commands

| Command | Purpose |
|---|---|
| `/devices` | List, pair, revoke, retier paired devices. New, follows `/channel` pattern. |
| `/channel` | Unchanged. Channel sender approval prompts already exist. |
| `/settings → Network` | Listener editing (add tailnet, add public TOFU, remove, view binds). |
| `/settings → Security` | Recovery phrase, key-storage status. |
| `/status` | Adds an "Auth" row — current principal, listener it came in on, scope tier. |
| `/profile <name>` | Switch active daemon profile (client-side). |

Every TUI action has a `borg <cmd>` CLI mirror (`borg device pair`, `borg device list`, `borg device revoke <id>`, `borg listener add tailnet`). CLI parity is a hard rule.

### Pairing UX

`/devices` → "Pair new device" generates a 6-word BIP-39 PAKE phrase prefixed with the channel id (`tg/voyage-kitten-orbit-marble-cinder-pluto`) and a QR (`borg-pair://host:port/?code=tg/voyage-kitten-orbit-marble-cinder-pluto&fp=sha256:...`). Code expires in 5 minutes; rate-limited (3 wrong attempts → invalidate). Words are sampled from the BIP-39 English wordlist via `OsRng` (~66 bits entropy); hyphen and space separators are both accepted on input.

### Pairing handshake

1. User: `borg pair --host mac-mini.tail-1234.ts.net` (or scan QR from a future mobile app).
2. Client and daemon run SPAKE2/CPace PAKE handshake using the 6-word phrase as the shared secret. A network attacker without the phrase cannot recover the session key.
3. Within the PAKE-protected channel:
   - Client generates an Ed25519 (or P-256) keypair. Private key goes to OS keychain / Secure Enclave / TPM (best available, reported back).
   - Client sends a self-signed cert + a device-identity blob (suggested name, key-storage tier).
   - Daemon stores `(device_id, fingerprint, key_storage_tier, scope=tier-default, paired_at)` in `paired_devices` table.
   - Daemon returns its own server fingerprint for client-side pinning (also embedded in the QR for belt-and-suspenders verification).
4. Subsequent connections: standard mTLS, both sides verify pinned fingerprints, no PAKE.

### Pairing tier UX

The first device against a fresh daemon automatically becomes `Privileged` (bootstrap admin).

Subsequent pairings prompt on an already-paired admin device:

```
┌─ Pair Device ────────────────────────────────┐
│ A device is requesting to pair:              │
│   Name: alice-iphone                         │
│   Key storage: Secure Enclave (hardware)     │
│   From: 192.168.1.42                         │
│                                              │
│   [ Approve as Operator ]  ← default         │
│   [ Approve as Read-only ]                   │
│   [ Approve as Privileged ]                  │
│   [ Deny ]                                   │
└──────────────────────────────────────────────┘
```

Three buttons. Editable later via `/devices → select → change tier`.

### Multi-daemon (named profiles)

Client-side `~/.borg/clients.db` (SQLite) holds N daemon profiles, each row `(profile, host, server_fingerprint, keychain_ref, last_seen_scope, last_used_at)`. Keys live in OS keychain under namespaced labels (`borg/<profile>/key`). Default daemon = last-paired-wins, with a TUI toast on switch. Active profile shown in the TUI status line: `[mac-mini/operator]`. Switch with `/profile <name>` (TUI) or `borg --profile <name>` (CLI).

Cross-daemon operations are operator-mediated, not delegated: `/forward home → work "<text>"` runs as the user's identity on each side. No daemon impersonates another principal.

### Renewal

Silent. Daemon endpoint `/internal/renew` accepts a CSR signed by the device's current valid key, mints a new 90-day cert with a freshly rotated key, preserves device-id and scope. Triggered automatically at the 14-day-to-expiry threshold. Audit-logged. Surfaces in TUI only on failure or anomaly (renewal from an unexpected IP, key-storage downgrade since last pair).

### Recovery

- **L1 (always):** `borgd recover` invoked locally with root/Administrator wipes the device registry and regenerates the daemon keypair. Audit-logged. Floor-of-last-resort — same trust assumption as "if you have root on the box you own it."
- **L2:** Show-once 24-word phrase shown at first boot. `borg pair --recovery <phrase>` accepts the phrase to bootstrap a new admin device. Single-use; auto-rotates. Phrase usage emits a high-priority audit event AND notifies all currently-paired devices.

### Channel senders

Webhook signature auth on the channel layer (Telegram bot token, Slack signing secret, etc.) gates the transport. The first time a new sender messages the bot, the existing pairing-request flow surfaces a TUI prompt:

```
┌─ Approve Sender ─────────────────────────────┐
│ New sender on Telegram:                      │
│   +1-555-0123 (Alice Smith)                  │
│                                              │
│   [ Approve as Operator ]                    │
│   [ Approve as Chat-only ]  ← channel default│
│   [ Deny ]                                   │
└──────────────────────────────────────────────┘
```

Per-channel default tiers (configurable):

| Channel | DM default | Group/multi-party default | Hard ceiling |
|---|---|---|---|
| iMessage | operator | chat | operator |
| Telegram | operator | chat | operator |
| Signal | operator | chat | operator |
| Slack | chat | chat | operator |
| Discord | chat | chat | chat |
| Teams | chat | chat | operator |
| Twilio SMS | chat | chat | chat |
| Google Chat | chat | chat | operator |

**Channels never grant `Privileged`.** The channel listener's `max_tier` ceiling caps every sender below privileged regardless of approval. Operators who really want shell-over-Telegram must explicitly flip `gateway.telegram.allow_privileged = true`, which surfaces a permanent doctor warning.

### Tier change vs revoke

- **Tier change** (e.g., admin downgrades sender from operator to chat): takes effect on the next request. In-flight turns complete. Audit-logged.
- **Revoke** (admin marks principal `revoked`): live connections drop within ~5s via the live-connection invalidation path. In-flight turns abort.

## Implementation notes

### Crates / files (sketch)

- `crates/cortex/src/auth/mod.rs` — `Principal`, `Scope`, `Capability`, `Tier` types.
- `crates/cortex/src/auth/verifier.rs` — `Verifier` trait + `TofuVerifier` + `TailscaleVerifier`.
- `crates/cortex/src/auth/pake.rs` — SPAKE2/CPace pairing handshake.
- `crates/cortex/src/auth/key_storage.rs` — abstraction over Keychain / Secure Enclave / TPM / encrypted file.
- `crates/cortex/src/auth/internal_ca.rs` — daemon's internal CA: root generation, cert issuance, revocation list.
- `crates/cortex/src/auth/scope_resolver.rs` — listener-ceiling intersection.
- `crates/cortex/src/auth/recovery.rs` — L1 reset, L2 phrase generation/verification.
- `crates/gateway/src/listener.rs` — per-listener TLS termination, dispatch to `Verifier`.
- `crates/interface/src/cmd/device.rs` — `borg device {pair,list,revoke,retier}` CLI; same handler invoked by TUI `/devices`.
- `crates/interface/src/cmd/listener.rs` — `borg listener {add,remove,list,update}` CLI; same handler invoked by `/settings → Network`.
- `crates/interface/src/tui/devices.rs` — `/devices` screen.
- `crates/interface/src/tui/settings/{network,security}.rs` — `/settings` subsections.

### SQLite tables (v1)

- `listeners(id, bind, auth_backend, max_tier, fail_closed, enabled, created_at, updated_at)`
- `paired_devices(id, fingerprint UNIQUE, owner_label, key_storage_tier, scope_json, paired_at, last_seen_at, status, principal_id)`
- `revoked_fingerprints(fingerprint, revoked_at, reason)`
- `pairing_codes(code_hash, expires_at, intent, created_by)`
- `audit_log_chain(seq, prev_hash, hash, event_json, ts, principal_id)` — hash-chained
- `internal_ca_certs(serial, kind, pem, issued_at, expires_at, revoked_at)` — daemon's own CA records
- (client-side, separate `clients.db`) `profiles(name PRIMARY KEY, host, server_fingerprint, keychain_ref, last_used_at)`

`principal_id` columns nullable in v1 (single principal); v3 multi-tenancy populates them per-user.

### Crypto stack

- `rustls` with `aws-lc-rs` cryptographic provider (standard build; FIPS feature is v3).
- `spake2` (or `cpace`) crate for PAKE.
- `tpm2-tss` via `tss-esapi` crate for Linux TPM access.
- `Security.framework` (macOS) / `wincrypt` (Windows) bindings for native key storage.

### Test posture

Per `CLAUDE.md` "no fluff tests":

- **Pairing handshake:** PAKE completes; wrong code fails; replayed code fails; expired code fails; rate-limited after 3 attempts.
- **Live-connection drop:** revoke fires within 5s on active stream; tier downgrade applies on next message but not mid-turn.
- **Listener floor enforcement:** public listener with `auth = none` refuses to start; key-storage downgrade since last pair surfaces in TUI.
- **Channel ceiling:** sender approved as `operator` cannot invoke `tools:run_shell`; explicit override flips ceiling; doctor warning persists.
- **Recovery phrase:** show-once enforced (second display refuses); use rotates phrase; use notifies all paired devices.
- **Multi-daemon profile:** last-paired-wins toast; explicit set-default override.
- **Internal CA renewal:** silent rotation at 14-day threshold; renewed cert pins same device-id; failure surfaces in TUI.

### What NOT to build

- A WASM or subprocess-loaded `Verifier` plugin in v1.
- An internal user database. Identity is devices; v3 adds upstream PKI/IdP.
- Cross-daemon delegated trust.
- Auto-discovery of daemons via mDNS in v1. Tailscale + manual host entry covers the use cases.
- A "convenience" path that re-displays the recovery phrase. Show-once is the security boundary.
- Re-using a fingerprint after revocation. Revoked fingerprints stay in `revoked_fingerprints` permanently.
- Filesystem TOML/YAML auth config alongside the DB. Source of truth is SQLite.
- Day-1 decision points. First run is one Enter-press; everything else surfaces only when the user goes looking.

## Deferred to v3 (Sentry mode)

When v3 lands the agency-compliance product, this doc gains a Sentry section that adds:

- `--mode=sentry` boot flag (not flippable from a `Privileged` principal at runtime).
- `StaticX509Verifier`, `VaultPkiVerifier`, `StepCaVerifier` (external CA support).
- FIPS 140-3 crypto provider gate at startup.
- SCIM 2.0 endpoint + LDAP polling fallback for Joiner-Mover-Leaver propagation.
- Group → scope mapping (`scope_mappings` table).
- M-of-N admin quorum (`pending_admin_actions` table) — default-on at N=2.
- Smart-card / Derived PIV mTLS for GFE devices.
- `min_key_storage = tpm` floor on non-loopback listeners.
- Hash-chained audit log forwarded to agency SIEM (CEF / structured syslog).
- `posture_boot_mode` event + `BootPostureDrift` detection in `borg audit verify`.
- Admin-cert-distinct-from-operator-cert separation (AC-5).
- L3 quorum recovery (replaces L2 phrase, which is disabled in Sentry).
- NIST SP 800-53 / 800-171 / FedRAMP crosswalk.

The v1 schema (`paired_devices.key_storage_tier`, `listeners.max_tier`, audit `principal_id`, etc.) is forward-compatible — v3 additions are columns and rows, not migrations of v1 data.

## Open questions (v1)

1. **Per-tier capability set** — exact mapping of `Read`/`Write`/`Execute`/`Privileged` to the current tool catalog needs to be enumerated and version-pinned.
2. **Client cert format for TOFU** — self-signed X.509 vs raw public key (RPK, RFC 7250). Default to self-signed X.509; revisit RPK if it lands cleanly upstream.
3. **PAKE choice — SPAKE2 vs CPace.** Pick based on Rust library quality at implementation time.

## References

- Existing channel pairing flow: `crates/cortex/src/db/pairing.rs` (approved_senders, pairing_requests)
- Existing gateway server: `crates/gateway/src/server.rs`
- Hermes OpenAI-compat reference: `reference/hermes-agent/gateway/`
- IETF RFC 8125 (PAKE survey), CFRG CPace draft, RFC 7250 (RPK)
