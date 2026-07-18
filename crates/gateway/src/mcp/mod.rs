//! Model Context Protocol (MCP) server.
//!
//! Exposes a subset of Borg's tools to MCP clients (Claude Desktop, MCP
//! Inspector, custom clients) over two transports:
//!
//! - **Stdio** — `borg mcp stdio` subcommand. JSON-RPC 2.0 messages
//!   over stdin/stdout using LSP-style framing
//!   (`Content-Length: N\r\n\r\n<json>`). What Claude Desktop spawns.
//!
//! - **HTTP** — `POST /mcp` with `Authorization: Bearer <token>`.
//!   Same bearer auth as the OpenAI-compat surface.
//!
//! ## Read-only tools only
//!
//! v1 exposes ONLY read-only tools: `memory_search`, `read_memory`,
//! `web_fetch`, `web_search`, `list` (read-only). Mutating tools
//! (`apply_patch`, `run_shell`, `write_memory`) are deliberately NOT
//! exposed — MCP is a tool surface, not a chat surface, and giving
//! third-party clients write authority over the agent's own state is
//! an unreviewed privilege escalation.
//!
//! The exclusion is enforced by a `tools/list` test that fails if a
//! mutating tool is ever added to the registry. Adding a mutator
//! intentionally requires updating that guard test (see the comment
//! on `tools_list_only_exposes_read_only_tools` in `server.rs`).

mod handlers;
mod routes;
mod server;
pub mod stdio;

pub use handlers::build_read_only_tools;
pub use routes::{http_router, McpHttpState};
pub use server::{McpError, McpServer, McpToolDescriptor, ReadOnlyToolHandler};
