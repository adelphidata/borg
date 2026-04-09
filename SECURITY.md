# Security Policy

## Supported Versions

| Version        | Supported |
| -------------- | --------- |
| Latest release | Yes       |
| Older releases | No        |

Only the latest release receives security updates. We recommend always running the most recent version.

## Reporting a Vulnerability

If you discover a security vulnerability, please report it responsibly:

1. **Do not** open a public issue
2. Use [GitHub private vulnerability reporting](https://github.com/borganization/borg/security/advisories/new) or email the maintainers directly
3. Include:
    - Steps to reproduce
    - Potential impact
    - Suggested fix (if any)

## Response Timeline

- **Acknowledgment:** within 48 hours
- **Assessment:** within 7 days
- **Fix or mitigation plan:** within 14 days for confirmed vulnerabilities

## Scope

The following are in scope for security reports:

- **Sandbox escapes** — tools or channels bypassing filesystem/network restrictions
- **Prompt injection** — external input causing the agent to execute unintended actions
- **Secret leakage** — API keys, tokens, or credentials exposed in logs, outputs, or tool results
- **Credential store vulnerabilities** — issues with keychain integration or credential resolution
- **Gateway authentication bypass** — unauthorized access to the webhook gateway or pairing system
- **Path traversal** — accessing files outside allowed directories (especially blocked paths like `.ssh`, `.aws`)

Out of scope:

- Issues requiring physical access to the machine
- Social engineering attacks against the user
- Vulnerabilities in upstream LLM providers

## Security Architecture

Borg includes multiple layers of security by design:

- **File permissions** — the database file is restricted to owner-only access (mode 0600)
- **Sandboxed tool execution** — macOS Seatbelt and Linux Bubblewrap isolate user tools
- **Blocked path enforcement** — sensitive directories (`.ssh`, `.aws`, `.gnupg`, etc.) are filtered from tool access
- **Secret redaction** — API keys and tokens are automatically redacted from tool outputs
- **Prompt injection detection** — scoring-based input sanitization flags suspicious content
- **Rate limiting** — per-session caps on tool calls, shell commands, file writes, and web requests
- **Sender pairing** — gateway messages from unknown senders require approval before processing
