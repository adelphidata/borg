# Security Policy

## Supported Versions

Only the latest release receives security updates.

## Reporting a Vulnerability

If you discover a security vulnerability, please report it responsibly:

1. **Do not** open a public issue
2. Use GitHub private vulnerability reporting or email the maintainers directly
3. Include steps to reproduce, potential impact, and a suggested fix if you have one

Vulnerabilities in the Borg agent or daemon itself should be reported to the [Borg repository](https://github.com/adelphidata/borg/security/advisories/new) instead.

## Response Timeline

- **Acknowledgment:** within 48 hours
- **Assessment:** within 7 days
- **Fix or mitigation plan:** within 14 days for confirmed vulnerabilities

## Scope

In scope for this repository: Electron security issues (context isolation bypass, node integration escapes, navigation/window-open handling), leakage of user data from the renderer, and supply-chain issues in the build.
