# Plugins

Borg ships with two plugin layers. The classic **marketplace** layer (`crates/plugins/`) covers compile-time-cataloged channel and tool integrations: one-click install, credential storage, template extraction, post-install hooks. The newer **WASM** layer (`crates/plugins/src/wasm.rs`) covers untrusted third-party tool plugins delivered as signed, capability-sandboxed WebAssembly modules.

Native channels (Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage) live in the `gateway` crate. They appear in the marketplace catalog with `is_native: true` so installation only collects credentials — no files extracted, the integration code is already compiled in. Everything else (script channels, script tools) writes templates under `~/.borg/channels/<id>/` or `~/.borg/tools/<id>/`.

## Catalog — compile-time `PluginDef`

`crates/plugins/src/catalog.rs` holds the static `CATALOG: &[PluginDef]`. Each `PluginDef` declares:

| Field | Purpose |
|---|---|
| `id` | Unique slug, may contain `/` (e.g. `messaging/telegram`). |
| `name`, `description`, `category` | UI metadata; `Category` groups plugins in the marketplace. |
| `kind` | `Channel` or `Tool` — picks the install root. |
| `required_credentials` | `CredentialSpec` entries: key, label, help URL, optional flag. |
| `required_bins` | External binaries the integration shells out to. |
| `templates` | Embedded `TemplateFile`s (via `include_str!`) written into the install root. |
| `platform` | `All` or `MacOS` — gates visibility on non-supported OSes. |
| `is_native` | When true, skip template extraction; the integration is compiled into `gateway`. |
| `post_install` | Optional hook for OS-permission prompts, state seeding, etc. |

Look up via `catalog::find_by_id` / `catalog::by_category`. `PluginDef::service_name()` derives the keychain service identifier as `borg-<id-with-slashes-replaced>`.

## Install flow

Driven by `installer::install(def, data_dir, credentials, progress_tx)`. The caller (TUI) collects credentials before calling. The function emits `InstallEvent`s as it goes (`Starting`, `WritingFiles`, `CredentialPrompt`, `CredentialStored`, `Complete`, `Error`) so the TUI can render progress.

1. **Prerequisites.** Required binaries probed via `which`. Missing bins surface as an early error rather than half-installing.
2. **Templates.** Non-native plugins extract `TemplateFile`s under `<data_dir>/channels/<id>/` or `<data_dir>/tools/<id>/` depending on `TemplateTarget`.
3. **Credentials.** Each `(key, value)` is stored via `credential_store::store` (keychain primary, file fallback — see below). `CredentialEntry` records are returned so the TUI can show what landed where.
4. **Script perms.** Shell/Python scripts in the extracted tree get the executable bit set.
5. **File hashes.** SHA-256 of every extracted template is recorded in `InstallResult.file_hashes` for later tamper detection by `verifier::verify` and `~/.borg/skill_audit` style flows.
6. **Post-install hook.** If `def.post_install` is set, the hook runs with the data directory and returns user-facing notes (e.g. iMessage's Full-Disk-Access prompt + Messages-DB cursor seeding).

`installer::uninstall` reverses steps 2 and 3: removes the install directory and removes keychain entries. If a keychain entry resurrects after removal — gnome-keyring caches sometimes do this — a `tracing::warn!` flags it.

`installer::is_installed` checks the manifest file for templated plugins (`channel.toml` / `tool.toml`) or required-credential presence for native plugins (env var **or** keychain).

## Credential storage

`crates/plugins/src/credential_store.rs` is the canonical entry point — installer and gateway both call it rather than touching the OS keychain directly.

- **Primary:** OS keychain via `keychain::store/read/check/remove`. macOS Keychain, Linux Secret Service / gnome-keyring.
- **Fallback:** `~/.borg/.credentials.json`, mode `0600`. Used only when the keychain is unavailable or the user denies the prompt during install. Onboarding stays unblocked on headless Linux without Secret Service.

The same keychain-first/file-fallback pattern is mirrored by `cortex::scripts::get_or_create_hmac_key` for the script HMAC signing key, and by `FileKekStore` for the KEK on headless deployments (see `CLAUDE.md` → "KEK Backend").

Account naming is `borg-<KEY>` under the service `borg-<plugin-id>`. The `service_name()` helper handles the slash-to-dash transform so `messaging/telegram` becomes service `borg-messaging-telegram`.

## Verification

`verifier::verify(def, data_dir)` is the post-install health check:

1. Required binaries on `PATH`.
2. For non-native plugins, install files actually present.
3. For each non-optional credential, env var **or** keychain entry exists.

`verifier::verify_all` runs the check across an installed-id list — used by `borg doctor` and the marketplace UI to show health status. This is a structural check only; per-provider API smoke tests (e.g. Telegram `getMe`) happen when the credential is first exercised by the gateway.

## WASM plugin sandbox

Third-party tool plugins ship as signed, capability-sandboxed WebAssembly modules instead of native code. The sandbox is **tool-only by design**: a plugin contributes one or more LLM-callable tools. Channels, hooks, onboarding scripts, and background tasks are first-class native subsystems and intentionally do not run as WASM plugins — they don't need the publisher-signed-untrusted-code trust model the sandbox exists to provide, and forcing them through a narrow ABI would cost ergonomics without buying security.

Source: `crates/plugins/src/wasm.rs`. Schema tag: `borg.plugin.v1`.

### Package shape

A package is a manifest + a `.wasm` module + an Ed25519 signature over the canonical manifest bytes.

```text
manifest.json   # canonical JSON, schema_version = "borg.plugin.v1"
plugin.wasm     # SHA-256 pinned in manifest.wasm_sha256
signature       # base64 Ed25519 signature over canonical manifest bytes
```

`WasmPluginManifest` fields:

| Field | Purpose |
|---|---|
| `schema_version` | Must be `borg.plugin.v1`. |
| `id`, `name`, `version` | Stable identity. `id` may contain `/` for catalog grouping. |
| `publisher.name`, `publisher.public_key` | Display name and base64 Ed25519 verifying key. |
| `wasm_sha256` | Lowercase hex SHA-256 of the WASM bytes — pinned in the manifest. |
| `tools[]` | `{ name, description, input_schema }` — one entry per LLM-callable tool. |
| `capabilities[]` | Subset of `log`, `state`, `http`, `credentials`. |
| `endpoints[]` | HTTPS allowlist; required iff `http` capability declared. |
| `credentials[]` | Credential keys the plugin may request; required iff `credentials` declared. |
| `limits` | `timeout_ms`, `fuel`, `max_memory_pages`, `max_output_bytes`. |

### Install gauntlet

`verify_package` runs the full gauntlet before any bytes touch the data directory:

1. **Manifest validation.** Schema version, id/name/version shape, tool uniqueness, capability/endpoint/credential coherence, limits in range (`fuel ≤ 50_000_000`, `timeout_ms ≤ 60_000`, `max_memory_pages ≤ 256`, `max_output_bytes ≤ 1 MiB`).
2. **WASM hash pin.** `sha256(wasm) == manifest.wasm_sha256`.
3. **Publisher signature.** Ed25519 over canonical manifest bytes, verified against an operator-trusted publisher list. Untrusted keys are rejected even if the signature is valid.
4. **Static WASM validation.** `wasmparser` walk that rejects:
   - WASI imports of any kind.
   - Host imports outside the declared capability set (see ABI below).
   - `start` functions.
   - `memory64` or shared memory.
   - More than one memory, or initial/declared maximum pages above the manifest limit.
   - Missing ABI exports (`memory`, `alloc`, `borg_tool_call`).
   - Endpoint allowlist entries outside `https://`.
   - Unknown capability names.
5. **Local HMAC seal.** After the above, `compute_local_hmac` produces a per-install HMAC-SHA256 over the canonical manifest + WASM bytes using a host-local key. Stored alongside the installed plugin.

`install_verified_package` writes `manifest.json` and `plugin.wasm` under `<data_dir>/wasm_plugins/<safe_id>/`. The safe-id transform strips path separators so a malicious `id` cannot escape the install root.

### Pre-execution integrity

Every load calls `verify_installed_hmac` against the stored seal. Any tampering after install — manifest edit, WASM swap, signature stripped — fails the constant-time HMAC compare and the plugin is **quarantined** (will not execute) until reinstalled from a fresh signed package.

### Host ABI

Plugin guest exports:

- `memory` — the single linear memory.
- `alloc(len: i32) -> i32` — host calls this to allocate input buffers inside the guest.
- `borg_tool_call(ptr: i32, len: i32) -> i64` — packed `(ptr << 32) | len` of the output buffer.

Plugin guest imports (each gated by the matching capability):

| Module | Function | Capability |
|---|---|---|
| `borg:log` | `log(ptr, len)` | `log` |
| `borg:http` | `fetch(ptr, len) -> i32` | `http` (allowlist enforced host-side) |
| `borg:state` | `read(ptr, len) -> i32`, `write(ptr, len) -> i32` | `state` |
| `borg:credentials` | `get(ptr, len) -> i32` | `credentials` |

A plugin that declares `http` MUST supply at least one `https://` endpoint in `endpoints`; conversely, endpoints without `http` are rejected. Same coherence rule for `credentials`/`credentials[]`.

### Runtime

Executed via `wasmtime` with:

- **Fuel** metering bounded by `limits.fuel` (host cap `50_000_000`).
- **Memory** bounded by `limits.max_memory_pages` (host cap `256` pages = 16 MiB).
- **Output** bounded by `limits.max_output_bytes` (host cap `1 MiB`).
- **Wall-clock** bounded by `limits.timeout_ms` (host cap `60_000` ms).
- **No WASI**, no sockets, no subprocesses, no filesystem.

Plugin-originated HTTP (when the capability is granted) goes through a host-boundary check that enforces the endpoint allowlist **and** runs secret-leak scanning over request URL, headers, and body before any byte hits the network. A request that smells like a leaked API key is refused at the boundary, not after the fact.

### Why this shape

- **Signature + hash pin** means the operator can trust packages by publisher identity, not by registry hostname.
- **Local HMAC seal** turns post-install tampering — the most realistic attacker move on a compromised host — into a hard quarantine rather than a silent compromise.
- **Capability-gated host imports** plus **static validation at install** means an unexpected host call surfaces *before* runtime, not as a confusing runtime error.
- **Narrow ABI** (`alloc` + `borg_tool_call`) keeps the host attack surface small and makes guest SDKs trivial to write in any language with a WASM target.

### Operator surface

- **Kill switches.** `borg plugin disable <id>` / `enable <id>` flip a row's status without touching the install; `disable --publisher <key>` / `enable-publisher <key>` blanket-disable by signing key. Both lists live in `plugins.wasm.disabled_plugin_ids` / `plugins.wasm.disabled_publisher_keys` and are checked at tool-listing and dispatch time.
- **Rate limiting.** Per-`plugin_id` token bucket gated by `plugins.wasm.rate_limit_per_minute` (default 60) and `plugins.wasm.rate_limit_burst` (default 10). Refused calls return a retry-after error to the model and emit a `wasm_plugin_rate_limited` audit event. Zero values fail closed — they don't unlock unlimited throughput.
- **Key deprecation.** `security.deprecated_publisher_keys` carries `{ key, sunset_at, reason }` entries. Install under a deprecated-but-pre-sunset key warns + records `deprecated_key=true` in the audit row; post-sunset installs are rejected.
- **Seal integrity.** The local HMAC is a single scheme with no fallback. A stored seal that does not verify quarantines the plugin at dispatch and emits `wasm_plugin_quarantined` with `reason_category=integrity_failure`. Changing the sealing scheme means operators reinstall — that is deliberate, since a second accepted scheme would weaken the tamper signal to whichever scheme is easiest to forge.
- **Observability.** OpenTelemetry counters/histograms under the `borg.wasm.*` namespace: `installs_total`, `executes_total`, `execute_duration_ms`, `quarantines_total`, `cache_hits_total` / `cache_misses_total`, `rate_limited_total`, `resource_exhausted_total{kind}`. Wired through the same `Metrics` seam as the rest of the agent — enable with `telemetry.metrics_enabled`.

## Roadmap

See `docs/ROADMAP.md` → "Plugins & extensibility" for the marketplace follow-ups (`plugin.json` onboard hooks, plugin SDK seams, doctor-driven dep install). Current invariants are pinned by tests in `crates/plugins/src/wasm.rs` and `crates/plugins/src/verifier.rs`.
