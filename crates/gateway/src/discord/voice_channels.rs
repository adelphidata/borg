//! Discord voice channel manager — `discord-voice-channels` feature only.
//!
//! Bridges Discord's two-stage voice protocol:
//!
//! 1. **Gateway WSS** (`wss://gateway.discord.gg`): hand-rolled here.
//!    HELLO → IDENTIFY → heartbeat loop → listen for `VOICE_STATE_UPDATE`
//!    (gives session_id) and `VOICE_SERVER_UPDATE` (gives endpoint + token).
//!    To join a channel we send op-4 `VOICE_STATE_UPDATE` with the target
//!    guild_id + channel_id; the server-side responses populate the
//!    `ConnectionInfo` we need for step 2.
//!
//! 2. **Voice Gateway + UDP RTP**: songbird's `Driver` owns this entirely
//!    once we hand it the `ConnectionInfo`. Songbird does the Opus codec,
//!    libsodium encryption, IP discovery, and RTP send/recv.
//!
//! `join_voice_channel` + `leave_voice_channel` + `play_in_voice_channel`.
//! Inbound voice (transcribing other speakers in the channel) is a follow-up
//! — songbird's `receive` feature + per-user RTP routing isn't wired here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::num::NonZeroU64;

use cortex::config::Config as CortexConfig;
use songbird::driver::Driver;
use songbird::events::{Event, EventContext, EventHandler};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::{Config as SongbirdConfig, ConnectionInfo};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// One Discord Gateway connection identified by bot token + intents.
/// Required intents: GUILDS (1) + GUILD_VOICE_STATES (128) = 129.
const REQUIRED_INTENTS: u64 = 1 | (1 << 7);

/// Discord Gateway entry URL with API version 10 and JSON encoding.
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Operator API to drive the voice manager from the /voice command path.
#[derive(Debug)]
pub enum VoiceCommand {
    /// Join the given voice channel. Reply: `Ok(())` or descriptive error.
    Join {
        guild_id: u64,
        channel_id: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Leave the active voice channel for a guild.
    Leave {
        guild_id: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Play an audio file (any format ffmpeg/symphonia can read) in the
    /// active voice channel. Failure if the bot isn't joined.
    Play {
        guild_id: u64,
        path: std::path::PathBuf,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Query whether the bot is currently in a voice channel for the guild.
    IsConnected {
        guild_id: u64,
        reply: oneshot::Sender<bool>,
    },
    /// Look up the voice channel a user is currently in (guild-member
    /// cache populated from VOICE_STATE_UPDATE events). Used by
    /// `/voice join` to auto-resolve the caller's channel.
    ResolveUserChannel {
        guild_id: u64,
        user_id: u64,
        reply: oneshot::Sender<Option<u64>>,
    },
    /// Drain any pending inbound transcripts from speakers in the voice
    /// channel. Returns the transcripts collected since the last drain;
    /// the caller decides whether to feed them to the agent.
    DrainTranscripts {
        guild_id: u64,
        reply: oneshot::Sender<Vec<String>>,
    },
}

/// Public handle returned from `spawn_voice_manager` — clones cheap.
#[derive(Clone)]
pub struct VoiceManagerHandle {
    tx: mpsc::Sender<VoiceCommand>,
}

/// Test-only constructor: wraps an arbitrary command channel so in-crate
/// unit tests can service [`VoiceCommand`]s with a stub task instead of a
/// live voice manager.
#[cfg(test)]
pub(crate) fn handle_for_tests(tx: mpsc::Sender<VoiceCommand>) -> VoiceManagerHandle {
    VoiceManagerHandle { tx }
}

impl VoiceManagerHandle {
    pub async fn join(&self, guild_id: u64, channel_id: u64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VoiceCommand::Join {
                guild_id,
                channel_id,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("voice manager task is down"))?;
        rx.await
            .map_err(|_| anyhow!("voice manager dropped reply"))?
    }

    pub async fn leave(&self, guild_id: u64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VoiceCommand::Leave {
                guild_id,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("voice manager task is down"))?;
        rx.await
            .map_err(|_| anyhow!("voice manager dropped reply"))?
    }

    pub async fn play(&self, guild_id: u64, path: std::path::PathBuf) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VoiceCommand::Play {
                guild_id,
                path,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("voice manager task is down"))?;
        rx.await
            .map_err(|_| anyhow!("voice manager dropped reply"))?
    }

    pub async fn is_connected(&self, guild_id: u64) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(VoiceCommand::IsConnected {
                guild_id,
                reply: tx,
            })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Look up the voice channel a user is currently in. None if the bot
    /// hasn't seen a VOICE_STATE_UPDATE for them yet, or they're not in
    /// any voice channel right now.
    pub async fn resolve_user_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VoiceCommand::ResolveUserChannel {
                guild_id,
                user_id,
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Drain transcripts produced by inbound speakers since the last
    /// drain. Empty when no one has been talking or STT failed.
    pub async fn drain_transcripts(&self, guild_id: u64) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(VoiceCommand::DrainTranscripts {
                guild_id,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }
}

/// Process-global handle. `init_voice_manager` installs the handle once at
/// gateway startup; `voice_manager_handle()` returns a clone for command
/// dispatch sites (handle_voice). Returning None after init failed is the
/// signal that the feature is enabled but no Discord bot token was
/// configured — the /voice join path falls back to its stub message.
static VOICE_MANAGER: std::sync::OnceLock<VoiceManagerHandle> = std::sync::OnceLock::new();

/// Install the process-global voice manager. Idempotent — repeat calls
/// after the first return without spawning again. The `cortex_config` is
/// forwarded so the inbound event handler can spin up an `AudioTranscriber`
/// for live STT against decoded speaker PCM.
pub fn init_voice_manager(bot_token: String, cortex_config: CortexConfig) {
    // Discard the Err: a second call means the manager is already installed.
    let _ = VOICE_MANAGER.set(spawn_voice_manager(bot_token, cortex_config));
}

/// Look up the installed manager handle for command dispatch.
pub fn voice_manager_handle() -> Option<&'static VoiceManagerHandle> {
    VOICE_MANAGER.get()
}

/// Spawn the long-running Discord voice manager task. Returns a handle the
/// gateway uses to dispatch /voice commands.
///
/// The task owns:
/// - the Discord Gateway WSS connection (hand-rolled),
/// - a `songbird::Driver` per guild we've joined,
/// - pending `VOICE_STATE_UPDATE` + `VOICE_SERVER_UPDATE` correlation so
///   that a `Join` request awaits both half-events before connecting.
///
/// The bot token must come from `gateway.discord_bot_token` config; the
/// caller passes it in pre-resolved.
pub fn spawn_voice_manager(bot_token: String, cortex_config: CortexConfig) -> VoiceManagerHandle {
    let (tx, rx) = mpsc::channel::<VoiceCommand>(32);
    tokio::spawn(async move {
        if let Err(e) = run_voice_manager(bot_token, rx, cortex_config, GATEWAY_URL).await {
            warn!(error = %e, "Discord voice manager exited");
        }
    });
    VoiceManagerHandle { tx }
}

/// Inbound transcripts collected by the per-guild event handler, shared
/// with the manager task so `DrainTranscripts` can hand them out.
type TranscriptInbox = Arc<Mutex<Vec<String>>>;

/// Transcripts kept per guild between `DrainTranscripts` calls. Past this
/// the oldest is dropped: a channel nobody drains must not grow without
/// bound.
const MAX_INBOX_TRANSCRIPTS: usize = 256;

/// Append a transcript, evicting the oldest once the inbox is full.
async fn push_transcript(inbox: &TranscriptInbox, transcript: String) {
    let mut v = inbox.lock().await;
    if v.len() >= MAX_INBOX_TRANSCRIPTS {
        warn!(
            cap = MAX_INBOX_TRANSCRIPTS,
            "voice transcript inbox full; dropping the oldest transcript"
        );
        v.remove(0);
    }
    v.push(transcript);
}

struct GuildState {
    driver: Driver,
    /// Target voice channel for this guild's connection. songbird 0.6's
    /// `ConnectionInfo.channel_id` is mandatory (was optional in 0.4); the
    /// raw `Driver::connect` still ignores it, but we thread the real id
    /// through so it's correct rather than a placeholder.
    channel_id: u64,
    pending_session_id: Option<String>,
    pending_endpoint_token: Option<(String, String)>,
    join_replies: Vec<oneshot::Sender<Result<()>>>,
    /// Buffer of transcripts captured from other speakers in this voice
    /// channel since the last `DrainTranscripts` call.
    inbox: TranscriptInbox,
    /// The inbound speaker handler for this driver. Kept so a channel
    /// switch can clear its per-speaker buffers.
    inbound: InboundVoiceHandler,
    /// Times a handler was registered on `driver`. The driver appends
    /// global handlers rather than replacing them, so anything above one
    /// transcribes every utterance that many times.
    inbound_registrations: u32,
}

impl GuildState {
    /// Fresh driver for `channel_id` with the inbound handler registered
    /// exactly once — the only place a handler is ever added.
    fn new(channel_id: u64, cortex_config: Arc<CortexConfig>) -> Self {
        let inbox: TranscriptInbox = Arc::new(Mutex::new(Vec::new()));
        let inbound = InboundVoiceHandler::new(inbox.clone(), cortex_config);
        let mut state = Self {
            driver: Driver::new(SongbirdConfig::default()),
            channel_id,
            pending_session_id: None,
            pending_endpoint_token: None,
            join_replies: Vec::new(),
            inbox,
            inbound,
            inbound_registrations: 0,
        };
        state.register_inbound();
        state
    }

    fn register_inbound(&mut self) {
        self.driver.add_global_event(
            Event::Core(songbird::CoreEvent::SpeakingStateUpdate),
            self.inbound.clone(),
        );
        self.driver.add_global_event(
            Event::Core(songbird::CoreEvent::VoiceTick),
            self.inbound.clone(),
        );
        self.inbound_registrations += 1;
    }

    /// Drop audio and transcripts that belong to the previous channel so
    /// nothing said there surfaces after a switch, and forget cached
    /// speaker verdicts so the new channel re-reads pairing state.
    async fn clear_inbound(&self) {
        self.inbox.lock().await.clear();
        self.inbound.speakers.lock().await.clear();
        self.inbound.verdicts.lock().await.clear();
    }
}

/// Per-(guild,user) → current voice channel cache. Populated from
/// VOICE_STATE_UPDATE events for *all* users (not just our bot).
/// `None` channel means the user left voice; we keep the entry so
/// /voice join knows about them. Returned as `Some(channel_id)` only when
/// the user is currently in a channel.
type VoiceStateCache = Arc<Mutex<HashMap<(u64, u64), Option<u64>>>>;

/// `gateway_url` is [`GATEWAY_URL`] in production; parameterized so tests
/// can stand up a loopback WebSocket server instead of Discord.
async fn run_voice_manager(
    bot_token: String,
    mut cmd_rx: mpsc::Receiver<VoiceCommand>,
    cortex_config: CortexConfig,
    gateway_url: &str,
) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(gateway_url)
        .await
        .context("Discord Gateway connect")?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Read HELLO to learn heartbeat_interval.
    let hello = next_gateway_message(&mut ws_rx)
        .await
        .ok_or_else(|| anyhow!("Gateway closed before HELLO"))?;
    let hello_json: serde_json::Value =
        serde_json::from_str(&hello).context("parse HELLO frame")?;
    let heartbeat_interval = hello_json
        .get("d")
        .and_then(|d| d.get("heartbeat_interval"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("HELLO missing heartbeat_interval"))?;

    let identify = serde_json::json!({
        "op": 2,
        "d": {
            "token": bot_token,
            "intents": REQUIRED_INTENTS,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "borg",
                "device": "borg",
            },
        }
    });
    ws_tx
        .send(Message::Text(identify.to_string().into()))
        .await
        .context("send IDENTIFY")?;

    // Heartbeats go through a channel rather than straight to `ws_tx`: the
    // sink is single-owner, so the timer task must not hold it.
    let (hb_tx, mut hb_rx) = mpsc::channel::<Message>(4);
    let hb_tx_clone = hb_tx.clone();
    let last_seq: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let last_seq_hb = Arc::clone(&last_seq);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        interval.tick().await; // first tick is immediate; skip
        loop {
            interval.tick().await;
            let seq = *last_seq_hb.lock().await;
            let beat = serde_json::json!({"op": 1, "d": seq});
            if hb_tx_clone
                .send(Message::Text(beat.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Our own user_id — populated from READY.
    let our_user_id: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let guilds: Arc<Mutex<HashMap<u64, GuildState>>> = Arc::new(Mutex::new(HashMap::new()));
    let voice_states: VoiceStateCache = Arc::new(Mutex::new(HashMap::new()));
    let cortex_config = Arc::new(cortex_config);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                if let Err(e) = handle_command(
                    cmd, &mut ws_tx, &guilds, &our_user_id, &voice_states,
                    cortex_config.clone(), &bot_token,
                ).await {
                    warn!(error = %e, "voice manager command failed");
                }
            }
            hb = hb_rx.recv() => {
                if let Some(msg) = hb {
                    if let Err(e) = ws_tx.send(msg).await {
                        warn!(error = %e, "heartbeat send failed");
                        break;
                    }
                }
            }
            msg = next_gateway_message(&mut ws_rx) => {
                let Some(text) = msg else { break; };
                if let Err(e) = process_event(
                    &text, &last_seq, &our_user_id, &guilds, &voice_states,
                ).await {
                    debug!(error = %e, "process_event");
                }
            }
        }
    }

    Ok(())
}

async fn next_gateway_message<S>(ws_rx: &mut S) -> Option<String>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(t) => return Some(t.to_string()),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return None,
            Message::Frame(_) => continue,
        }
    }
    None
}

/// Every `reply.send(..)` below is discarded on purpose: the requester is a
/// `/voice` command that may have timed out and dropped its oneshot, and the
/// gateway loop must keep servicing the socket regardless.
async fn handle_command<W>(
    cmd: VoiceCommand,
    ws_tx: &mut W,
    guilds: &Arc<Mutex<HashMap<u64, GuildState>>>,
    _our_user_id: &Arc<Mutex<Option<u64>>>,
    voice_states: &VoiceStateCache,
    cortex_config: Arc<CortexConfig>,
    _bot_token: &str,
) -> Result<()>
where
    W: SinkExt<Message> + Unpin,
    <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    match cmd {
        VoiceCommand::ResolveUserChannel {
            guild_id,
            user_id,
            reply,
        } => {
            let states = voice_states.lock().await;
            let _ = reply.send(states.get(&(guild_id, user_id)).copied().flatten());
        }
        VoiceCommand::DrainTranscripts { guild_id, reply } => {
            let inbox = {
                let g = guilds.lock().await;
                g.get(&guild_id).map(|s| s.inbox.clone())
            };
            let drained = if let Some(inbox) = inbox {
                let mut v = inbox.lock().await;
                std::mem::take(&mut *v)
            } else {
                Vec::new()
            };
            let _ = reply.send(drained);
        }
        VoiceCommand::Join {
            guild_id,
            channel_id,
            reply,
        } => {
            // Send op-4 VOICE_STATE_UPDATE; record reply for the
            // event handler to complete when both halves arrive.
            let mut g = guilds.lock().await;
            // The inbound handler is registered once, when the guild state
            // is created; a rejoin reuses it.
            let entry = g
                .entry(guild_id)
                .or_insert_with(|| GuildState::new(channel_id, cortex_config.clone()));
            if entry.channel_id != channel_id {
                // Switching channels: audio buffered for the old channel
                // must not be transcribed into the new one.
                entry.clear_inbound().await;
                entry.channel_id = channel_id;
            }
            entry.join_replies.push(reply);
            let payload = serde_json::json!({
                "op": 4,
                "d": {
                    "guild_id": guild_id.to_string(),
                    "channel_id": channel_id.to_string(),
                    "self_mute": false,
                    "self_deaf": false,
                }
            });
            ws_tx
                .send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|e| anyhow!("send voice state update: {e}"))?;
        }
        VoiceCommand::Leave { guild_id, reply } => {
            let mut g = guilds.lock().await;
            if let Some(state) = g.get_mut(&guild_id) {
                state.driver.leave();
            }
            g.remove(&guild_id);
            // Send op-4 with channel_id: null to actually leave server-side.
            let payload = serde_json::json!({
                "op": 4,
                "d": {
                    "guild_id": guild_id.to_string(),
                    "channel_id": serde_json::Value::Null,
                    "self_mute": false,
                    "self_deaf": false,
                }
            });
            ws_tx
                .send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|e| anyhow!("send leave voice state: {e}"))?;
            let _ = reply.send(Ok(()));
        }
        VoiceCommand::Play {
            guild_id,
            path,
            reply,
        } => {
            let mut g = guilds.lock().await;
            let Some(state) = g.get_mut(&guild_id) else {
                let _ = reply.send(Err(anyhow!("not in a voice channel for guild {guild_id}")));
                return Ok(());
            };
            let input = songbird::input::File::new(path);
            let _track = state.driver.play_input(input.into());
            let _ = reply.send(Ok(()));
        }
        VoiceCommand::IsConnected { guild_id, reply } => {
            let g = guilds.lock().await;
            let _ = reply.send(g.contains_key(&guild_id));
        }
    }
    Ok(())
}

async fn process_event(
    text: &str,
    last_seq: &Arc<Mutex<Option<u64>>>,
    our_user_id: &Arc<Mutex<Option<u64>>>,
    guilds: &Arc<Mutex<HashMap<u64, GuildState>>>,
    voice_states: &VoiceStateCache,
) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    if let Some(seq) = v.get("s").and_then(serde_json::Value::as_u64) {
        *last_seq.lock().await = Some(seq);
    }
    let op = v.get("op").and_then(serde_json::Value::as_i64);
    if op != Some(0) {
        // Only handle DISPATCH; HEARTBEAT_ACK et al ignored.
        return Ok(());
    }
    let t = v.get("t").and_then(serde_json::Value::as_str).unwrap_or("");
    let d = v.get("d").cloned().unwrap_or(serde_json::Value::Null);
    match t {
        "READY" => {
            let uid = d
                .get("user")
                .and_then(|u| u.get("id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(uid) = uid {
                *our_user_id.lock().await = Some(uid);
                info!(user_id = uid, "Discord voice manager READY");
            }
        }
        "VOICE_STATE_UPDATE" => {
            let user_id = d
                .get("user_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<u64>().ok());
            let guild_id = d
                .get("guild_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<u64>().ok());
            // channel_id is null when the user leaves voice entirely.
            let channel_id = d.get("channel_id").and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_str().and_then(|s| s.parse::<u64>().ok())
                }
            });

            // Populate the voice-state cache for ALL users — this is
            // how `/voice join` (no-arg) resolves the caller's channel.
            if let (Some(gid), Some(uid)) = (guild_id, user_id) {
                voice_states.lock().await.insert((gid, uid), channel_id);
            }

            // The session_id half of join-completion belongs only to us.
            let our = *our_user_id.lock().await;
            if user_id != our {
                return Ok(());
            }
            let session_id = d
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            if let (Some(gid), Some(sid)) = (guild_id, session_id) {
                let mut g = guilds.lock().await;
                if let Some(state) = g.get_mut(&gid) {
                    state.pending_session_id = Some(sid);
                    try_complete_join(state, gid, our.unwrap_or(0)).await;
                }
            }
        }
        "VOICE_SERVER_UPDATE" => {
            let guild_id = d
                .get("guild_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<u64>().ok());
            let endpoint = d
                .get("endpoint")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let token = d
                .get("token")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            if let (Some(gid), Some(ep), Some(tk)) = (guild_id, endpoint, token) {
                let mut g = guilds.lock().await;
                if let Some(state) = g.get_mut(&gid) {
                    state.pending_endpoint_token = Some((ep, tk));
                    let our = our_user_id.lock().await.unwrap_or(0);
                    try_complete_join(state, gid, our).await;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn try_complete_join(state: &mut GuildState, guild_id: u64, our_user_id: u64) {
    let (sid, ep, tk) = match (
        state.pending_session_id.clone(),
        state.pending_endpoint_token.clone(),
    ) {
        (Some(s), Some((e, t))) => (s, e, t),
        _ => return,
    };
    let Some(gid_nz) = NonZeroU64::new(guild_id) else {
        warn!("guild_id 0 — invalid");
        return;
    };
    let Some(uid_nz) = NonZeroU64::new(our_user_id) else {
        warn!("our_user_id 0 — Gateway READY didn't arrive yet?");
        return;
    };
    let Some(cid_nz) = NonZeroU64::new(state.channel_id) else {
        // Terminal branch: channel_id never transitions to non-zero, so unblock
        // any queued join() callers with an error instead of leaving them
        // hanging until a later leave() drops the reply sender.
        warn!("channel_id 0 — invalid");
        for reply in state.join_replies.drain(..) {
            let _ = reply.send(Err(anyhow!("invalid voice channel_id 0")));
        }
        return;
    };
    let info = ConnectionInfo {
        channel_id: ChannelId::from(cid_nz),
        endpoint: ep,
        guild_id: GuildId::from(gid_nz),
        session_id: sid,
        token: tk,
        user_id: UserId::from(uid_nz),
    };
    state.pending_session_id = None;
    state.pending_endpoint_token = None;
    let result = match state.driver.connect(info).await {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow!("songbird connect: {e}")),
    };
    for reply in state.join_replies.drain(..) {
        let _ = reply.send(match &result {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow!("{e}")),
        });
    }
    if let Err(e) = &result {
        warn!(guild_id, error = %e, "voice connect failed");
    }
}

/// Per-speaker buffer state. Songbird ticks every 20ms with decoded PCM
/// (or marks the user silent). We accumulate samples until N consecutive
/// silent ticks indicate the user stopped talking, then ship the buffer
/// through STT and drop it on the inbox.
struct SpeakerBuffer {
    /// 48 kHz stereo i16 samples (songbird default). Resampled to 16 kHz
    /// mono before WAV-encoding for STT.
    pcm: Vec<i16>,
    silent_ticks: u32,
    user_id: Option<u64>,
}

/// Number of consecutive silent 20ms ticks before we consider the user
/// to have finished speaking and flush their buffer through STT.
/// 50 ticks × 20ms = 1.0s of silence — same threshold as VAD-based
/// stop in the local voice loop.
const SILENT_TICKS_THRESHOLD: u32 = 50;

/// Minimum samples we'll send to STT — short blips below this are
/// almost always button-clicks or filler.
const MIN_FLUSH_SAMPLES_48K_STEREO: usize = (48_000 * 2) / 4; // 250 ms

/// Per-speaker PCM cap: 60 s of 48 kHz stereo (~11.5 MB). A source that
/// never goes silent (an open mic, a music bot) stops accumulating here;
/// what is buffered still flushes on the next silence.
const MAX_SPEAKER_PCM_SAMPLES: usize = 48_000 * 2 * 60;

/// How long a speaker verdict is cached, approved or denied. A denial must
/// expire so a speaker who pairs on the text channel while in voice is heard
/// without rejoining; an approval must expire so a sender revoked through
/// the admin API stops being transcribed without a channel switch.
const VERDICT_TTL: Duration = Duration::from_secs(60);

/// A cached pairing verdict for one speaker in this guild.
#[derive(Clone, Copy)]
struct SpeakerVerdict {
    allowed: bool,
    checked_at: Instant,
}

impl SpeakerVerdict {
    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.checked_at) < VERDICT_TTL
    }
}

#[derive(Clone)]
struct InboundVoiceHandler {
    inbox: TranscriptInbox,
    cortex_config: Arc<CortexConfig>,
    /// SSRC → SpeakerBuffer. Locked behind a Mutex because songbird's
    /// EventHandler::act is async and the per-tick rate is bursty.
    speakers: Arc<Mutex<HashMap<u32, SpeakerBuffer>>>,
    /// Discord user id → pairing verdict, so one speaker costs one database
    /// read per channel session rather than one per flushed utterance.
    /// Cleared by [`GuildState::clear_inbound`] on a channel switch.
    verdicts: Arc<Mutex<HashMap<u64, SpeakerVerdict>>>,
    /// Times the pairing check opened the database. A test seam that
    /// proves the verdict cache holds.
    db_opens: Arc<AtomicUsize>,
}

impl InboundVoiceHandler {
    fn new(inbox: TranscriptInbox, cortex_config: Arc<CortexConfig>) -> Self {
        Self {
            inbox,
            cortex_config,
            speakers: Arc::new(Mutex::new(HashMap::new())),
            verdicts: Arc::new(Mutex::new(HashMap::new())),
            db_opens: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Whether a speaker may be transcribed, evaluated before any audio
    /// reaches an STT provider. The gate is read-only: it asks whether the
    /// sender is already paired and never mints a code, because every
    /// flushed utterance runs through here and `check_sender_access` would
    /// expire the code the speaker was just sent on the text channel. A
    /// speaker with no known user id passes only under an open policy —
    /// pairing cannot have approved an anonymous SSRC.
    async fn speaker_allowed(&self, user_id: Option<u64>) -> bool {
        use cortex::pairing::{resolve_dm_policy, DmPolicy};
        match resolve_dm_policy(&self.cortex_config, "discord") {
            DmPolicy::Open => true,
            DmPolicy::Disabled => false,
            DmPolicy::Pairing => {
                let Some(uid) = user_id else {
                    return false;
                };
                let now = Instant::now();
                if let Some(v) = self.verdicts.lock().await.get(&uid) {
                    if v.is_fresh(now) {
                        return v.allowed;
                    }
                }
                let Some(allowed) = self.read_pairing_verdict(uid).await else {
                    // Database unavailable: deny this utterance, cache
                    // nothing so the next one retries.
                    return false;
                };
                self.verdicts.lock().await.insert(
                    uid,
                    SpeakerVerdict {
                        allowed,
                        checked_at: now,
                    },
                );
                allowed
            }
        }
    }

    /// One read of `approved_senders` for `uid`. `None` when the database
    /// could not answer.
    async fn read_pairing_verdict(&self, uid: u64) -> Option<bool> {
        self.db_opens.fetch_add(1, Ordering::Relaxed);
        let db = match cortex::db::Database::open_with_timeout(
            cortex::db::Database::GATEWAY_BUSY_TIMEOUT_MS,
        )
        .await
        {
            Ok(db) => db,
            Err(e) => {
                warn!(error = %e, "voice speaker access check: database unavailable");
                return None;
            }
        };
        match db.is_sender_approved("discord", &uid.to_string()).await {
            Ok(approved) => Some(approved),
            Err(e) => {
                warn!(error = %e, "voice speaker access check failed");
                None
            }
        }
    }

    /// Core of the `VoiceTick` arm, on plain data: `speaking` pairs each
    /// active SSRC with its decoded PCM for this tick (`None` = not
    /// decoded), `silent` lists SSRCs with no audio this tick. Factored
    /// out of [`EventHandler::act`] because songbird's `VoiceTick` /
    /// `VoiceData` are `#[non_exhaustive]` and cannot be constructed in
    /// unit tests.
    async fn on_voice_tick(&self, speaking: &[(u32, Option<&[i16]>)], silent: &[u32]) {
        let mut speakers = self.speakers.lock().await;
        // Active speakers — extend buffer with the tick's PCM.
        for &(ssrc, pcm) in speaking {
            if let Some(pcm) = pcm {
                let entry = speakers.entry(ssrc).or_insert_with(|| SpeakerBuffer {
                    pcm: Vec::new(),
                    silent_ticks: 0,
                    user_id: None,
                });
                let room = MAX_SPEAKER_PCM_SAMPLES.saturating_sub(entry.pcm.len());
                if room > 0 {
                    entry.pcm.extend_from_slice(&pcm[..pcm.len().min(room)]);
                    if entry.pcm.len() >= MAX_SPEAKER_PCM_SAMPLES {
                        warn!(
                            ssrc,
                            "speaker buffer full; dropping further audio until silence"
                        );
                    }
                }
                entry.silent_ticks = 0;
            }
        }
        // Silent speakers — increment silent counter and flush
        // if past threshold.
        let mut to_flush: Vec<(u32, SpeakerBuffer)> = Vec::new();
        for ssrc in silent {
            if let Some(entry) = speakers.get_mut(ssrc) {
                entry.silent_ticks += 1;
                if entry.silent_ticks >= SILENT_TICKS_THRESHOLD
                    && entry.pcm.len() >= MIN_FLUSH_SAMPLES_48K_STEREO
                {
                    let drained = SpeakerBuffer {
                        pcm: std::mem::take(&mut entry.pcm),
                        silent_ticks: 0,
                        user_id: entry.user_id,
                    };
                    to_flush.push((*ssrc, drained));
                } else if entry.silent_ticks >= SILENT_TICKS_THRESHOLD {
                    // Below min-samples — discard, don't STT.
                    entry.pcm.clear();
                }
            }
        }
        drop(speakers);
        for (ssrc, buf) in to_flush {
            if !self.speaker_allowed(buf.user_id).await {
                debug!(ssrc, user_id = ?buf.user_id, "speaker not allowed; audio dropped");
                continue;
            }
            if let Err(e) = flush_buffer_to_inbox(ssrc, buf, &self.inbox, &self.cortex_config).await
            {
                debug!(ssrc, error = %e, "inbound voice flush failed");
            }
        }
    }
}

#[async_trait::async_trait]
impl EventHandler for InboundVoiceHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                let mut speakers = self.speakers.lock().await;
                let entry = speakers
                    .entry(speaking.ssrc)
                    .or_insert_with(|| SpeakerBuffer {
                        pcm: Vec::new(),
                        silent_ticks: 0,
                        user_id: None,
                    });
                entry.user_id = speaking.user_id.map(|u| u.0);
            }
            EventContext::VoiceTick(tick) => {
                let speaking: Vec<(u32, Option<&[i16]>)> = tick
                    .speaking
                    .iter()
                    .map(|(&ssrc, data)| (ssrc, data.decoded_voice.as_deref()))
                    .collect();
                let silent: Vec<u32> = tick.silent.iter().copied().collect();
                self.on_voice_tick(&speaking, &silent).await;
            }
            _ => {}
        }
        None
    }
}

/// Convert a SpeakerBuffer's 48 kHz stereo PCM into 16 kHz mono WAV bytes,
/// run it through the configured STT pipeline, and append the transcript
/// to the inbox. Best-effort — any failure is logged and dropped. The
/// caller has already passed the speaker through
/// [`InboundVoiceHandler::speaker_allowed`].
async fn flush_buffer_to_inbox(
    ssrc: u32,
    buf: SpeakerBuffer,
    inbox: &TranscriptInbox,
    cortex_config: &CortexConfig,
) -> Result<()> {
    let Some(transcriber) =
        cortex::media::understanding::AudioTranscriber::from_config(cortex_config)
    else {
        return Ok(()); // No STT configured — silently drop.
    };
    let mono_16k = downmix_and_resample_48k_stereo_to_16k_mono(&buf.pcm);
    let wav = encode_wav_mono_16k(&mono_16k)?;
    let label = buf
        .user_id
        .map(|u| format!("discord-user-{u}.wav"))
        .unwrap_or_else(|| format!("discord-ssrc-{ssrc}.wav"));
    match transcriber
        .transcribe(&wav, "audio/wav", &label, None)
        .await
    {
        Ok((transcript, _)) => {
            let prefixed = match buf.user_id {
                Some(uid) => format!("[<@{uid}>]: {transcript}"),
                None => transcript,
            };
            push_transcript(inbox, prefixed).await;
        }
        Err(e) => debug!(ssrc, error = %e, "discord voice STT failed"),
    }
    Ok(())
}

/// Downmix 48 kHz interleaved stereo (L,R,L,R,...) to 48 kHz mono by
/// averaging channels, then naive linear-decimate by 3 to reach 16 kHz.
/// Songbird's default decode is `48 kHz × 2 ch`; STT expects 16 kHz mono.
fn downmix_and_resample_48k_stereo_to_16k_mono(stereo_48k: &[i16]) -> Vec<i16> {
    let mut mono_48k: Vec<i16> = stereo_48k
        .chunks_exact(2)
        .map(|c| ((c[0] as i32 + c[1] as i32) / 2) as i16)
        .collect();
    // Decimate 48 kHz → 16 kHz (keep every third sample). Crude but
    // good enough for Whisper-class STT — speech intelligibility tops
    // out well below 8 kHz so we lose nothing audible.
    let mut out = Vec::with_capacity(mono_48k.len() / 3);
    let mut i = 0;
    while i < mono_48k.len() {
        out.push(mono_48k[i]);
        i += 3;
    }
    mono_48k.clear();
    out
}

fn encode_wav_mono_16k(samples: &[i16]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf: Vec<u8> = Vec::with_capacity(samples.len() * 2 + 44);
    {
        let mut writer = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec)
            .context("WavWriter init")?;
        for s in samples {
            writer.write_sample(*s).context("WAV write_sample")?;
        }
        writer.finalize().context("WAV finalize")?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Required Gateway intents include GUILD_VOICE_STATES so that
    /// VOICE_STATE_UPDATE / VOICE_SERVER_UPDATE events flow. Dropping
    /// the bit silently breaks voice — guard the constant.
    #[test]
    fn intents_include_guild_voice_states() {
        assert!(REQUIRED_INTENTS & (1 << 7) != 0);
        assert!(REQUIRED_INTENTS & 1 != 0, "GUILDS intent also required");
    }

    #[test]
    fn downmix_48k_stereo_to_16k_mono_decimation_correctness() {
        // Six stereo samples (12 ints) at 48 kHz → average to 6 mono
        // samples → decimate by 3 → 2 mono samples.
        let stereo: Vec<i16> = vec![
            100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 1200,
        ];
        let out = downmix_and_resample_48k_stereo_to_16k_mono(&stereo);
        // Mono averages: 150, 350, 550, 750, 950, 1150
        // Keep every 3rd → 150, 750
        assert_eq!(out, vec![150, 750]);
    }

    #[test]
    fn wav_encoder_roundtrips_via_hound() {
        // Sanity: the encoded bytes must be a hound-readable WAV with
        // the right header (16 kHz mono 16-bit). If we change the
        // codec params, STT silently degrades — guard the contract.
        let samples: Vec<i16> = (0..1600).map(|i| (i * 10 - 8000) as i16).collect();
        let bytes = encode_wav_mono_16k(&samples).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(&bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        let decoded: Vec<i16> = reader
            .into_samples::<i16>()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded.len(), samples.len());
    }

    /// Stub manager whose replies embed the arguments it received, so the
    /// handle tests can verify both propagation and argument forwarding.
    fn spawn_echo_stub() -> VoiceManagerHandle {
        let (tx, mut rx) = mpsc::channel::<VoiceCommand>(8);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    VoiceCommand::Join {
                        guild_id,
                        channel_id,
                        reply,
                    } => {
                        let _ = reply.send(Err(anyhow!("join {guild_id}:{channel_id}")));
                    }
                    VoiceCommand::Leave { guild_id, reply } => {
                        let _ = reply.send(Err(anyhow!("leave {guild_id}")));
                    }
                    VoiceCommand::Play {
                        guild_id,
                        path,
                        reply,
                    } => {
                        let _ = reply.send(Err(anyhow!("play {guild_id} {}", path.display())));
                    }
                    VoiceCommand::IsConnected { guild_id, reply } => {
                        let _ = reply.send(guild_id == 1);
                    }
                    VoiceCommand::DrainTranscripts { guild_id, reply } => {
                        let _ = reply.send(if guild_id == 1 {
                            vec!["first".to_string(), "second".to_string()]
                        } else {
                            Vec::new()
                        });
                    }
                    VoiceCommand::ResolveUserChannel { .. } => {}
                }
            }
        });
        handle_for_tests(tx)
    }

    /// The handle methods must round-trip through the manager task: the
    /// command carries the caller's arguments and the caller sees the
    /// manager's actual reply (not a fabricated success/default).
    #[tokio::test]
    async fn handle_methods_forward_args_and_return_manager_replies() {
        let handle = spawn_echo_stub();

        let err = handle.join(7, 8).await.unwrap_err();
        assert_eq!(err.to_string(), "join 7:8");
        let err = handle.leave(3).await.unwrap_err();
        assert_eq!(err.to_string(), "leave 3");
        let err = handle
            .play(4, std::path::PathBuf::from("/tmp/clip.mp3"))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "play 4 /tmp/clip.mp3");

        assert!(handle.is_connected(1).await);
        assert!(!handle.is_connected(2).await);

        assert_eq!(
            handle.drain_transcripts(1).await,
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(handle.drain_transcripts(2).await.is_empty());
    }

    /// `init_voice_manager` must actually install the process-global
    /// handle that `voice_manager_handle()` hands to dispatch sites.
    /// Runs on the current-thread test runtime and never yields, so the
    /// spawned manager task is dropped unpolled — no gateway connection
    /// is ever dialed.
    #[tokio::test]
    async fn init_voice_manager_installs_global_handle() {
        init_voice_manager("test-token".to_string(), CortexConfig::default());
        assert!(voice_manager_handle().is_some(), "handle not installed");
        // Idempotent — a second init keeps the handle installed.
        init_voice_manager("other-token".to_string(), CortexConfig::default());
        assert!(voice_manager_handle().is_some());
    }

    /// Drive `run_voice_manager` against a loopback WebSocket server:
    /// after HELLO it must send a well-formed IDENTIFY (right token,
    /// GUILDS + GUILD_VOICE_STATES intents) and then service commands
    /// until the command channel closes.
    #[tokio::test]
    async fn run_voice_manager_identifies_and_services_commands() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (identify_tx, identify_rx) = oneshot::channel::<serde_json::Value>();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = r#"{"op":10,"d":{"heartbeat_interval":45000}}"#;
            ws.send(Message::Text(hello.to_string().into()))
                .await
                .unwrap();
            let frame = loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => break t.to_string(),
                    Some(Ok(_)) => continue,
                    other => panic!("gateway closed before IDENTIFY: {other:?}"),
                }
            };
            let _ = identify_tx.send(serde_json::from_str(&frame).unwrap());
            // Hold the socket open until the client hangs up.
            while let Some(Ok(_)) = ws.next().await {}
        });

        let (cmd_tx, cmd_rx) = mpsc::channel::<VoiceCommand>(4);
        let url = format!("ws://{addr}");
        let manager = tokio::spawn(async move {
            run_voice_manager(
                "sekrit-token".to_string(),
                cmd_rx,
                CortexConfig::default(),
                &url,
            )
            .await
        });

        let identify = tokio::time::timeout(Duration::from_secs(5), identify_rx)
            .await
            .expect("timed out waiting for IDENTIFY")
            .expect("manager never connected");
        assert_eq!(identify["op"].as_i64(), Some(2));
        assert_eq!(identify["d"]["token"].as_str(), Some("sekrit-token"));
        assert_eq!(identify["d"]["intents"].as_u64(), Some(129));

        // The command loop is live: IsConnected round-trips a reply.
        let (tx, rx) = oneshot::channel();
        cmd_tx
            .send(VoiceCommand::IsConnected {
                guild_id: 1,
                reply: tx,
            })
            .await
            .expect("manager dropped the command channel");
        let connected = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for IsConnected reply")
            .unwrap();
        assert!(!connected, "no guild joined yet");

        // Closing the command channel ends the manager loop cleanly.
        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), manager)
            .await
            .expect("manager did not exit after command channel close")
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn next_gateway_message_skips_control_frames_and_stops_on_close() {
        type WsResult = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>;

        // Control/binary frames are skipped; each call pops the next text.
        let mut s = futures_util::stream::iter::<Vec<WsResult>>(vec![
            Ok(Message::Ping(Vec::new().into())),
            Ok(Message::Binary(vec![1, 2].into())),
            Ok(Message::Pong(Vec::new().into())),
            Ok(Message::Text("payload".to_string().into())),
            Ok(Message::Text("second".to_string().into())),
        ]);
        assert_eq!(
            next_gateway_message(&mut s).await.as_deref(),
            Some("payload")
        );
        assert_eq!(
            next_gateway_message(&mut s).await.as_deref(),
            Some("second")
        );
        assert_eq!(next_gateway_message(&mut s).await, None, "stream exhausted");

        // Close terminates immediately, even with text queued behind it.
        let mut closed = futures_util::stream::iter::<Vec<WsResult>>(vec![
            Ok(Message::Close(None)),
            Ok(Message::Text("late".to_string().into())),
        ]);
        assert_eq!(next_gateway_message(&mut closed).await, None);
    }

    fn test_guild_state(channel_id: u64) -> GuildState {
        GuildState::new(channel_id, Arc::new(CortexConfig::default()))
    }

    async fn join(
        sink: &mut futures::channel::mpsc::UnboundedSender<Message>,
        guilds: &Arc<Mutex<HashMap<u64, GuildState>>>,
        config: &Arc<CortexConfig>,
        channel_id: u64,
    ) {
        let (tx, _rx) = oneshot::channel();
        handle_command(
            VoiceCommand::Join {
                guild_id: 1,
                channel_id,
                reply: tx,
            },
            sink,
            guilds,
            &Arc::new(Mutex::new(None)),
            &Arc::new(Mutex::new(HashMap::new())),
            config.clone(),
            "t",
        )
        .await
        .unwrap();
    }

    /// Spec: the inbound handler is registered on a guild's driver once.
    /// The driver appends handlers, so a rejoin that registered again
    /// would transcribe every utterance twice.
    ///
    /// songbird's `Driver` exposes no handler count, so this reads the
    /// `inbound_registrations` counter that `register_inbound` bumps. The
    /// assertion guards against a reintroduced second `register_inbound`
    /// call on the rejoin path (the regression this test was written
    /// for), not against the counter's own arithmetic.
    #[tokio::test]
    async fn rejoin_registers_inbound_handler_once() {
        let (mut sink, _sent) = futures::channel::mpsc::unbounded::<Message>();
        let guilds: Arc<Mutex<HashMap<u64, GuildState>>> = Arc::new(Mutex::new(HashMap::new()));
        let config = Arc::new(CortexConfig::default());

        join(&mut sink, &guilds, &config, 777).await;
        join(&mut sink, &guilds, &config, 777).await;
        join(&mut sink, &guilds, &config, 778).await;

        let g = guilds.lock().await;
        assert_eq!(
            g.get(&1).unwrap().inbound_registrations,
            1,
            "handler must be registered once per call, not per join"
        );
    }

    /// Spec: switching channels discards audio and transcripts buffered
    /// for the old channel; rejoining the same channel keeps them.
    #[tokio::test]
    async fn switching_channels_clears_buffered_audio_and_transcripts() {
        let (mut sink, _sent) = futures::channel::mpsc::unbounded::<Message>();
        let guilds: Arc<Mutex<HashMap<u64, GuildState>>> = Arc::new(Mutex::new(HashMap::new()));
        let config = Arc::new(CortexConfig::default());

        join(&mut sink, &guilds, &config, 777).await;
        {
            let g = guilds.lock().await;
            let state = g.get(&1).unwrap();
            state.inbox.lock().await.push("said in 777".to_string());
            let pcm = vec![1i16; 960];
            state
                .inbound
                .on_voice_tick(&[(10, Some(pcm.as_slice()))], &[])
                .await;
        }

        // Same channel: nothing is lost.
        join(&mut sink, &guilds, &config, 777).await;
        {
            let g = guilds.lock().await;
            let state = g.get(&1).unwrap();
            assert_eq!(state.inbox.lock().await.len(), 1);
            assert_eq!(state.inbound.speakers.lock().await.len(), 1);
        }

        // Different channel: both buffers are cleared.
        join(&mut sink, &guilds, &config, 778).await;
        let g = guilds.lock().await;
        let state = g.get(&1).unwrap();
        assert_eq!(state.channel_id, 778);
        assert!(
            state.inbox.lock().await.is_empty(),
            "old channel's transcripts must not leak into the new one"
        );
        assert!(
            state.inbound.speakers.lock().await.is_empty(),
            "old channel's audio must not be transcribed later"
        );
    }

    /// Spec: the inbox is bounded; past the cap the oldest transcript goes.
    #[tokio::test]
    async fn inbox_drops_oldest_past_cap() {
        let inbox: TranscriptInbox = Arc::new(Mutex::new(Vec::new()));
        for i in 0..=MAX_INBOX_TRANSCRIPTS {
            push_transcript(&inbox, format!("t{i}")).await;
        }
        let v = inbox.lock().await;
        assert_eq!(v.len(), MAX_INBOX_TRANSCRIPTS);
        assert_eq!(v.first().map(String::as_str), Some("t1"), "oldest dropped");
        assert_eq!(
            v.last().map(String::as_str),
            Some(&*format!("t{MAX_INBOX_TRANSCRIPTS}"))
        );
    }

    /// Spec: a speaker who never goes silent stops accumulating at the cap
    /// instead of growing without bound.
    #[tokio::test]
    async fn speaker_buffer_stops_growing_at_cap() {
        let handler = handler_with_stub_stt("unused");
        let chunk = vec![1i16; MAX_SPEAKER_PCM_SAMPLES / 2 + 1000];
        for _ in 0..3 {
            handler
                .on_voice_tick(&[(10, Some(chunk.as_slice()))], &[])
                .await;
        }
        let speakers = handler.speakers.lock().await;
        assert_eq!(
            speakers.get(&10).unwrap().pcm.len(),
            MAX_SPEAKER_PCM_SAMPLES
        );
    }

    type EventState = (
        Arc<Mutex<Option<u64>>>,
        Arc<Mutex<Option<u64>>>,
        Arc<Mutex<HashMap<u64, GuildState>>>,
        VoiceStateCache,
    );

    fn fresh_event_state() -> EventState {
        (
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    /// `handle_command` must service every command variant: replies carry
    /// real state (not defaults), and Join/Leave put the correct op-4
    /// frames on the gateway socket.
    #[tokio::test]
    async fn handle_command_services_each_command_variant() {
        let (mut sink, mut sent) = futures::channel::mpsc::unbounded::<Message>();
        let guilds: Arc<Mutex<HashMap<u64, GuildState>>> = Arc::new(Mutex::new(HashMap::new()));
        let our_user_id: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let voice_states: VoiceStateCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Arc::new(CortexConfig::default());
        voice_states.lock().await.insert((1, 42), Some(777));
        voice_states.lock().await.insert((1, 43), None);

        let mut next_sent = || {
            let msg = sent.try_recv().expect("no frame sent");
            let Message::Text(t) = msg else {
                panic!("expected text frame");
            };
            serde_json::from_str::<serde_json::Value>(&t).unwrap()
        };

        // ResolveUserChannel reads the voice-state cache.
        for (user_id, want) in [(42u64, Some(777u64)), (43, None), (9, None)] {
            let (tx, rx) = oneshot::channel();
            handle_command(
                VoiceCommand::ResolveUserChannel {
                    guild_id: 1,
                    user_id,
                    reply: tx,
                },
                &mut sink,
                &guilds,
                &our_user_id,
                &voice_states,
                config.clone(),
                "t",
            )
            .await
            .unwrap();
            assert_eq!(rx.await.unwrap(), want, "user {user_id}");
        }

        // Not joined anywhere yet.
        let (tx, rx) = oneshot::channel();
        handle_command(
            VoiceCommand::IsConnected {
                guild_id: 1,
                reply: tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        assert!(!rx.await.unwrap());

        // Join registers guild state and sends the op-4 join frame.
        let (join_tx, mut join_rx) = oneshot::channel();
        handle_command(
            VoiceCommand::Join {
                guild_id: 1,
                channel_id: 777,
                reply: join_tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        let frame = next_sent();
        assert_eq!(frame["op"].as_i64(), Some(4));
        assert_eq!(frame["d"]["guild_id"].as_str(), Some("1"));
        assert_eq!(frame["d"]["channel_id"].as_str(), Some("777"));
        // The join reply stays queued until both gateway halves arrive.
        assert!(join_rx.try_recv().is_err());
        assert!(guilds.lock().await.contains_key(&1));

        let (tx, rx) = oneshot::channel();
        handle_command(
            VoiceCommand::IsConnected {
                guild_id: 1,
                reply: tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        assert!(rx.await.unwrap());

        // DrainTranscripts empties the guild inbox exactly once.
        {
            let g = guilds.lock().await;
            g.get(&1).unwrap().inbox.lock().await.push("hi".to_string());
        }
        for want in [vec!["hi".to_string()], Vec::new()] {
            let (tx, rx) = oneshot::channel();
            handle_command(
                VoiceCommand::DrainTranscripts {
                    guild_id: 1,
                    reply: tx,
                },
                &mut sink,
                &guilds,
                &our_user_id,
                &voice_states,
                config.clone(),
                "t",
            )
            .await
            .unwrap();
            assert_eq!(rx.await.unwrap(), want);
        }

        // Play fails for a guild we haven't joined, succeeds for one we have.
        let (tx, rx) = oneshot::channel();
        handle_command(
            VoiceCommand::Play {
                guild_id: 2,
                path: std::path::PathBuf::from("/tmp/clip.mp3"),
                reply: tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        let err = rx.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("not in a voice channel"), "{err}");
        let (tx, rx) = oneshot::channel();
        handle_command(
            VoiceCommand::Play {
                guild_id: 1,
                path: std::path::PathBuf::from("/tmp/clip.mp3"),
                reply: tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        rx.await.unwrap().unwrap();

        // Leave drops the guild state and sends the op-4 null-channel frame.
        let (tx, rx) = oneshot::channel();
        handle_command(
            VoiceCommand::Leave {
                guild_id: 1,
                reply: tx,
            },
            &mut sink,
            &guilds,
            &our_user_id,
            &voice_states,
            config.clone(),
            "t",
        )
        .await
        .unwrap();
        rx.await.unwrap().unwrap();
        assert!(!guilds.lock().await.contains_key(&1));
        let frame = next_sent();
        assert_eq!(frame["op"].as_i64(), Some(4));
        assert_eq!(frame["d"]["guild_id"].as_str(), Some("1"));
        assert!(frame["d"]["channel_id"].is_null());
    }

    #[tokio::test]
    async fn process_event_records_seq_and_ready_user_id() {
        let (last_seq, our_user_id, guilds, voice_states) = fresh_event_state();
        process_event(
            r#"{"op":0,"s":7,"t":"READY","d":{"user":{"id":"555"}}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(*last_seq.lock().await, Some(7));
        assert_eq!(*our_user_id.lock().await, Some(555));

        // Non-dispatch frames record the seq but never touch dispatch state.
        process_event(
            r#"{"op":11,"s":8,"t":"READY","d":{"user":{"id":"999"}}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(*last_seq.lock().await, Some(8));
        assert_eq!(
            *our_user_id.lock().await,
            Some(555),
            "op 11 must be ignored"
        );
    }

    #[tokio::test]
    async fn voice_state_update_caches_all_users_but_captures_only_our_session() {
        let (last_seq, our_user_id, guilds, voice_states) = fresh_event_state();
        *our_user_id.lock().await = Some(9);
        guilds.lock().await.insert(1, test_guild_state(5));

        // Another user's update populates the cache but must not be
        // mistaken for our join's session half.
        process_event(
            r#"{"op":0,"t":"VOICE_STATE_UPDATE","d":{"user_id":"42","guild_id":"1","channel_id":"777","session_id":"not-ours"}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(voice_states.lock().await.get(&(1, 42)), Some(&Some(777)));
        assert_eq!(
            guilds.lock().await.get(&1).unwrap().pending_session_id,
            None
        );

        // Our own update captures the session id for join completion.
        process_event(
            r#"{"op":0,"t":"VOICE_STATE_UPDATE","d":{"user_id":"9","guild_id":"1","channel_id":"777","session_id":"sess-9"}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(
            guilds.lock().await.get(&1).unwrap().pending_session_id,
            Some("sess-9".to_string())
        );
        assert_eq!(voice_states.lock().await.get(&(1, 9)), Some(&Some(777)));

        // A null channel_id means the user left voice — the cache keeps
        // the entry with None so /voice join knows they're not in voice.
        process_event(
            r#"{"op":0,"t":"VOICE_STATE_UPDATE","d":{"user_id":"42","guild_id":"1","channel_id":null}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(voice_states.lock().await.get(&(1, 42)), Some(&None));
    }

    #[tokio::test]
    async fn voice_server_update_stores_endpoint_and_token() {
        let (last_seq, our_user_id, guilds, voice_states) = fresh_event_state();
        *our_user_id.lock().await = Some(9);
        guilds.lock().await.insert(1, test_guild_state(5));

        process_event(
            r#"{"op":0,"t":"VOICE_SERVER_UPDATE","d":{"guild_id":"1","endpoint":"ep.example:443","token":"tok"}}"#,
            &last_seq,
            &our_user_id,
            &guilds,
            &voice_states,
        )
        .await
        .unwrap();
        assert_eq!(
            guilds.lock().await.get(&1).unwrap().pending_endpoint_token,
            Some(("ep.example:443".to_string(), "tok".to_string()))
        );
    }

    #[tokio::test]
    async fn try_complete_join_fails_queued_joins_on_zero_channel() {
        // Terminal branch: channel_id 0 must unblock queued joins with an
        // error rather than leaving them hanging.
        let mut state = test_guild_state(0);
        state.pending_session_id = Some("sid".to_string());
        state.pending_endpoint_token = Some(("ep".to_string(), "tok".to_string()));
        let (tx_a, mut rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        state.join_replies.push(tx_a);
        state.join_replies.push(tx_b);

        try_complete_join(&mut state, 1, 9).await;

        for rx in [&mut rx_a, &mut rx_b] {
            let err = rx
                .try_recv()
                .expect("join caller left hanging")
                .unwrap_err();
            assert!(
                err.to_string().contains("invalid voice channel_id 0"),
                "{err}"
            );
        }
        assert!(state.join_replies.is_empty());
    }

    #[tokio::test]
    async fn try_complete_join_waits_until_both_halves_arrive() {
        // Only the session half present — nothing must complete or fail.
        let mut state = test_guild_state(5);
        state.pending_session_id = Some("sid".to_string());
        let (tx, mut rx) = oneshot::channel();
        state.join_replies.push(tx);

        try_complete_join(&mut state, 1, 9).await;

        assert!(rx.try_recv().is_err(), "reply must stay queued");
        assert_eq!(state.join_replies.len(), 1);
        assert_eq!(state.pending_session_id, Some("sid".to_string()));
    }

    /// Handler whose STT is the hermetic command-template provider — the
    /// transcript is whatever the shell command prints. No network, no keys.
    fn handler_with_stub_stt(transcript: &str) -> InboundVoiceHandler {
        let mut config = stub_stt_config(transcript);
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;
        InboundVoiceHandler::new(Arc::new(Mutex::new(Vec::new())), Arc::new(config))
    }

    fn stub_stt_config(transcript: &str) -> CortexConfig {
        let mut config = CortexConfig::default();
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "custom".into(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: Some(10_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some(format!(": {{audio_file}} && printf '{transcript}'")),
        }];
        config
    }

    /// Spec: speaker access is checked before any audio reaches STT. Under
    /// a disabled policy no speaker is transcribed; under pairing an SSRC
    /// with no known user id cannot have been approved and is dropped.
    #[tokio::test]
    async fn denied_speaker_is_not_transcribed() {
        let mut disabled = stub_stt_config("leaked");
        disabled.gateway.dm_policy = cortex::pairing::DmPolicy::Disabled;
        let mut pairing = stub_stt_config("leaked");
        pairing.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;

        for (label, config, user) in [
            ("disabled policy", disabled, Some(4242u64)),
            ("pairing, anonymous ssrc", pairing, None),
        ] {
            let handler =
                InboundVoiceHandler::new(Arc::new(Mutex::new(Vec::new())), Arc::new(config));
            if let Some(uid) = user {
                handler.act(&speaking_update(10, uid)).await;
            }
            let pcm = vec![100i16; MIN_FLUSH_SAMPLES_48K_STEREO];
            handler
                .on_voice_tick(&[(10, Some(pcm.as_slice()))], &[])
                .await;
            for _ in 0..SILENT_TICKS_THRESHOLD {
                handler.on_voice_tick(&[], &[10]).await;
            }
            assert!(
                handler.inbox.lock().await.is_empty(),
                "{label}: denied speaker must not be transcribed"
            );
        }
    }

    /// A fresh `BORG_HOME` so the pairing tables start empty and nothing
    /// touches the developer's real database.
    struct VoiceHome {
        _tmp: tempfile::TempDir,
        _guard: crate::test_env::BorgHomeGuard,
    }

    fn hermetic_home() -> VoiceHome {
        let guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("BORG_HOME", tmp.path()) };
        VoiceHome {
            _tmp: tmp,
            _guard: guard,
        }
    }

    impl Drop for VoiceHome {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("BORG_HOME") };
        }
    }

    async fn open_test_db() -> cortex::db::Database {
        cortex::db::Database::open_with_timeout(cortex::db::Database::GATEWAY_BUSY_TIMEOUT_MS)
            .await
            .unwrap()
    }

    async fn approve_discord_sender(db: &cortex::db::Database, uid: u64) {
        let code = format!("dc/test-code-{uid}");
        db.create_pairing_request("discord", &uid.to_string(), &code, None, 600)
            .await
            .unwrap();
        db.approve_pairing("discord", &code).await.unwrap();
    }

    /// One complete utterance from `uid` on `ssrc`: a speaking update, a
    /// tick of audio above the flush floor, then enough silence to flush.
    async fn utter(handler: &InboundVoiceHandler, ssrc: u32, uid: u64) {
        handler.act(&speaking_update(ssrc, uid)).await;
        let pcm = vec![100i16; MIN_FLUSH_SAMPLES_48K_STEREO];
        handler
            .on_voice_tick(&[(ssrc, Some(pcm.as_slice()))], &[])
            .await;
        for _ in 0..SILENT_TICKS_THRESHOLD {
            handler.on_voice_tick(&[], &[ssrc]).await;
        }
    }

    fn pairing_handler(transcript: &str) -> InboundVoiceHandler {
        let mut config = stub_stt_config(transcript);
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;
        InboundVoiceHandler::new(Arc::new(Mutex::new(Vec::new())), Arc::new(config))
    }

    /// Spec: under pairing an unapproved speaker is dropped, and the check
    /// is read-only. Routing it through the text path's access check would
    /// mint a fresh code on every utterance and expire the one the speaker
    /// was just sent on the text channel.
    #[tokio::test]
    async fn unpaired_speaker_is_denied_without_minting_a_pairing_code() {
        let _home = hermetic_home();
        let handler = pairing_handler("leaked");

        utter(&handler, 10, 4242).await;

        assert!(
            handler.inbox.lock().await.is_empty(),
            "unpaired speaker must not be transcribed"
        );
        let db = open_test_db().await;
        assert!(
            db.find_pending_for_sender("discord", "4242")
                .await
                .unwrap()
                .is_none(),
            "voice gate must not create a pairing request"
        );
        assert_eq!(
            db.count_pairing_attempts("discord", "4242", 3600)
                .await
                .unwrap(),
            0,
            "voice gate must not count as a pairing attempt"
        );
    }

    /// Spec: an approved speaker is transcribed, and the verdict is read
    /// from the database once per speaker, not once per utterance.
    #[tokio::test]
    async fn approved_speaker_is_transcribed_and_verdict_is_cached() {
        let _home = hermetic_home();
        approve_discord_sender(&open_test_db().await, 4242).await;
        let handler = pairing_handler("hello");

        utter(&handler, 10, 4242).await;
        utter(&handler, 10, 4242).await;

        assert_eq!(
            handler.inbox.lock().await.as_slice(),
            ["[<@4242>]: hello", "[<@4242>]: hello"],
        );
        assert_eq!(
            handler.db_opens.load(Ordering::Relaxed),
            1,
            "second utterance from the same speaker must hit the cache"
        );
    }

    /// Spec: an approval is cached only for the verdict TTL, so a sender
    /// revoked through the admin API stops being transcribed once the cached
    /// verdict ages out, without a channel switch or a daemon restart.
    #[tokio::test]
    async fn revoked_speaker_is_denied_once_the_cached_approval_ages_out() {
        let _home = hermetic_home();
        let db = open_test_db().await;
        approve_discord_sender(&db, 4242).await;
        let handler = pairing_handler("hello");

        utter(&handler, 10, 4242).await;
        assert_eq!(handler.inbox.lock().await.as_slice(), ["[<@4242>]: hello"]);

        assert!(
            db.revoke_sender("discord", "4242")
                .await
                .expect("revoke sender"),
            "the approved sender row must exist to be revoked"
        );
        let expired = Instant::now()
            .checked_sub(VERDICT_TTL + Duration::from_secs(1))
            .expect("instant arithmetic");
        handler
            .verdicts
            .lock()
            .await
            .get_mut(&4242)
            .expect("verdict cached after the first utterance")
            .checked_at = expired;

        utter(&handler, 10, 4242).await;
        assert_eq!(
            handler.inbox.lock().await.as_slice(),
            ["[<@4242>]: hello"],
            "a revoked speaker must not be transcribed after the verdict expires"
        );
        assert_eq!(
            handler.db_opens.load(Ordering::Relaxed),
            2,
            "the expired approval must be re-read from the database"
        );
    }

    /// Spec: a cached denial survives an approval until the channel is
    /// switched (or the denial ages out), and a switch re-reads pairing
    /// state so the newly approved speaker is heard.
    #[tokio::test]
    async fn channel_switch_forgets_cached_speaker_verdicts() {
        let _home = hermetic_home();
        let mut config = stub_stt_config("hello");
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;
        let state = GuildState::new(777, Arc::new(config));
        let handler = &state.inbound;

        utter(handler, 10, 4242).await;
        approve_discord_sender(&open_test_db().await, 4242).await;
        utter(handler, 10, 4242).await;
        assert!(
            handler.inbox.lock().await.is_empty(),
            "denial is cached for the channel session"
        );
        assert_eq!(handler.db_opens.load(Ordering::Relaxed), 1);

        state.clear_inbound().await;
        utter(handler, 10, 4242).await;
        assert_eq!(
            handler.inbox.lock().await.as_slice(),
            ["[<@4242>]: hello"],
            "a channel switch must re-read pairing state"
        );
        assert_eq!(handler.db_opens.load(Ordering::Relaxed), 2);
    }

    fn speaking_update(ssrc: u32, user_id: u64) -> EventContext<'static> {
        EventContext::SpeakingStateUpdate(songbird::model::payload::Speaking {
            delay: Some(0),
            speaking: songbird::model::SpeakingState::MICROPHONE,
            ssrc,
            user_id: Some(songbird::model::id::UserId(user_id)),
        })
    }

    #[tokio::test]
    async fn speaking_state_update_maps_ssrc_to_user() {
        let handler = handler_with_stub_stt("unused");
        handler.act(&speaking_update(10, 4242)).await;
        let speakers = handler.speakers.lock().await;
        assert_eq!(
            speakers.get(&10).expect("speaker registered").user_id,
            Some(4242)
        );
    }

    #[tokio::test]
    async fn voice_tick_flushes_after_full_silence_threshold() {
        let handler = handler_with_stub_stt("voice hi");
        handler.act(&speaking_update(10, 4242)).await;
        let pcm = vec![100i16; MIN_FLUSH_SAMPLES_48K_STEREO];
        handler
            .on_voice_tick(&[(10, Some(pcm.as_slice()))], &[])
            .await;

        // 49 silent ticks (0.98s) — still below the 1s threshold.
        for _ in 0..SILENT_TICKS_THRESHOLD - 1 {
            handler.on_voice_tick(&[], &[10]).await;
        }
        assert!(
            handler.inbox.lock().await.is_empty(),
            "flushed before the silence threshold"
        );

        // The 50th tick crosses the threshold: STT runs and the transcript
        // lands on the inbox with the speaker's user mention prefix.
        handler.on_voice_tick(&[], &[10]).await;
        assert_eq!(
            *handler.inbox.lock().await,
            vec!["[<@4242>]: voice hi".to_string()]
        );

        // The buffer was consumed — further silence must not re-flush.
        handler.on_voice_tick(&[], &[10]).await;
        assert_eq!(handler.inbox.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn voice_tick_discards_sub_250ms_buffer_without_stt() {
        let handler = handler_with_stub_stt("must-not-appear");
        // Half the 250ms minimum — a blip, not speech.
        let pcm = vec![100i16; MIN_FLUSH_SAMPLES_48K_STEREO / 2];
        handler
            .on_voice_tick(&[(10, Some(pcm.as_slice()))], &[])
            .await;
        for _ in 0..SILENT_TICKS_THRESHOLD {
            handler.on_voice_tick(&[], &[10]).await;
        }
        assert!(
            handler.inbox.lock().await.is_empty(),
            "short blip must be discarded, not transcribed"
        );
        let speakers = handler.speakers.lock().await;
        assert!(
            speakers.get(&10).unwrap().pcm.is_empty(),
            "buffer not discarded"
        );
    }

    #[tokio::test]
    async fn voice_tick_retains_buffer_during_brief_silence() {
        let handler = handler_with_stub_stt("unused");
        let pcm = vec![100i16; MIN_FLUSH_SAMPLES_48K_STEREO];
        handler
            .on_voice_tick(&[(10, Some(pcm.as_slice()))], &[])
            .await;
        handler.on_voice_tick(&[], &[10]).await;

        let speakers = handler.speakers.lock().await;
        let entry = speakers.get(&10).unwrap();
        assert_eq!(entry.silent_ticks, 1, "one silent tick counted");
        assert_eq!(
            entry.pcm.len(),
            MIN_FLUSH_SAMPLES_48K_STEREO,
            "buffer must survive a brief pause"
        );
        assert!(handler.inbox.lock().await.is_empty());
    }
}
