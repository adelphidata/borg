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
//!   are resolved at check time and rejected if *any* resolved address is
//!   local (SSRF guard).
//!
//! ## Known follow-ups (Phase 2)
//!
//! - **TLS backend pinning.** Phase 1 uses the default `reqwest` TLS
//!   backend (`native-tls` on macOS, system OpenSSL on Linux). Both verify
//!   certs by default, but a Phase 2 should enable the `rustls-tls`
//!   workspace feature on `reqwest` so the verifier is identical across
//!   platforms.
//! - **DNS rebinding.** The URL guard resolves DNS once at check time and
//!   passes the parsed URL to `reqwest`, which resolves again at connect
//!   time. A hostile authoritative DNS server could return a public IP on
//!   the first lookup and a private IP on the second. Phase 2 should pin
//!   the resolved IP into the connector.
//!
//! Phase 1 covers MCP *tools* only. Resources, prompts, sampling, and
//! borg-as-MCP-server are deferred.
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
pub mod session;
pub mod transport;

pub use errors::McpError;
pub use manifest::{HttpAuth, ServerManifest, ToolFilter, Transport};
pub use protocol::{McpToolSchema, ToolCallResult};
pub use registry::{McpRegistry, RegisteredTool, RegistryStatus};
