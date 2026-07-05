//! gRPC client for `borgd`.
//!
//! Wraps a tonic UDS channel and exposes ergonomic methods that match the
//! shape of the in-process Agent API the TUI / REPL / CLI subcommands expect:
//! `connect()`, `open_session()`, `send_message()`, `stream_events()`,
//! `respond_to_prompt()`, `cancel()`, plus admin RPCs (`poke`, `heal`,
//! settings I/O, list_sessions, get_session).
//!
//! Auto-spawn convenience: `connect_or_spawn` launches a
//! fresh `borgd` if no socket is present at the expected path (dev
//! workflow). Production deployments should run `borgd` as a supervised
//! service and connect via `connect` which errors cleanly
//! when the socket is missing.

mod connect;
mod event_stream;

pub use connect::{connect, default_socket_path};
pub use event_stream::{forward_event_stream, spawn_event_adapter};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::{
    ActivityLogQuery, ActivityLogText, AddCredentialRequest, AddProviderRequest,
    AgentList as PbAgentList, AgentListRequest, AgentRef as PbAgentRef, AgentSnapshotList,
    AgentSnapshotRefRequest, AgentTreeEvent as PbAgentTreeEvent, AmbientStatus, ApiKeyList,
    ApiKeyRef, ApiKeyValue, ApprovalGreetingRequest, ApprovedSenderList, ApprovedSenderRef,
    BrowserManageRequest, BrowserManageResponse, ClientTokenRef, CreateWebhookRequest,
    CreateWebhookResponse, CredentialEntry, CredentialList, DaemonStatus,
    DoctorSquelchClearRequest, DoctorSquelchSetRequest, DreamReport, Empty as AdminEmpty,
    ExportSessionRequest, ExportSessionResponse, GetScheduledTaskRunsRequest, GoalDetail,
    GoalEvent as PbGoalEvent, GoalMutation, GoalRef, HandoffFailRequest,
    HandoffRequest as AdminHandoffRequest, HandoffStatusReply, HealReport,
    HeartbeatEvent as PbHeartbeatEvent, IssueClientTokenRequest, IssueClientTokenResponse,
    ListCredentialsRequest, LogActivityRequest, MonthlyTokenTotal, MonthlyTotalCost,
    OnboardingState, PairingApproval, PairingFilter, PairingRequestList, PluginInstallRequest,
    PluginInstallResponse, ReloadRequest, ReloadResponse, RemoveCredentialRequest,
    RemoveProviderRequest, ResetCredentialsRequest, RevokedClientToken, ScheduledTask,
    ScheduledTaskCreate, ScheduledTaskDetail, ScheduledTaskList, ScheduledTaskRef,
    ScheduledTaskRunList, ScheduledTaskUpdate, SessionDetail, SessionList, SessionRef,
    SetCredentialStrategyRequest, SetSubprocessPinnedRequest, SettingKey, SettingMutation,
    SettingsSnapshot, SubprocessSnapshot, TriggerHeartbeatRequest, TunnelStatusReply, WarningList,
    WebhookRef,
};
use protocols::pki::pki_client::PkiClient;
use protocols::pki::{
    CertLifecycleResponse, PairedDeviceList, PkiEmpty, RenewCertRequest, RenewCertResponse,
    RevokeDeviceRequest, StartPairingRequest, StartPairingResponse, VerifyAuditResponse,
};
use protocols::session::session_client::SessionClient;
use protocols::session::{
    acp_client_message, acp_fs_request, acp_fs_response, acp_server_message, AcpCancel,
    AcpClientMessage, AcpFsCapabilities, AcpFsError, AcpFsResponse, AcpReadTextFileResponse,
    AcpServerMessage, AcpStartTurn, AcpWriteTextFileResponse, AgentEvent,
    AppendAssistantMessageRequest, AutoSaveRequest, CancelRequest, ClearRequest, CloseRequest,
    CompactRequest, CompactResponse, ForkRequest, ForkResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, OpenRequest, PromptResponse,
    RewindRequest, RewindResponse, SendRequest, SessionState, SessionStateRequest, SteerRequest,
    StreamRequest, UndoRequest, UndoResponse,
};
use protocols::status::status_client::StatusClient;
use protocols::status::{
    BondEventList, BondStateProto, Empty as StatusEmpty, EvolutionEventList, EvolutionStateProto,
    Int64Cursor, StatusText, StatusTextRequest, UsageRequest, Vitals, VitalsEventList, XpSnapshot,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Streaming;

#[allow(unused_imports)]
pub use connect::connect_or_spawn;

/// Turn a goal-RPC `tonic::Status` into an `anyhow::Error` that preserves both
/// the daemon's specific message AND the gRPC code. `.context("…failed")?`
/// would chain the Status inside anyhow but its outer `Display` — what the
/// TUI footer shows — drops both. Without this, users see "Admin.SetGoal
/// {Replace} failed" with no clue the daemon actually said "no active
/// session — start a turn first" and no way to tell `FailedPrecondition`
/// (terminal, user-visible) from `Unavailable` (transient, retry).
fn goal_rpc_err(op: &str, status: &tonic::Status) -> anyhow::Error {
    let code = status.code();
    let msg = status.message();
    if msg.is_empty() {
        anyhow!("{op} failed ({code:?})")
    } else {
        anyhow!("{op} failed ({code:?}): {msg}")
    }
}

/// Lightweight handle wrapping a tonic channel + the four primary clients.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    session: SessionClient<Channel>,
    admin: AdminClient<Channel>,
    status: StatusClient<Channel>,
    pki: PkiClient<Channel>,
}

#[async_trait]
pub trait AcpFsClient: Clone + Send + Sync + 'static {
    async fn read_text_file(&self, path: &str) -> Result<String>;
    async fn write_text_file(&self, path: &str, content: &str) -> Result<()>;
    fn capabilities(&self) -> AcpFsCapabilities;
}

/// Generates the thin `DaemonClient` unary-RPC wrapper methods from a compact
/// table. Each row expands to a `pub async fn` that calls one tonic stub
/// method, awaits it, and either decodes or discards the reply.
///
/// Row grammar:
///
/// ```text
/// /// doc comment(s), attached verbatim to the generated method
/// <stub> fn <name>(<arg>: <Ty>, …) [-> <ReplyTy>] = <rpc>(["<Ctx>",] <request expr>);
/// ```
///
/// - `<stub>` is a `DaemonClient` field: `admin`, `session`, `status`, `pki`.
/// - `<rpc>` is the tonic stub method invoked with `<request expr>` (which
///   may reference the row's args).
/// - `"<Ctx>"` is the error-context prefix using the PascalCase RPC name
///   (e.g. `"Admin.TunnelStart"`); the macro appends `" failed"`. Omitting it
///   (the pki rows) surfaces the raw `tonic::Status` via `?` instead of
///   wrapping with `.context(…)`.
/// - With `-> ReplyTy` the wrapper returns `Ok(reply.into_inner())`; without
///   it the reply is discarded and `Ok(())` returned.
macro_rules! rpc_wrappers {
    // Internal: reply-returning wrapper with `.context("… failed")`.
    (@method
        $(#[$doc:meta])*
        $stub:ident fn $name:ident ( $($arg:ident : $argty:ty),* ) ( $reply:ty )
            = $rpc:ident ( $ctx:literal, $req:expr );
    ) => {
        $(#[$doc])*
        pub async fn $name(&mut self $(, $arg: $argty)*) -> Result<$reply> {
            Ok(self
                .$stub
                .$rpc($req)
                .await
                .context(concat!($ctx, " failed"))?
                .into_inner())
        }
    };
    // Internal: reply-discarding wrapper with `.context("… failed")`.
    (@method
        $(#[$doc:meta])*
        $stub:ident fn $name:ident ( $($arg:ident : $argty:ty),* ) ( )
            = $rpc:ident ( $ctx:literal, $req:expr );
    ) => {
        $(#[$doc])*
        pub async fn $name(&mut self $(, $arg: $argty)*) -> Result<()> {
            self.$stub
                .$rpc($req)
                .await
                .context(concat!($ctx, " failed"))?;
            Ok(())
        }
    };
    // Internal: reply-returning wrapper, raw `tonic::Status` error (no context).
    (@method
        $(#[$doc:meta])*
        $stub:ident fn $name:ident ( $($arg:ident : $argty:ty),* ) ( $reply:ty )
            = $rpc:ident ( $req:expr );
    ) => {
        $(#[$doc])*
        pub async fn $name(&mut self $(, $arg: $argty)*) -> Result<$reply> {
            Ok(self.$stub.$rpc($req).await?.into_inner())
        }
    };
    // Internal: reply-discarding wrapper, raw `tonic::Status` error (no context).
    (@method
        $(#[$doc:meta])*
        $stub:ident fn $name:ident ( $($arg:ident : $argty:ty),* ) ( )
            = $rpc:ident ( $req:expr );
    ) => {
        $(#[$doc])*
        pub async fn $name(&mut self $(, $arg: $argty)*) -> Result<()> {
            self.$stub.$rpc($req).await?;
            Ok(())
        }
    };
    // Entry point: one impl block from the whole table.
    (
        $(
            $(#[$doc:meta])*
            $stub:ident fn $name:ident ( $($arg:ident : $argty:ty),* $(,)? ) $(-> $reply:ty)?
                = $rpc:ident ( $($call:tt)+ );
        )*
    ) => {
        impl DaemonClient {
            $(
                rpc_wrappers!(@method
                    $(#[$doc])*
                    $stub fn $name ( $($arg : $argty),* ) ( $($reply)? )
                        = $rpc ( $($call)+ );
                );
            )*
        }
    };
}

impl DaemonClient {
    /// Construct a client whose channel is lazily connected (and will fail
    /// every RPC). Test-only — used by TUI unit tests that build an `App`
    /// without spawning a real daemon.
    ///
    /// Internally enters a process-wide tokio runtime context so it works
    /// from sync `#[test]` fns; tonic's `connect_lazy` spawns reactor tasks
    /// at construction time.
    ///
    /// Gated to test builds so production binaries cannot accidentally
    /// instantiate a permanently-disconnected client.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn lazy_for_tests() -> Self {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => panic!("lazy_for_tests: failed to build tokio runtime: {e}"),
            }
        });
        let _guard = rt.enter();
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        Self::from_channel(channel)
    }

    /// Wrap an already-connected channel.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            session: SessionClient::new(channel.clone()),
            admin: AdminClient::new(channel.clone()),
            status: StatusClient::new(channel.clone()),
            pki: PkiClient::new(channel),
        }
    }

    /// If the daemon reports the calling client's mTLS cert is inside the
    /// 14-day silent-renewal window, mint a fresh cert and rotate the
    /// keychain-stored PEMs in place. Returns `Ok(true)` if a renewal
    /// happened, `Ok(false)` if no renewal was needed (no cert yet, daemon
    /// reports nothing to renew, or the channel is unauthenticated /
    /// unimplemented — e.g., bootstrap pairing UDS or in-process tests
    /// without mTLS).
    ///
    /// The daemon enforces that the request fingerprint matches the
    /// connection's `Principal` — so callers MUST invoke this on a
    /// channel established with the keychain-stored client cert; passing
    /// the keychain cert's fingerprint while connected with a different
    /// identity is rejected as `permission_denied` by the server.
    ///
    /// **Keychain rotation order** is key-then-cert: a crash between the
    /// two writes leaves the *old* cert paired with the *new* key, which
    /// fails cleanly at next handshake (the daemon will offer renewal
    /// again). The reverse order (cert-first) would brick the client,
    /// because the daemon revokes the old fingerprint as part of the
    /// successful `renew_cert` RPC.
    ///
    /// Errors when a renewal was attempted and failed mid-flight; callers
    /// in fire-and-forget contexts should `tracing::warn!` and continue
    /// rather than abort the user-visible operation.
    pub async fn ensure_cert_fresh<K: identity::KekStore>(&mut self, kek: &K) -> Result<bool> {
        // Pre-pairing connections (bootstrap UDS) have no cert yet — nothing
        // to renew. Skip BEFORE calling the daemon so we don't surface a
        // misleading "no client cert in keychain" error during pairing.
        // `KekStore::get` returns `NotFound` (not `Ok(None)`) for absent
        // accounts; treat that as the pre-pairing case.
        let cert_pem = match kek.get(crate::commands::device_pair::CLIENT_CERT_KEY_ACCOUNT) {
            Ok(pem) => pem,
            Err(identity::PkiError::NotFound(_)) => return Ok(false),
            Err(e) => return Err(anyhow!("read current client cert from keychain: {e}")),
        };

        // Call the underlying tonic stub so we can inspect the gRPC
        // `Status::code()` rather than the anyhow-wrapped form returned by
        // [`Self::get_cert_lifecycle`]. Distinguish "this channel can't
        // tell us" (unauthenticated / unimplemented / permission denied)
        // from real failures — the former is silently a no-op; the latter
        // must surface so the caller can `tracing::warn!`.
        let lifecycle = match self.pki.get_cert_lifecycle(PkiEmpty {}).await {
            Ok(resp) => resp.into_inner(),
            Err(status) => {
                use tonic::Code;
                return match status.code() {
                    Code::Unimplemented | Code::Unauthenticated | Code::PermissionDenied => {
                        Ok(false)
                    }
                    _ => Err(anyhow!("get_cert_lifecycle: {status}")),
                };
            }
        };
        if !lifecycle.renewal_due || lifecycle.not_after == 0 {
            return Ok(false);
        }
        let fp =
            identity::fingerprint_of_pem(&cert_pem).context("compute current cert fingerprint")?;
        let resp = self
            .renew_cert(&fp)
            .await
            .context("daemon renew_cert RPC")?;
        // Key first, then cert — see doc comment for the failure-mode
        // rationale. Anything that survives a key-only crash is recoverable.
        kek.put(
            crate::commands::device_pair::CLIENT_KEY_KEY_ACCOUNT,
            resp.new_key_pkcs8_pem.as_bytes(),
        )
        .context("rotate client key in keychain")?;
        kek.put(
            crate::commands::device_pair::CLIENT_CERT_KEY_ACCOUNT,
            resp.new_cert_pem.as_bytes(),
        )
        .context("rotate client cert in keychain")?;
        Ok(true)
    }

    /// Open a fresh session (or resume the named one if `resume_id` is set).
    /// Returns `(session_id, last_event_seq)`.
    pub async fn open_session(&mut self, resume_id: Option<&str>) -> Result<(String, u64)> {
        self.open_session_with_workspace(resume_id, None, None, false)
            .await
    }

    /// Open a session with an optional explicit workspace context.
    pub async fn open_session_with_workspace(
        &mut self,
        resume_id: Option<&str>,
        workspace_cwd: Option<&std::path::Path>,
        workspace_root: Option<&std::path::Path>,
        enforce_workspace_root: bool,
    ) -> Result<(String, u64)> {
        let resp = self
            .session
            .open(OpenRequest {
                resume_id: resume_id.unwrap_or_default().to_string(),
                workspace_cwd: workspace_cwd
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                workspace_root: workspace_root
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                enforce_workspace_root,
            })
            .await
            .context("Session.Open failed")?
            .into_inner();
        Ok((resp.session_id, resp.last_event_seq))
    }

    /// Send a user message and stream the resulting `AgentEvent`s for this
    /// turn. The stream closes after `TurnComplete` or `Error`.
    pub async fn send_message(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<Streaming<AgentEvent>> {
        self.send_message_with_images(session_id, text, Vec::new())
            .await
    }

    /// Send a multimodal message (text + inline images).
    pub async fn send_message_with_images(
        &mut self,
        session_id: &str,
        text: &str,
        images: Vec<protocols::session::InlineImage>,
    ) -> Result<Streaming<AgentEvent>> {
        let stream = self
            .session
            .send(SendRequest {
                session_id: session_id.to_string(),
                text: text.to_string(),
                images,
            })
            .await
            .context("Session.Send failed")?
            .into_inner();
        Ok(stream)
    }

    /// Send an ACP-backed prompt and service daemon fs callbacks over the
    /// same bidi turn stream.
    pub async fn send_acp_prompt<F: AcpFsClient>(
        &mut self,
        session_id: &str,
        text: &str,
        images: Vec<protocols::session::InlineImage>,
        fs_client: F,
    ) -> Result<mpsc::Receiver<cortex::agent::AgentEvent>> {
        let (client_tx, client_rx) = mpsc::channel::<AcpClientMessage>(32);
        // Pre-load StartTurn into the channel BEFORE opening the bidi stream.
        // The daemon-side `acp_turn` handler awaits the first inbound message
        // before returning a Response — and `acp_turn(rx).await` on the
        // client side doesn't resolve until that Response arrives. Sending
        // StartTurn after the await would deadlock both sides; with the
        // channel pre-loaded, tonic forwards the message as soon as the
        // request body opens and the server unblocks immediately.
        client_tx
            .send(AcpClientMessage {
                kind: Some(acp_client_message::Kind::StartTurn(AcpStartTurn {
                    session_id: session_id.to_string(),
                    text: text.to_string(),
                    images,
                    fs_capabilities: Some(fs_client.capabilities()),
                })),
            })
            .await
            .map_err(|e| anyhow!("failed to queue ACP start_turn: {e}"))?;
        let mut stream = self
            .session
            .acp_turn(ReceiverStream::new(client_rx))
            .await
            .context("Session.AcpTurn failed")?
            .into_inner();

        let (event_tx, event_rx) = mpsc::channel::<cortex::agent::AgentEvent>(256);
        let daemon_client = self.clone();
        let session_id_owned = session_id.to_string();
        tokio::spawn(async move {
            loop {
                match stream.message().await {
                    Ok(Some(AcpServerMessage {
                        kind: Some(acp_server_message::Kind::AgentEvent(pb)),
                    })) => {
                        if let Some(core) = crate::daemon_client::event_stream::convert_pb_event(
                            pb,
                            &daemon_client,
                            &session_id_owned,
                        ) {
                            if event_tx.send(core).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Some(AcpServerMessage {
                        kind: Some(acp_server_message::Kind::FsRequest(request)),
                    })) => {
                        let response = match request.kind {
                            Some(acp_fs_request::Kind::ReadTextFile(read)) => {
                                match fs_client.read_text_file(&read.path).await {
                                    Ok(content) => AcpFsResponse {
                                        request_id: request.request_id,
                                        kind: Some(acp_fs_response::Kind::ReadTextFile(
                                            AcpReadTextFileResponse { content },
                                        )),
                                    },
                                    Err(e) => AcpFsResponse {
                                        request_id: request.request_id,
                                        kind: Some(acp_fs_response::Kind::Error(AcpFsError {
                                            message: e.to_string(),
                                        })),
                                    },
                                }
                            }
                            Some(acp_fs_request::Kind::WriteTextFile(write)) => {
                                match fs_client.write_text_file(&write.path, &write.content).await {
                                    Ok(()) => AcpFsResponse {
                                        request_id: request.request_id,
                                        kind: Some(acp_fs_response::Kind::WriteTextFile(
                                            AcpWriteTextFileResponse {},
                                        )),
                                    },
                                    Err(e) => AcpFsResponse {
                                        request_id: request.request_id,
                                        kind: Some(acp_fs_response::Kind::Error(AcpFsError {
                                            message: e.to_string(),
                                        })),
                                    },
                                }
                            }
                            None => AcpFsResponse {
                                request_id: request.request_id,
                                kind: Some(acp_fs_response::Kind::Error(AcpFsError {
                                    message: "missing ACP fs request kind".to_string(),
                                })),
                            },
                        };
                        if client_tx
                            .send(AcpClientMessage {
                                kind: Some(acp_client_message::Kind::FsResponse(response)),
                            })
                            .await
                            .is_err()
                        {
                            let _ = event_tx
                                .send(cortex::agent::AgentEvent::Error(
                                    "ACP turn stream closed while sending fs response".into(),
                                ))
                                .await;
                            break;
                        }
                    }
                    Ok(Some(AcpServerMessage { kind: None })) => {}
                    Ok(None) => break,
                    Err(e) => {
                        let _ = event_tx
                            .send(cortex::agent::AgentEvent::Error(e.to_string()))
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(event_rx)
    }

    /// Send ACP cancellation on a live bidi turn stream.
    pub async fn send_acp_cancel(
        tx: &mpsc::Sender<AcpClientMessage>,
        session_id: &str,
    ) -> Result<()> {
        tx.send(AcpClientMessage {
            kind: Some(acp_client_message::Kind::Cancel(AcpCancel {
                session_id: session_id.to_string(),
            })),
        })
        .await
        .map_err(|e| anyhow!("failed to send ACP cancel: {e}"))
    }

    /// Subscribe to a session's event stream from `since_event_seq` (use 0 to
    /// replay everything still in the daemon's ring buffer). Long-lived
    /// stream — terminates only on disconnect or session close.
    pub async fn stream_events(
        &mut self,
        session_id: &str,
        since_event_seq: u64,
    ) -> Result<Streaming<AgentEvent>> {
        let stream = self
            .session
            .stream(StreamRequest {
                session_id: session_id.to_string(),
                since_event_seq,
            })
            .await
            .context("Session.Stream failed")?
            .into_inner();
        Ok(stream)
    }

    /// Reply to a `ShellConfirmation` or `UserInputRequest`. The `value` is
    /// `"true"`/`"false"` (or `"yes"`/`"no"`) for confirmations, free-form
    /// text for user-input requests.
    pub async fn respond_to_prompt(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        value: &str,
    ) -> Result<()> {
        self.session
            .respond_to_prompt(PromptResponse {
                session_id: session_id.to_string(),
                prompt_id: prompt_id.to_string(),
                value: value.to_string(),
            })
            .await
            .map_err(|e| anyhow!("Session.RespondToPrompt failed: {e}"))?;
        Ok(())
    }

    // Goal RPC wrappers — see `goal_rpc_err` (top of file) for why these use
    // `map_err` instead of `.context()`.

    /// Read the goal for `session_id`. Empty `session_id` resolves to the
    /// most-recently-active session (matches the TUI's "current session"
    /// semantics). Returns the wire `GoalDetail`; callers check `present`
    /// before reading other fields.
    pub async fn get_goal(&mut self, session_id: &str) -> Result<GoalDetail> {
        Ok(self
            .admin
            .get_goal(GoalRef {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.GetGoal", &s))?
            .into_inner())
    }

    /// Create a brand-new goal. Fails when one already exists.
    pub async fn set_goal_create(
        &mut self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<GoalDetail> {
        Ok(self
            .admin
            .set_goal(GoalMutation {
                session_id: session_id.to_string(),
                op: protocols::admin::goal_mutation::Op::Create as i32,
                objective: objective.to_string(),
                token_budget_present: token_budget.is_some(),
                token_budget: token_budget.unwrap_or(0),
                status: String::new(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.SetGoal{Create}", &s))?
            .into_inner())
    }

    /// Create or replace the goal. Regenerates `goal_id` and resets
    /// accounting on replace.
    pub async fn set_goal_replace(
        &mut self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<GoalDetail> {
        Ok(self
            .admin
            .set_goal(GoalMutation {
                session_id: session_id.to_string(),
                op: protocols::admin::goal_mutation::Op::Replace as i32,
                objective: objective.to_string(),
                token_budget_present: token_budget.is_some(),
                token_budget: token_budget.unwrap_or(0),
                status: String::new(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.SetGoal{Replace}", &s))?
            .into_inner())
    }

    /// Change goal status. Accepted values: `active` (resume), `paused`,
    /// `complete`. Daemon rejects illegal transitions.
    pub async fn set_goal_status(&mut self, session_id: &str, status: &str) -> Result<GoalDetail> {
        Ok(self
            .admin
            .set_goal(GoalMutation {
                session_id: session_id.to_string(),
                op: protocols::admin::goal_mutation::Op::SetStatus as i32,
                objective: String::new(),
                token_budget_present: false,
                token_budget: 0,
                status: status.to_string(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.SetGoal{SetStatus}", &s))?
            .into_inner())
    }

    /// Delete the goal row. No-op when none exists.
    pub async fn clear_goal(&mut self, session_id: &str) -> Result<()> {
        self.admin
            .clear_goal(GoalRef {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.ClearGoal", &s))?;
        Ok(())
    }

    /// Flip an Active goal to Paused — wired to `/cancel` so an in-flight
    /// turn aborted by the operator also pauses the Ralph loop. Returns
    /// `present=false` on the detail when there was nothing to pause
    /// (matches the no-op semantics on the server side).
    pub async fn pause_goal_on_interrupt(&mut self, session_id: &str) -> Result<GoalDetail> {
        Ok(self
            .admin
            .pause_goal_on_interrupt(GoalRef {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|s| goal_rpc_err("Admin.PauseGoalOnInterrupt", &s))?
            .into_inner())
    }

    /// Subscribe to goal lifecycle events. Caller drains the stream;
    /// dropping it cleans up the daemon-side subscriber.
    pub async fn subscribe_goal_events(&mut self) -> Result<Streaming<PbGoalEvent>> {
        Ok(self
            .admin
            .subscribe_goal_events(AdminEmpty {})
            .await
            .context("Admin.SubscribeGoalEvents failed")?
            .into_inner())
    }

    /// Subscribe to tree-lifecycle events. Caller drains the stream;
    /// dropping it cleans up the daemon-side subscriber.
    pub async fn subscribe_agent_tree_events(
        &mut self,
        session_id: &str,
    ) -> Result<Streaming<PbAgentTreeEvent>> {
        Ok(self
            .admin
            .subscribe_agent_tree_events(SessionRef {
                id: session_id.to_string(),
            })
            .await
            .context("Admin.SubscribeAgentTreeEvents failed")?
            .into_inner())
    }

    /// Subscribe to heartbeat lifecycle events. Caller drains the stream;
    /// dropping it cleans up the daemon-side subscriber.
    pub async fn subscribe_heartbeat_events(&mut self) -> Result<Streaming<PbHeartbeatEvent>> {
        Ok(self
            .admin
            .subscribe_heartbeat_events(AdminEmpty {})
            .await
            .context("Admin.SubscribeHeartbeatEvents failed")?
            .into_inner())
    }

    /// Drop a previously-set doctor squelch. Returns whether a row was
    /// removed (`false` = no active squelch existed for this pair).
    pub async fn doctor_squelch_clear(&mut self, category: &str, name: &str) -> Result<bool> {
        Ok(self
            .admin
            .doctor_squelch_clear(DoctorSquelchClearRequest {
                category: category.to_string(),
                name: name.to_string(),
            })
            .await
            .context("Admin.DoctorSquelchClear failed")?
            .into_inner()
            .removed)
    }

    /// Clear all cooled credentials. Returns the count cleared (does not
    /// touch removed entries).
    pub async fn reset_credentials(&mut self, provider: &str) -> Result<u64> {
        Ok(self
            .admin
            .reset_credentials(ResetCredentialsRequest {
                provider: provider.to_string(),
            })
            .await
            .context("Admin.ResetCredentials failed")?
            .into_inner()
            .cleared)
    }

    /// Pair an OAuth credential. Runs the discoverer for `kind` against the
    /// daemon's home directory and seeds whatever it finds. Returns one entry
    /// per detected account (empty if the on-disk store is missing).
    pub async fn pair_credential(&mut self, kind: &str) -> Result<Vec<CredentialEntry>> {
        Ok(self
            .admin
            .pair_credential(protocols::admin::PairCredentialRequest {
                kind: kind.to_string(),
            })
            .await
            .context("Admin.PairCredential failed")?
            .into_inner()
            .entries)
    }
}

rpc_wrappers! {
    // ── PKI (no context literal — errors surface the raw tonic Status) ────

    /// Mint a fresh pairing code on the daemon. Returns the plaintext code
    /// and its expiry. The code is shown to the operator and used by a
    /// separate `borg pair <code>` invocation that drives the SPAKE2 exchange
    /// over the bootstrap UDS.
    pki fn start_pairing(device_label: &str) -> StartPairingResponse =
        start_pairing(StartPairingRequest {
            device_label: device_label.to_string(),
        });

    pki fn revoke_device(fingerprint: &str, reason: &str) =
        revoke_device(RevokeDeviceRequest {
            fingerprint: fingerprint.to_string(),
            reason: reason.to_string(),
        });

    pki fn list_paired_devices() -> PairedDeviceList =
        list_paired_devices(PkiEmpty {});

    pki fn verify_audit_chain() -> VerifyAuditResponse =
        verify_audit_chain(PkiEmpty {});

    /// Ask the daemon whether the calling client's mTLS cert is within the
    /// 14-day silent-renewal window. Cheap call (single indexed lookup).
    pki fn get_cert_lifecycle() -> CertLifecycleResponse =
        get_cert_lifecycle(PkiEmpty {});

    /// Ask the daemon to mint a fresh client cert for `fingerprint` (must
    /// match the connection's principal — daemon enforces). Returned PEM
    /// is the caller's responsibility to persist into the OS keychain.
    pki fn renew_cert(fingerprint: &str) -> RenewCertResponse =
        renew_cert(RenewCertRequest {
            fingerprint: fingerprint.to_string(),
        });

    // ── Session lifecycle ──────────────────────────────────────────────────

    /// Cancel the in-flight turn for `session_id`. No-op if the session is
    /// idle.
    session fn cancel(session_id: &str) =
        cancel("Session.Cancel", CancelRequest {
            session_id: session_id.to_string(),
        });

    /// Close a session. Returns `Err` if the id is unknown.
    session fn close_session(session_id: &str) =
        close("Session.Close", CloseRequest {
            session_id: session_id.to_string(),
        });

    // ── Admin ──────────────────────────────────────────────────────────────

    /// Trigger an immediate heartbeat. Bypasses quiet hours per the in-process
    /// `/poke` semantics.
    admin fn poke() = poke("Admin.Poke", AdminEmpty {});

    /// Trigger a specific heartbeat source on the daemon.
    admin fn trigger_heartbeat(source: &str, bypass_quiet_hours: bool) =
        trigger_heartbeat("Admin.TriggerHeartbeat", TriggerHeartbeatRequest {
            source: source.to_string(),
            bypass_quiet_hours,
        });

    /// Start (or restart) the daemon's public-internet tunnel using the
    /// `tunnel.*` settings. Returns the post-apply snapshot, with the
    /// public URL when the start succeeded and `error` populated otherwise.
    admin fn tunnel_start() -> TunnelStatusReply =
        tunnel_start("Admin.TunnelStart", AdminEmpty {});

    /// Stop the daemon's tunnel. Idempotent — no error when no tunnel runs.
    admin fn tunnel_stop() = tunnel_stop("Admin.TunnelStop", AdminEmpty {});

    /// Manage the browser CDP attach setting. `action` is one of
    /// `"connect"`, `"disconnect"`, `"status"`; `url` is only honored for
    /// `"connect"` and may be empty to use the default.
    admin fn browser_manage(action: &str, url: Option<&str>) -> BrowserManageResponse =
        browser_manage("Admin.BrowserManage", BrowserManageRequest {
            action: action.to_string(),
            url: url.unwrap_or("").to_string(),
        });

    /// Snapshot of the daemon's tunnel state.
    admin fn tunnel_status() -> TunnelStatusReply =
        tunnel_status("Admin.TunnelStatus", AdminEmpty {});

    /// Run a daily-maintenance sweep on the daemon. Same code path as the
    /// nightly scheduled task and the in-process `/heal`.
    admin fn heal() -> HealReport = heal("Admin.Heal", AdminEmpty {});

    /// Hot-reload MCP servers and/or user skills on the daemon without restart.
    /// `target` selects the subsystem; `0`/`Unspecified` means everything.
    admin fn reload(target: i32) -> ReloadResponse =
        reload("Admin.Reload", ReloadRequest { target });

    /// Run memory consolidation on the daemon.
    admin fn dream() -> DreamReport = dream("Admin.Dream", AdminEmpty {});

    /// Get every setting (db + default).
    admin fn get_settings() -> SettingsSnapshot =
        get_settings("Admin.GetSettings", AdminEmpty {});

    /// Set a setting value (validated and persisted to the daemon's DB).
    admin fn set_setting(key: &str, value: &str) =
        set_setting("Admin.SetSetting", SettingMutation {
            key: key.to_string(),
            value: value.to_string(),
        });

    /// Remove a setting override (revert to default).
    admin fn unset_setting(key: &str) =
        unset_setting("Admin.UnsetSetting", SettingKey {
            key: key.to_string(),
        });

    /// List recent sessions. Live sessions are tagged `(live)` in the title.
    admin fn list_sessions() -> SessionList =
        list_sessions("Admin.ListSessions", AdminEmpty {});

    /// Fetch a single session's transcript.
    admin fn get_session(id: &str) -> SessionDetail =
        get_session("Admin.GetSession", SessionRef { id: id.to_string() });

    // ── Session-state RPCs (replace direct agent calls in TUI) ────────────

    /// Snapshot of agent state for the given session_id.
    session fn get_state(session_id: &str) -> SessionState =
        get_state("Session.GetState", SessionStateRequest {
            session_id: session_id.to_string(),
        });

    /// Compact the agent's history.
    session fn compact(session_id: &str) -> CompactResponse =
        compact("Session.Compact", CompactRequest {
            session_id: session_id.to_string(),
        });

    /// Wipe history.
    session fn clear_history(session_id: &str) =
        clear_history("Session.ClearHistory", ClearRequest {
            session_id: session_id.to_string(),
        });

    /// Drop the most recent assistant turn.
    session fn undo(session_id: &str) -> UndoResponse =
        undo("Session.Undo", UndoRequest {
            session_id: session_id.to_string(),
        });

    /// Rewind to the Nth user message.
    session fn rewind(session_id: &str, n: u64) -> RewindResponse =
        rewind_to_nth_user_message("Session.RewindToNthUserMessage", RewindRequest {
            session_id: session_id.to_string(),
            n,
        });

    /// Persist the agent's current state.
    session fn auto_save(session_id: &str) =
        auto_save("Session.AutoSave", AutoSaveRequest {
            session_id: session_id.to_string(),
        });

    /// Start a fresh agent session inside the existing host.
    session fn new_agent_session(session_id: &str) -> NewSessionResponse =
        new_session("Session.NewSession", NewSessionRequest {
            session_id: session_id.to_string(),
        });

    /// Load an existing agent session by id inside the existing host.
    session fn load_agent_session(session_id: &str, agent_session_id: &str) -> LoadSessionResponse =
        load_session("Session.LoadSession", LoadSessionRequest {
            session_id: session_id.to_string(),
            agent_session_id: agent_session_id.to_string(),
        });

    /// Push a steer message into the in-flight turn.
    session fn steer(session_id: &str, text: &str) =
        steer("Session.Steer", SteerRequest {
            session_id: session_id.to_string(),
            text: text.to_string(),
        });

    /// Mirror a system-injected assistant message (e.g. a heartbeat the
    /// daemon streamed to this TUI) into the active session's history,
    /// so the next user reply has the message in context.
    session fn append_assistant_message(session_id: &str, content: &str) =
        append_assistant_message("Session.AppendAssistantMessage", AppendAssistantMessageRequest {
            session_id: session_id.to_string(),
            content: content.to_string(),
        });

    /// Fork the active agent session at its last message. Daemon-side
    /// execution — the TUI must NOT open a SQLite writer to do this itself,
    /// since `borgd` is the single writer (CLAUDE.md "DB writer split-brain").
    session fn fork_session(session_id: &str) -> ForkResponse =
        fork("Session.Fork", ForkRequest {
            session_id: session_id.to_string(),
        });

    // ── Scheduled tasks ───────────────────────────────────────────────────

    /// List all scheduled tasks.
    admin fn list_scheduled_tasks() -> ScheduledTaskList =
        list_scheduled_tasks("Admin.ListScheduledTasks", AdminEmpty {});

    admin fn create_scheduled_task(spec: ScheduledTaskCreate) -> ScheduledTask =
        create_scheduled_task("Admin.CreateScheduledTask", spec);

    /// Delete a scheduled task by id.
    admin fn delete_scheduled_task(id: &str) =
        delete_scheduled_task("Admin.DeleteScheduledTask", ScheduledTaskRef {
            id: id.to_string(),
        });

    /// Pause a scheduled task.
    admin fn pause_scheduled_task(id: &str) =
        pause_scheduled_task("Admin.PauseScheduledTask", ScheduledTaskRef {
            id: id.to_string(),
        });

    /// Resume a paused scheduled task.
    admin fn resume_scheduled_task(id: &str) =
        resume_scheduled_task("Admin.ResumeScheduledTask", ScheduledTaskRef {
            id: id.to_string(),
        });

    /// Queue a scheduled task for immediate execution.
    admin fn run_scheduled_task_now(id: &str) =
        run_scheduled_task_now("Admin.RunScheduledTaskNow", ScheduledTaskRef {
            id: id.to_string(),
        });

    /// Fetch the most recent run history for a scheduled task.
    admin fn get_scheduled_task_runs(id: &str, limit: u32) -> ScheduledTaskRunList =
        get_scheduled_task_runs("Admin.GetScheduledTaskRuns", GetScheduledTaskRunsRequest {
            id: id.to_string(),
            limit,
        });

    /// Detailed view of a scheduled task: full row + last run summary.
    admin fn get_scheduled_task_detail(id: &str) -> ScheduledTaskDetail =
        get_scheduled_task_detail("Admin.GetScheduledTaskDetail", ScheduledTaskRef {
            id: id.to_string(),
        });

    /// Render a session via the daemon's export pipeline.
    admin fn export_session(session_id: &str, format: &str) -> ExportSessionResponse =
        export_session("Admin.ExportSession", ExportSessionRequest {
            session_id: session_id.to_string(),
            format: format.to_string(),
        });

    // ── Pairing ───────────────────────────────────────────────────────────

    /// List pairing requests, optionally filtered by channel.
    admin fn list_pairing_requests(channel: Option<&str>) -> PairingRequestList =
        list_pairing_requests("Admin.ListPairingRequests", PairingFilter {
            channel_name: channel.unwrap_or("").to_string(),
        });

    /// Mark `session_id` as pending handoff to `target_platform`. The
    /// destination gateway picks the row up on its next claim sweep.
    admin fn request_handoff(session_id: &str, target_platform: &str) =
        request_handoff("Admin.RequestHandoff", AdminHandoffRequest {
            session_id: session_id.to_string(),
            target_platform: target_platform.to_string(),
        });

    /// Mark a running handoff as completed.
    admin fn complete_handoff(session_id: &str) =
        complete_handoff("Admin.CompleteHandoff", SessionRef {
            id: session_id.to_string(),
        });

    /// Mark a pending/running handoff as failed with a human-readable reason.
    admin fn fail_handoff(session_id: &str, reason: &str) =
        fail_handoff("Admin.FailHandoff", HandoffFailRequest {
            session_id: session_id.to_string(),
            reason: reason.to_string(),
        });

    /// Fetch the current handoff status for a session.
    admin fn get_handoff_status(session_id: &str) -> HandoffStatusReply =
        get_handoff_status("Admin.GetHandoffStatus", SessionRef {
            id: session_id.to_string(),
        });

    /// Approve a pairing request by code.
    admin fn approve_pairing(channel: &str, code: &str) =
        approve_pairing("Admin.ApprovePairing", PairingApproval {
            channel_name: channel.to_string(),
            code: code.to_string(),
        });

    /// List approved senders, optionally filtered by channel.
    admin fn list_approved_senders(channel: Option<&str>) -> ApprovedSenderList =
        list_approved_senders("Admin.ListApprovedSenders", PairingFilter {
            channel_name: channel.unwrap_or("").to_string(),
        });

    /// Revoke an approved sender.
    admin fn revoke_approved_sender(channel: &str, sender_id: &str) =
        revoke_approved_sender("Admin.RevokeApprovedSender", ApprovedSenderRef {
            channel_name: channel.to_string(),
            sender_id: sender_id.to_string(),
        });

    /// Update a scheduled task. Empty-string fields are left unchanged
    /// (mirrors `cortex::db::UpdateTask`'s `Option<&str>` semantics).
    admin fn update_scheduled_task(spec: ScheduledTaskUpdate) -> ScheduledTask =
        update_scheduled_task("Admin.UpdateScheduledTask", spec);

    /// Ambient status (the `class:` line in the TUI header).
    admin fn get_ambient_status() -> AmbientStatus =
        get_ambient_status("Admin.GetAmbientStatus", AdminEmpty {});

    /// Snapshot of running + recently-finished subprocesses tracked by the
    /// daemon's `ProcessRegistry`. Empty when the daemon has no registry
    /// wired (older daemon builds, test harnesses without the registry).
    admin fn get_subprocesses() -> SubprocessSnapshot =
        get_subprocesses("Admin.GetSubprocesses", AdminEmpty {});

    /// Toggle the auto-reap pin on a specific subprocess entry. Pinning a
    /// running workload exempts it from the daily maintenance sweep's
    /// 24h auto-kill and downgrades its doctor surface.
    admin fn set_subprocess_pinned(id: u64, pinned: bool) =
        set_subprocess_pinned("Admin.SetSubprocessPinned", SetSubprocessPinnedRequest {
            id,
            pinned,
        });

    /// Latest doctor-run persistent warnings.
    admin fn get_persistent_warnings() -> WarningList =
        get_persistent_warnings("Admin.GetPersistentWarnings", AdminEmpty {});

    // ── Status ────────────────────────────────────────────────────────────

    /// Vitals snapshot — already decay-applied, with optional drift_notice.
    status fn get_vitals() -> Vitals = get_vitals("Status.GetVitals", StatusEmpty {});

    /// Evolution state snapshot (replay-derived).
    status fn get_evolution() -> EvolutionStateProto =
        get_evolution("Status.GetEvolution", StatusEmpty {});

    /// Bond state snapshot.
    status fn get_bond_state() -> BondStateProto =
        get_bond_state("Status.GetBondState", StatusEmpty {});

    /// Bond events since a unix timestamp (DESC).
    status fn get_bond_events_since(since_unix: i64) -> BondEventList =
        get_bond_events_since("Status.GetBondEventsSince", Int64Cursor { since_unix });

    /// Evolution events since a unix timestamp (DESC).
    status fn get_evolution_events_since(since_unix: i64) -> EvolutionEventList =
        get_evolution_events_since("Status.GetEvolutionEventsSince", Int64Cursor { since_unix });

    /// Vitals events since a unix timestamp (DESC).
    status fn get_vitals_events_since(since_unix: i64) -> VitalsEventList =
        get_vitals_events_since("Status.GetVitalsEventsSince", Int64Cursor { since_unix });

    /// Pre-rendered text for the status popup's Overview tab. `card_width` is
    /// the popup's inner width (passed verbatim to the daemon's evolution-card
    /// formatter); pass 0 for a sane default.
    status fn get_status_overview_text(card_width: u32) -> StatusText =
        get_status_overview_text("Status.GetStatusOverviewText", StatusTextRequest { card_width });

    /// Pre-rendered `/evolution` text for the status popup's Evolution tab.
    status fn get_evolution_text() -> StatusText =
        get_evolution_text("Status.GetEvolutionText", StatusEmpty {});

    /// Pre-rendered `/xp` text for the status popup's XP tab.
    status fn get_xp_text() -> StatusText =
        get_xp_text("Status.GetXpText", StatusEmpty {});

    /// Pre-rendered archetype-scores text for the status popup.
    status fn get_archetype_scores_text() -> StatusText =
        get_archetype_scores_text("Status.GetArchetypeScoresText", StatusEmpty {});

    /// Pre-rendered evolution-history text for the status popup.
    status fn get_evolution_history_text() -> StatusText =
        get_evolution_history_text("Status.GetEvolutionHistoryText", StatusEmpty {});

    /// Pre-rendered `/usage` text for the usage popup. `days = 0`
    /// asks the daemon to use its default window (30); the daemon clamps
    /// large values to 365 — the cli does not validate.
    status fn get_usage_text(days: u32) -> StatusText =
        get_usage_text("Status.GetUsageText", UsageRequest { days });

    /// XP snapshot (`/xp` data).
    status fn get_xp() -> XpSnapshot = get_xp("Status.GetXp", StatusEmpty {});

    /// Ask the daemon to shut down gracefully. Returns once the daemon has
    /// acknowledged the request — the daemon's drain loop runs in the
    /// background. Subsequent `borg` invocations re-spawn the daemon.
    admin fn shutdown() = shutdown("Admin.Shutdown", AdminEmpty {});

    /// Generate + deliver a personalized greeting for a newly approved sender.
    admin fn send_approval_greeting(channel: &str, sender_id: &str) =
        send_approval_greeting("Admin.SendApprovalGreeting", ApprovalGreetingRequest {
            channel: channel.to_string(),
            sender_id: sender_id.to_string(),
        });

    // -- Sub-agent control (`/agents`) --------------------------------------

    /// List sub-agents owned by `session_id`. Empty `session_id` resolves to
    /// the most-recently active session daemon-side.
    admin fn list_agents(session_id: &str) -> PbAgentList =
        list_agents("Admin.ListAgents", AgentListRequest {
            session_id: session_id.to_string(),
        });

    /// Kill a single sub-agent.
    admin fn kill_agent(agent_id: &str) =
        kill_agent("Admin.KillAgent", PbAgentRef {
            agent_id: agent_id.to_string(),
        });

    /// Kill an agent and every descendant in its subtree.
    admin fn kill_agent_subtree(agent_id: &str) =
        kill_agent_subtree("Admin.KillAgentSubtree", PbAgentRef {
            agent_id: agent_id.to_string(),
        });

    /// Pause an agent at its next iteration boundary.
    admin fn pause_agent(agent_id: &str) =
        pause_agent("Admin.PauseAgent", PbAgentRef {
            agent_id: agent_id.to_string(),
        });

    /// Resume a paused agent.
    admin fn resume_agent(agent_id: &str) =
        resume_agent("Admin.ResumeAgent", PbAgentRef {
            agent_id: agent_id.to_string(),
        });

    /// List persisted spawn-tree snapshots for a session.
    admin fn list_agent_snapshots(session_id: &str) -> AgentSnapshotList =
        list_agent_snapshots("Admin.ListAgentSnapshots", SessionRef {
            id: session_id.to_string(),
        });

    /// Fetch the agents captured in one persisted snapshot.
    admin fn get_agent_snapshot(session_id: &str, turn_id: &str) -> PbAgentList =
        get_agent_snapshot("Admin.GetAgentSnapshot", AgentSnapshotRefRequest {
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
        });

    /// Append a row to `activity_log` via the daemon (avoids opening the DB
    /// from the cli).
    admin fn log_activity(level: &str, source: &str, message: &str) =
        log_activity("Admin.LogActivity", LogActivityRequest {
            level: level.to_string(),
            source: source.to_string(),
            message: message.to_string(),
        });

    /// Pre-rendered activity-log tail. `min_level` accepts the same level
    /// strings as `cortex::db::query_activity` (`"error"`, `"warn"`,
    /// `"info"`, `"debug"`, `"all"`); empty falls back to `"info"`.
    admin fn get_activity_log_text(count: u32, min_level: &str, category: &str) -> ActivityLogText =
        get_activity_log_text("Admin.GetActivityLogText", ActivityLogQuery {
            count,
            min_level: min_level.to_string(),
            category: category.to_string(),
        });

    /// Persist plugin install metadata + file hashes in a single round-trip.
    admin fn plugin_install(req: PluginInstallRequest) -> PluginInstallResponse =
        plugin_install("Admin.PluginInstall", req);

    // ── API key routing ──────────────────────────────────────────────────

    /// Fetch the routing metadata for a provider's API key. Returns an
    /// `ApiKeyValue` whose `body` is one of `literal`/`keychain`/`unset`.
    /// The cli does the keychain lookup locally — the daemon only stores
    /// the routing.
    admin fn get_api_key(provider: &str) -> ApiKeyValue =
        get_api_key("Admin.GetApiKey", ApiKeyRef {
            provider: provider.to_string(),
        });

    /// Persist API-key routing metadata for `provider`. The body must be a
    /// `literal` or `keychain` ref — `unset` is rejected here; use
    /// [`unset_api_key`](Self::unset_api_key) to clear.
    admin fn set_api_key(value: ApiKeyValue) =
        set_api_key("Admin.SetApiKey", value);

    /// Clear the stored routing metadata for `provider`. The cli is
    /// responsible for deleting any companion keychain entry.
    admin fn unset_api_key(provider: &str) =
        unset_api_key("Admin.UnsetApiKey", ApiKeyRef {
            provider: provider.to_string(),
        });

    /// List every provider with routing metadata stored, with the backend
    /// kind ("literal" or "keychain") so the cli can render which backend
    /// is in use without leaking the secret.
    admin fn list_api_keys() -> ApiKeyList =
        list_api_keys("Admin.ListApiKeys", AdminEmpty {});

    // ── Onboarding ───────────────────────────────────────────────────────

    /// Read the `onboarded` sentinel from the daemon's settings table.
    admin fn get_onboarding_state() -> OnboardingState =
        get_onboarding_state("Admin.GetOnboardingState", AdminEmpty {});

    /// Mark first-run onboarding complete (`onboarded=true` in settings).
    admin fn set_onboarding_complete() =
        set_onboarding_complete("Admin.SetOnboardingComplete", AdminEmpty {});

    /// Monthly token total across `token_usage` rows in the current calendar
    /// month (UTC). Used by the TUI `/usage` view.
    admin fn get_monthly_token_total() -> MonthlyTokenTotal =
        get_monthly_token_total("Admin.GetMonthlyTokenTotal", AdminEmpty {});

    /// Monthly cost total. `present=false` means there are no rows this
    /// month — caller should hide the line rather than render `$0.00`.
    admin fn get_monthly_total_cost() -> MonthlyTotalCost =
        get_monthly_total_cost("Admin.GetMonthlyTotalCost", AdminEmpty {});

    /// Daemon-wide status snapshot.
    admin fn get_daemon_status() -> DaemonStatus =
        get_daemon_status("Admin.GetDaemonStatus", AdminEmpty {});

    // ── Provider / webhook / client-token writes ──────────────────────────

    /// Insert a BYO provider row. Reads stay on `Database::open_readonly`.
    admin fn add_provider(
        id: &str,
        display_name: &str,
        base_url: &str,
        api_key_env: Option<&str>,
        category: &str,
    ) = add_provider("Admin.AddProvider", AddProviderRequest {
        id: id.to_string(),
        display_name: display_name.to_string(),
        base_url: base_url.to_string(),
        api_key_env: api_key_env.unwrap_or("").to_string(),
        category: category.to_string(),
    });

    /// Remove a BYO provider row by id. Idempotent — succeeds even if the id
    /// is already absent.
    admin fn remove_provider(id: &str) =
        remove_provider("Admin.RemoveProvider", RemoveProviderRequest {
            id: id.to_string(),
        });

    /// Create a webhook definition; returns the raw HMAC secret bytes —
    /// the caller is responsible for printing the hex once and discarding.
    admin fn create_webhook(
        name: &str,
        sender_path: &str,
        thread_path: Option<&str>,
        body_path: &str,
    ) -> CreateWebhookResponse = create_webhook("Admin.CreateWebhook", CreateWebhookRequest {
        name: name.to_string(),
        sender_path: sender_path.to_string(),
        thread_path: thread_path.unwrap_or("").to_string(),
        body_path: body_path.to_string(),
    });

    /// Mark a webhook revoked. Idempotent.
    admin fn revoke_webhook(name: &str) =
        revoke_webhook("Admin.RevokeWebhook", WebhookRef {
            name: name.to_string(),
        });

    /// Issue a bearer client token; the plaintext is returned exactly once.
    admin fn issue_client_token(label: &str, scopes: &str) -> IssueClientTokenResponse =
        issue_client_token("Admin.IssueClientToken", IssueClientTokenRequest {
            label: label.to_string(),
            scopes: scopes.to_string(),
        });

    /// Revoke a bearer token by label.
    admin fn revoke_client_token(label: &str) -> RevokedClientToken =
        revoke_client_token("Admin.RevokeClientToken", ClientTokenRef {
            label: label.to_string(),
        });

    /// Squelch a doctor finding `(category, name)` until `expires_at_unix`.
    /// Re-squelching an existing pair refreshes the timestamps + reason.
    admin fn doctor_squelch_set(
        category: &str,
        name: &str,
        expires_at_unix: i64,
        reason: Option<&str>,
    ) = doctor_squelch_set("Admin.DoctorSquelchSet", DoctorSquelchSetRequest {
        category: category.to_string(),
        name: name.to_string(),
        expires_at_unix,
        reason: reason.unwrap_or("").to_string(),
    });

    // ── Credential pool ────────────────────────────────────────────────

    /// List credential pools. `provider` empty = all configured providers.
    admin fn list_credentials(provider: Option<&str>) -> CredentialList =
        list_credentials("Admin.ListCredentials", ListCredentialsRequest {
            provider: provider.unwrap_or("").to_string(),
        });

    /// Add a credential to a pool. Returns the persisted entry with its
    /// fingerprint (plaintext key is not echoed back).
    admin fn add_credential(provider: &str, key: &str, label: Option<&str>) -> CredentialEntry =
        add_credential("Admin.AddCredential", AddCredentialRequest {
            provider: provider.to_string(),
            key: key.to_string(),
            label: label.unwrap_or("").to_string(),
        });

    /// Remove a credential (soft-delete + write suppression row).
    admin fn remove_credential(provider: &str, target: &str) -> CredentialEntry =
        remove_credential("Admin.RemoveCredential", RemoveCredentialRequest {
            provider: provider.to_string(),
            target: target.to_string(),
        });

    /// Change the selection strategy.
    admin fn set_credential_strategy(provider: &str, strategy: &str) =
        set_credential_strategy("Admin.SetCredentialStrategy", SetCredentialStrategyRequest {
            provider: provider.to_string(),
            strategy: strategy.to_string(),
        });
}

#[cfg(test)]
mod goal_rpc_err_tests {
    use super::goal_rpc_err;

    #[test]
    fn preserves_daemon_message_for_user_visibility() {
        // The bug we're guarding against: `.context("Admin.SetGoal{Replace}
        // failed")?` chained the tonic Status inside anyhow but only the
        // outer label rendered in the TUI footer. Users saw the opaque
        // "Admin.SetGoal{Replace} failed" instead of the actual reason.
        let status =
            tonic::Status::failed_precondition("set_goal: no active session — start a turn first");
        let err = goal_rpc_err("Admin.SetGoal{Replace}", &status);
        let display = format!("{err}");
        assert!(
            display.contains("Admin.SetGoal{Replace}"),
            "must keep the operation label, got: {display}"
        );
        assert!(
            display.contains("no active session"),
            "must include the daemon's specific message, got: {display}"
        );
        assert!(
            display.contains("FailedPrecondition"),
            "must include the gRPC code so callers can distinguish \
             transient vs terminal failures, got: {display}"
        );
    }

    #[test]
    fn includes_code_when_message_empty() {
        let status = tonic::Status::new(tonic::Code::Unavailable, "");
        let err = goal_rpc_err("Admin.GetGoal", &status);
        let display = format!("{err}");
        assert!(display.contains("Admin.GetGoal"));
        assert!(
            display.contains("Unavailable"),
            "must include the code, got: {display}"
        );
    }
}
