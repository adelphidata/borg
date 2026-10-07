# Sandboxing

Scripts and channel integrations run inside a platform-specific sandbox. This isolates execution from the host system, limiting filesystem access and network capabilities.

## Overview

- **macOS**: Uses `sandbox-exec` with generated [Seatbelt](https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf) profiles
- **Linux**: Uses [Bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) with namespace isolation
- **Windows**: Tier 1 (restricted token + DACL ACEs + `CreateProcessAsUserW`) **ported but not yet validated on a Windows host**. Builds clean against the Windows target; tests on the FFI surface are gated `cfg(windows)` and need a Windows VM to run. See the [Windows](#windows-in-development) section below.
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

`bwrap` (and `sandbox-exec` on macOS) is the one program Borg runs *before* the sandbox exists, so it is never taken from a place a sandboxed tool could write to. Borg walks `PATH` and skips relative entries, any candidate under an `fs_write` grant or the tool's directory, any binary that is group- or world-writable or not executable, and any binary whose directory is world-writable; the first clean candidate is launched by its absolute path. If no entry passes, Borg logs a warning and runs without the sandbox, exactly as if `bwrap` were not installed. On macOS the helper is always `/usr/bin/sandbox-exec`.

## Windows (in development)

Windows has no built-in command-line sandbox wrapper analogous to `sandbox-exec` or `bwrap`, so Borg implements its own using Windows kernel security primitives. **Tier 1 is ported but has not yet been validated on a real Windows host** — it cross-compiles cleanly and the platform-neutral pieces are unit-tested on every CI host, but the FFI paths run for the first time inside a Windows VM. Treat the Tier 1 enforcement as in-development until that smoke validation has been done.

### Tier 1 architecture (current)

`SandboxPolicy::wrap_command` on Windows produces a re-exec of the running `borg.exe` with a hidden subcommand: `borg __windows-sandbox-spawn --policy <json> --tool-dir <path> -- <program> <args...>`. The dispatcher in `crates/interface/src/main.rs` intercepts that subcommand before clap sees argv (it is hidden from `--help` and never registered as a clap subcommand), then hands off to `sandbox::windows::runtime::run`:

1. Generate a per-spawn random capability SID (`S-1-5-21-{a}-{b}-{c}-{d}`).
2. `OpenProcessToken` on the current process, then `CreateRestrictedToken` with the cap SID added as a *restricting SID*. The kernel intersects this with the original token's groups on every access check, so a path with no ALLOW ACE for the cap SID is unreachable from the child even though the parent user has access.
3. Resolve the policy into a concrete ACE batch: DENY READ for `deny_read`, DENY WRITE for `deny_write` (which always includes `~/.borg`), ALLOW WRITE for `fs_write` roots.
4. Apply the ACEs via `SetEntriesInAclW` + `SetNamedSecurityInfoW` (`SetEntriesInAclW` enforces DENY-before-ALLOW ordering for us). The result is wrapped in an `AclGuard` whose `Drop` reverts every ACE — a panicking sandbox run does not permanently mutate filesystem ACLs.
5. `CreateProcessAsUserW` the wrapped command under the restricted token, inheriting the trampoline's stdio so output flows through to the user's terminal.
6. Wait for the child to exit, drop the guard (reverts ACEs), re-emit the child's exit code as our own.

Every step fails closed: any error short-circuits to `std::process::exit(2)` with a clear message; the wrapped command never runs unsandboxed.

### Tier 1 limitations

- **No network isolation.** `network = false` does not block outbound connections in Tier 1 — that needs WFP, which needs admin. The Tier 2 elevated backend (planned) closes this gap. Today, set `network = true` for tools that legitimately need it; the *false* setting is documentation only on Windows.
- **No DLL planting protection.** `PATH` is forwarded into the child's environment per `SAFE_ENV_VARS`, but Borg does not normalize it to `System32` before spawning. A poisoned PATH on the parent leaks into the child's DLL search path. Tracked in `crates/sandbox/src/constants.rs`.
- **System paths are restricted by Windows itself.** The cap SID can't add new authority over paths the user has no access to. The sandbox prevents *the agent's tools* from reaching the user's `~/.ssh` etc; it does not stop a tool that already runs as Administrator from escalating, because such a tool wouldn't go through Borg's sandbox in the first place.

### Planned Tier 2 (deferred)

Two-tiered with auto-fallback when an operator runs `borg service install --windows-sandbox` once:

| Tier | Mechanism | Admin? | Network isolation | Filesystem isolation |
|------|-----------|--------|-------------------|----------------------|
| **Tier 1** (current, no admin) | Restricted token + capability SIDs + DACL deny/allow ACEs, `CreateProcessAsUserW` | No | None | DENY ACEs on `blocked_paths` + `deny_read`/`deny_write`; ALLOW ACE for capability SID on `fs_write` roots only |
| **Tier 2** (deferred, admin once) | Above + spawn child as dedicated `BorgSandboxOffline`/`BorgSandboxOnline` user via the hidden runner subcommand; WFP firewall rules deny all outbound for that SID unless `network=true` | Once at install | Real (WFP kernel filter) | Above + native cross-user file ACL deny |

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
