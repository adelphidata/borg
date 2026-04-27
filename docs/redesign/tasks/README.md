# v1 Redesign — Task Breakdown

Five logically isolated tasks. Each is sized for a separate agent session and links back into the design docs in `docs/redesign/`. Execute in roughly the listed order — later tasks assume the daemon + gRPC kernel from Task 01 exists.

| # | Task | Touches | Depends on |
|---|---|---|---|
| 01 | [Daemon kernel + gRPC + TUI client](01-daemon-grpc-kernel.md) | new `borgd` crate, agent-loop lift, `crates/interface` TUI rewrite, capability protocol stubs | — |
| 02 | [Seamless mTLS + PAKE pairing + audit chain](02-mtls-pairing-audit.md) | internal CA, paired_devices, audit_log_chain, verifier trait | 01 (gRPC server exists) |
| 03 | [Layer 5 ingress adapters](03-ingress-adapters.md) | port 8 native channels + OpenAI-compat HTTP + webhook + MCP into daemon | 01 (daemon hosts adapters) |
| 04 | [LLM provider plugin layer + budgets](04-providers-and-budgets.md) | `Provider` trait, rate cards, token accounting, `budgets` table, pre/post-flight enforcement | 01 (daemon owns LLM calls) |
| 05 | [Posture v2 + data handling + cleanup](05-posture-data-cleanup.md) | 3-state posture, retention/tombstone/backups, dead-code purge, stale-doc sweep | 01–04 (final pass) |

## Ground rules across all tasks

- **Preserve the CLAUDE.md invariants verbatim.** No per-tool-call approval prompts. Mouse text-selection guards stay. Error handling rules apply.
- **Tests for every behavior change** (regression test that fails before, passes after). Reject the fluff-test patterns listed in CLAUDE.md.
- **Schema-stub `principal_id` everywhere** even though v1 has one principal — forward-compat for v3 multi-tenancy.
- **No workspaces in v1.** One global agent state per `BORG_HOME`.
- **No vault subsystem.** OS keychain (Keychain / Secret Service / TPM) for mTLS keys; existing `[credentials]` table for API keys.
- **Sentry-mode features are out of scope.** If a task spec mentions them, it's flagged as v3-deferred — do not implement.
