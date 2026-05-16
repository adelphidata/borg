# Sandboxing

Scripts and channel integrations run inside a platform-specific sandbox. This isolates execution from the host system, limiting filesystem access and network capabilities.

## Overview

- **macOS**: Uses `sandbox-exec` with generated [Seatbelt](https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf) profiles
- **Linux**: Uses [Bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) with namespace isolation
- **Windows**: Architecture scaffolded but **runtime not yet implemented**. With `sandbox.enabled = true`, every wrapped command fails closed (refuses to run) rather than silently passing through. See the [Windows](#windows-in-development) section below.
- **Other platforms**: No sandboxing (tools run unsandboxed)

Sandboxing is enabled by default and configured via settings:

```sh
borg settings set sandbox.enabled true
borg settings set sandbox.mode strict
```

## Per-script sandbox policy

Each script or channel defines its sandbox permissions:

```toml
[sandbox]
network = false           # deny network access by default
fs_read = ["/etc/ssl"]    # additional paths the script can read
fs_write = ["/tmp"]       # paths the script can write to
```

### Policy fields

| Field | Default | Description |
|-------|---------|-------------|
| `network` | `false` | Whether the tool can make network connections |
| `fs_read` | `[]` | Extra filesystem paths the tool can read |
| `fs_write` | `[]` | Filesystem paths the tool can write to |
| `deny_read` | `[]` | Paths to explicitly deny read access (takes precedence over fs_read) |
| `deny_write` | `[]` | Paths to explicitly deny write access |

### Automatic protections

- **Borg directory protection**: `~/.borg/` is automatically added to `deny_write` to prevent tools from modifying agent config, memory, or other tools.
- **Blocked path filtering**: Paths listed in `[security] blocked_paths` config (defaults: `.ssh`, `.aws`, `.gnupg`, `.config/gh`, `.env`, `credentials`, `private_key`) are filtered from tool `fs_read`/`fs_write` before sandbox profile generation.
- **TLS paths**: When `network = true`, standard TLS certificate paths are automatically added to `fs_read`.
- **Windows blocked-path enforcement (planned)**: The Windows runtime will enforce `blocked_paths` via DACL DENY ACEs in addition to the path-string filter, so a tool that bypasses the filter via path canonicalization tricks still hits a kernel deny. Not yet active — the trampoline currently fails closed before reaching this layer.

## macOS (Seatbelt)

On macOS, the sandbox generates a Seatbelt profile with a deny-all default and explicit allows:

- Process execution is allowed
- Read access to the tool directory, standard library paths, and any `fs_read` paths
- Write access to any `fs_write` paths
- Network access only if `network = true`

The generated profile is passed to `sandbox-exec -p <profile>`.

## Linux (Bubblewrap)

On Linux, Bubblewrap creates an isolated namespace:

- `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin` are mounted read-only
- `/proc` is mounted
- The tool directory is bind-mounted read-only
- `fs_read` paths are bind-mounted read-only
- `fs_write` paths are bind-mounted read-write
- Network is unshared (isolated) unless `network = true`
- A new PID namespace is created

Requires `bwrap` to be installed. On Debian/Ubuntu:

```sh
sudo apt install bubblewrap
```

## Windows (in development)

Windows has no built-in command-line sandbox wrapper analogous to `sandbox-exec` or `bwrap`, so Borg implements its own using Windows kernel security primitives. **The runtime that applies the restricted token and DACL ACEs is not yet implemented.** What ships today is the trampoline architecture, fail-closed wired:

- `SandboxPolicy::wrap_command` on Windows produces a re-exec of the running `borg.exe` with a hidden subcommand: `borg __windows-sandbox-spawn --policy <json> --tool-dir <path> -- <program> <args...>`.
- The `borg` binary intercepts that subcommand before clap parses argv (it is hidden from `--help` and never registered as a clap subcommand).
- The trampoline parses the policy, validates the wire format, then **exits with a non-zero status and a clear error message** because the restricted-token logic has not landed.

The intentional choice here is to fail loud rather than silently run unsandboxed: a Windows operator who sets `sandbox.enabled = true` today will see the wrapped command refuse to start, surfacing the gap. To run on Windows in the meantime, set `sandbox.enabled = false` explicitly — that takes the wrapping path out entirely and runs commands as plain subprocesses, exactly as on the macOS/Linux paths when sandboxing is disabled.

### Planned design (two tiers)

When the runtime lands, the design is two-tiered with auto-fallback (mirroring `security.kek_backend = auto`):

| Tier | Mechanism | Admin? | Network isolation | Filesystem isolation |
|------|-----------|--------|-------------------|----------------------|
| **Legacy** (default, no admin) | Restricted token + capability SIDs + DACL deny/allow ACEs, `CreateProcessAsUserW` | No | None (warns when `network=false`) | DENY ACEs on `blocked_paths` + `deny_read`/`deny_write`; ALLOW ACE for capability SID on `fs_write` roots only |
| **Elevated** (opt-in, admin once) | Above + spawn child as dedicated `BorgSandboxOffline`/`BorgSandboxOnline` user via the hidden runner subcommand; WFP firewall rules deny all outbound for that SID unless `network=true` | Once at install | Real (WFP kernel filter) | Above + native cross-user file ACL deny |

### Wire format and security notes

The trampoline's argv is the wire format between the parent and the spawned `borg.exe` subcommand. Two transitional weaknesses are documented in `crates/sandbox/src/windows/mod.rs` and must be addressed before the runtime lands:

- **Policy on argv is observable to same-session local processes** (via WMI / `tasklist /v` / `Get-CimInstance`). The policy itself is not a credential, but `deny_read` / `deny_write` paths and `allowed_domains` form a useful map for a different-vector attacker. Plan: replace with an inheritable anonymous-pipe handle or a transient HMAC-sealed file at `~/.borg/run/sandbox-<nonce>.json` with a per-spawn nonce.
- **Caller authentication is by name only.** Any process that can invoke `borg.exe` can hand-craft a `__windows-sandbox-spawn --policy ...` invocation. Plan: parent-PID validation (same `borg.exe` path, same logon session) plus an inheritable shared-memory nonce only the spawning parent could have written.

These are blockers on shipping Tier 1 functionally, not on the trampoline scaffold itself.

## Runtime resolution

Before sandboxing, the executor resolves the runtime binary:

| Runtime | Binary resolved |
|---------|----------------|
| `python` | `python3` |
| `node` | `node` |
| `deno` | `deno run` |
| `bash` | `bash` |

The resolved binary path is included in the sandbox command.

## Disabling sandboxing

To disable sandboxing globally:

```sh
borg settings set sandbox.enabled false
```

When disabled, tools run directly as subprocesses without isolation. Config is DB-only; there is no `config.toml`.
