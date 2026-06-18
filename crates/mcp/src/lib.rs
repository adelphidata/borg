//! Model Context Protocol (MCP) client.
//!
//! Borg connects out to external MCP servers, discovers their tools, and
//! surfaces them to the agent loop. The integration is *additive*: MCP tools
//! merge into [`core_tool_definitions`](../cortex/) at the same call site as
//! WASM plugin tools, and disabling `mcp.enabled` returns borg to exactly the
//! pre-change state.
//!
//! ## Trust posture
//!
//! MCP servers are treated as **untrusted code at arm's length**:
//!
//! - Every server response is *untrusted user input* in the LLM context — the
//!   cortex adapter wraps responses in `<mcp_tool_result trust="untrusted">`
//!   and routes them through prompt-injection scoring + secret redaction.
//! - Stdio subprocesses receive a strict environment allowlist; borg's API
//!   keys, KEK passphrase, and OAuth tokens never leak to children.
//! - Stdio stderr is captured to a per-server log file, never returned to the
//!   LLM (credential exfiltration vector).
//! - HTTP transports are `https://`-only by default and refuse loopback /
//!   RFC1918 destinations unless `allow_local_network` is set. DNS hostnames
//!   are resolved at check time, every resolved address is checked, and the
//!   resolved IPs are pinned into the reqwest connector so a hostile
//!   authoritative DNS server cannot rebind the host between the check and
//!   the connect.
//! - TLS uses `rustls` across all platforms (the workspace `reqwest` dep is
//!   pinned to `rustls-tls` with default features off) so the verifier
//!   behaviour is identical on macOS / Linux / CI.
//!
//! Covers MCP *tools* only. Resources, prompts, sampling, and
//! borg-as-MCP-server are out of scope.
#![warn(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::needless_borrow,
        clippy::redundant_clone,
        clippy::uninlined_format_args,
        clippy::unwrap_used
    )
)]

pub mod errors;
pub mod manifest;
pub mod protocol;
pub mod registry;
pub mod security;
pub mod session;
pub mod transport;

pub use errors::McpError;
pub use manifest::{HttpAuth, ServerManifest, ToolFilter, Transport};
pub use protocol::{McpToolSchema, ToolCallResult};
pub use registry::{McpRegistry, RegisteredTool, RegistryStatus};
pub use security::{is_stdio_command_suspicious, validate_stdio_command};
