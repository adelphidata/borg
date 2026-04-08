# Sandboxing

Scripts and channel integrations run inside a platform-specific sandbox. This isolates execution from the host system, limiting filesystem access and network capabilities.

## Overview

- **macOS**: Uses `sandbox-exec` with generated [Seatbelt](https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf) profiles
- **Linux**: Uses [Bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) with namespace isolation
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

```toml
[sandbox]
enabled = false
```

When disabled, tools run directly as subprocesses without isolation.
