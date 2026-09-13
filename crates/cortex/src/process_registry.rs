//! Subprocess process registry — central record of every agent-initiated
//! child process so `borg status`, `borg doctor`, and the self-healing
//! sweep can see what's still running and reap what isn't.
//!
//! ## Scope
//!
//! Only **agent-initiated** spawns are tracked: `run_shell`, the headless
//! `browser`, hook scripts, scheduled command tasks, and `pre_run_command`
//! preambles. Short-lived internal utilities (`git`, `sips`, `security`,
//! `launchctl`) are intentionally excluded — they finish in milliseconds
//! and would crowd out the signal we actually want, which is "what's
//! still running that shouldn't be." If you're adding a new spawn site,
//! ask: would I want this PID to show up in `borg status`? If no, don't
//! register it.
//!
//! ## RAII lifecycle
//!
//! Every register call returns a [`ProcessGuard`]. The guard's `Drop` impl
//! marks the entry [`ProcessStatus::Killed`] if it wasn't explicitly
//! completed — so a forgotten `?` early-return on the spawn site still
//! produces an accurate audit trail. Spawn sites should call
//! [`ProcessGuard::complete`] / [`ProcessGuard::timed_out`] /
//! [`ProcessGuard::killed`] on the success / timeout / explicit-kill arms.
//!
//! ## Auto-reap & whitelisting
//!
//! [`ProcessRegistry::auto_reap`] is invoked by the daily maintenance
//! sweep. It SIGTERMs (then SIGKILLs after `sigterm_grace_secs`) any
//! running entry older than `max_age_secs`, *unless* one of three
//! whitelist layers exempts it:
//!
//! 1. **Kind exemption** — `Browser` is hard-coded exempt. The headless
//!    Chrome session is meant to outlive any single tool call.
//! 2. **Pattern allowlist** — caller-supplied glob list (see
//!    [`compile_never_kill_patterns`]) matched against the command
//!    string. Default patterns target common long-running build/test
//!    workloads.
//! 3. **Per-entry pin** — [`ProcessRegistry::pin`] flips a `pinned` flag
//!    that the TUI exposes as an inline toggle on the subprocess view.

use crate::util::time::now_unix;
use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};

/// Process-global registry handle. The daemon installs once at startup
/// (see `borgd::daemon::run`) so library call sites — task_runner's
/// command-task spawn, the maintenance sweep when invoked from a
/// scheduled task — can pick it up without threading a handle through
/// every signature.
///
/// In CLI/one-shot/test contexts the global stays uninstalled and
/// [`global`] returns `None`; spawn sites then operate without a
/// registry exactly as they did before this feature.
static GLOBAL_REGISTRY: OnceLock<ProcessRegistry> = OnceLock::new();

/// Install the process-global registry. Returns `true` on first install,
/// `false` if one was already installed (single-installer invariant —
/// matches `install_global_audit`).
pub fn install_global(reg: ProcessRegistry) -> bool {
    GLOBAL_REGISTRY.set(reg).is_ok()
}

/// Fetch a clone of the process-global registry, if installed.
pub fn global() -> Option<ProcessRegistry> {
    GLOBAL_REGISTRY.get().cloned()
}

/// Maximum characters retained from the command string. Long shell
/// pipelines truncate to this; the prefix is the part that matters for
/// pattern matching and human display.
pub const COMMAND_DISPLAY_MAX_CHARS: usize = 256;

/// Why a subprocess was registered. Drives display formatting and is the
/// pivot for the hard-coded auto-reap kind exemption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessKind {
    /// A `run_shell` tool invocation (incl. PTY variant).
    RunShell,
    /// The headless `browser` (Chrome via chromiumoxide).
    Browser,
    /// A user-authored lifecycle hook script.
    Hook,
    /// A scheduled task of type `command`.
    ScheduledCommand,
    /// A `pre_run_command` preamble for a scheduled prompt task.
    PreRun,
}

impl ProcessKind {
    /// Display tag used in `borg status` and the TUI subprocess view.
    pub fn tag(self) -> &'static str {
        match self {
            ProcessKind::RunShell => "RunShell",
            ProcessKind::Browser => "Browser",
            ProcessKind::Hook => "Hook",
            ProcessKind::ScheduledCommand => "ScheduledCommand",
            ProcessKind::PreRun => "PreRun",
        }
    }

    /// Kinds the auto-reap step always skips, regardless of age.
    fn is_auto_reap_exempt(self) -> bool {
        // The browser is meant to outlive any single tool call; auto-killing
        // it at 24h would defeat the feature. Other kinds are eligible.
        matches!(self, ProcessKind::Browser)
    }
}

/// Lifecycle state of a registered entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessStatus {
    /// Still running according to the guard.
    Running,
    /// Exited normally; `exit_code` populated.
    Exited,
    /// Wall-clock or user timeout fired; differs from `Killed` so the
    /// doctor surface can distinguish "we gave up waiting" from "the
    /// caller bailed without finishing the lifecycle."
    TimedOut,
    /// Explicitly killed (auto-reap, user action, or guard dropped
    /// without a terminal call).
    Killed,
    /// The maintenance sweep's `kill -0` liveness probe found the PID
    /// gone. Either the daemon crashed mid-spawn or the kernel reaped
    /// the process before the guard could fire.
    Dead,
}

/// One entry in the registry. Cloned freely; the registry itself owns
/// the source of truth.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessEntry {
    /// Monotonic registry id. PIDs reuse; ids do not.
    pub id: u64,
    /// OS PID. `0` when unknown (e.g. headless Chrome — see
    /// [`ProcessKind::Browser`] notes in the module rustdoc).
    pub pid: u32,
    pub kind: ProcessKind,
    /// Command string, truncated to [`COMMAND_DISPLAY_MAX_CHARS`].
    pub command: String,
    /// Owning session, for "kill all my children" UX when implemented.
    pub session_id: Option<String>,
    /// Unix seconds at register time.
    pub started_at: i64,
    /// Kernel process start-time fingerprint captured at register time, used to
    /// detect PID reuse before we signal. `None` when the platform can't
    /// provide it (the kill path then falls back to liveness-only behavior).
    /// `#[serde(default)]` so snapshots from older daemons still deserialize.
    #[serde(default)]
    pub start_fingerprint: Option<u64>,
    /// Unix seconds when the entry transitioned out of `Running`.
    pub finished_at: Option<i64>,
    /// Exit code captured on `complete()`; `None` for non-`Exited`
    /// terminal states.
    pub exit_code: Option<i32>,
    pub status: ProcessStatus,
    /// Pin override — exempt from auto-reap and downgrades doctor
    /// severity. Cleared automatically when the process exits.
    pub pinned: bool,
}

impl ProcessEntry {
    /// Age in seconds at `now`. Saturating so a clock jump backward
    /// doesn't produce a panic.
    pub fn age_secs(&self, now: i64) -> i64 {
        now.saturating_sub(self.started_at).max(0)
    }
}

/// Snapshot view returned to callers (gRPC, status renderers).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RegistrySnapshot {
    pub running: Vec<ProcessEntry>,
    pub finished: Vec<ProcessEntry>,
}

/// Policy passed into [`ProcessRegistry::auto_reap`].
#[derive(Clone, Debug)]
pub struct AutoReapPolicy {
    /// Age past which a non-exempt running entry is SIGTERM'd. `0`
    /// disables auto-reap entirely.
    pub max_age_secs: i64,
    /// Compiled command-string patterns whose match exempts an entry.
    pub never_kill_patterns: Vec<Regex>,
    /// Seconds between SIGTERM and SIGKILL.
    pub sigterm_grace_secs: u64,
}

/// Report returned from a single auto-reap pass.
#[derive(Clone, Debug, Default)]
pub struct AutoReapReport {
    /// SIGTERM landed and the process exited within the grace window.
    pub terminated: Vec<ProcessEntry>,
    /// Process ignored SIGTERM and had to be SIGKILL'd.
    pub killed: Vec<ProcessEntry>,
    pub exempted_by_kind: usize,
    pub exempted_by_pattern: usize,
    pub exempted_by_pin: usize,
    /// Entries whose PID was recycled to a different process before the sweep
    /// could signal it — skipped to avoid killing an unrelated bystander.
    pub recycled_skipped: usize,
}

impl AutoReapReport {
    /// Count of entries the sweep actually killed (SIGTERM or SIGKILL).
    pub fn killed_count(&self) -> usize {
        self.terminated.len() + self.killed.len()
    }

    /// Count of entries past the age threshold but spared.
    pub fn exempted_count(&self) -> usize {
        self.exempted_by_kind + self.exempted_by_pattern + self.exempted_by_pin
    }
}

/// Turn-scoped process baseline captured by [`ProcessRegistry::begin_turn`].
///
/// Records which processes were already running when the turn started (those
/// always survive an abandoned-turn reap) plus the session's turn generation
/// at capture time (a stale generation makes the reap a no-op).
#[derive(Clone, Debug)]
pub struct TurnBaseline {
    /// Session that owns the turn. The reap only considers entries whose
    /// `session_id` matches — a concurrent session's spawns are untouchable.
    pub session_id: String,
    /// The session's turn generation when this baseline was captured.
    pub generation: u64,
    /// Registry ids running at turn start — exempt from the abandoned reap.
    pub baseline_ids: HashSet<u64>,
}

/// Report returned by [`ProcessRegistry::reap_abandoned_turn`].
#[derive(Clone, Debug, Default)]
pub struct AbandonedTurnReport {
    /// Entries killed (SIGTERM or SIGKILL; includes signal-failed entries
    /// recorded as `Dead`, mirroring `auto_reap`).
    pub killed: Vec<ProcessEntry>,
    /// Turn-spawned entries spared by an explicit user pin.
    pub exempted_by_pin: usize,
    /// Turn-spawned entries spared by a `never_kill_patterns` match.
    pub exempted_by_pattern: usize,
    /// Turn-spawned entries spared by kind (Browser).
    pub exempted_by_kind: usize,
    /// Entries whose PID was already gone — recorded `Dead`, not signaled.
    pub already_gone: usize,
    /// Entries skipped because the PID was recycled to a different process.
    pub recycled_skipped: usize,
    /// True when the whole reap was skipped because a replacement turn has
    /// claimed the session since the baseline was captured.
    pub stale_generation: bool,
}

/// First few tokens of a command for log lines — enough to identify the
/// workload without dumping a full pipeline into the log.
fn command_head(command: &str) -> String {
    const HEAD_MAX_CHARS: usize = 64;
    if command.chars().count() <= HEAD_MAX_CHARS {
        return command.to_string();
    }
    let head: String = command.chars().take(HEAD_MAX_CHARS).collect();
    format!("{head}…")
}

/// Convert glob-style patterns (`cargo build*`, `bun *`) into anchored
/// regexes. Only `*` is honoured; other regex metacharacters are
/// escaped. Empty / invalid patterns are silently dropped with a
/// `tracing::warn!` so a single bad row in `never_kill_patterns` never
/// turns the registry into a no-op.
pub fn compile_never_kill_patterns(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pat| match glob_to_regex(pat) {
            Some(rx) => Some(rx),
            None => {
                tracing::warn!(
                    pattern = pat.as_str(),
                    "process_registry: dropping unparseable never_kill_patterns entry"
                );
                None
            }
        })
        .collect()
}

fn glob_to_regex(pattern: &str) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    let mut rx = String::with_capacity(pattern.len() + 4);
    rx.push('^');
    for ch in pattern.chars() {
        if ch == '*' {
            rx.push_str(".*");
        } else {
            // Escape everything else so a literal `.` in a command name
            // doesn't accidentally match anything.
            rx.push_str(&regex::escape(&ch.to_string()));
        }
    }
    rx.push('$');
    Regex::new(&rx).ok()
}

/// Default `never_kill_patterns` shipped with the binary. Covers the
/// common cases where 24h+ wall-clock is legitimate (long builds,
/// long test suites). Operators add host-specific entries via
/// `process_registry.never_kill_patterns`.
pub fn default_never_kill_patterns() -> Vec<String> {
    vec![
        "cargo build*".into(),
        "cargo test*".into(),
        "cargo run*".into(),
        "cargo clippy*".into(),
        "make*".into(),
        "pytest*".into(),
        "npm run*".into(),
        "bun *".into(),
    ]
}

/// Central registry. Cheap to clone — the inner state is shared via
/// `Arc<Mutex<_>>`.
#[derive(Clone, Default)]
pub struct ProcessRegistry {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    running: HashMap<u64, ProcessEntry>,
    /// Bounded LRU. Most recent push to the back; eviction off the front.
    finished: VecDeque<ProcessEntry>,
    next_id: u64,
    /// Last sweep's auto-reap target commands (normalized first token).
    /// Used by the maintenance sweep to detect repeat-offender workloads
    /// that the operator should add to `never_kill_patterns`.
    last_auto_reaped_commands: Vec<String>,
    /// Per-session turn generation, bumped by [`ProcessRegistry::begin_turn`].
    /// The abandoned-turn reap compares its captured generation against this
    /// map: if a replacement turn has claimed the session since the baseline
    /// was taken, the (now stale) reap is a no-op — it must never kill
    /// processes the replacement turn legitimately spawned. One `u64` per
    /// session id; bounded by the number of distinct sessions this daemon
    /// hosts over its lifetime.
    turn_generations: HashMap<String, u64>,
}

impl ProcessRegistry {
    /// Construct an empty registry. The daemon creates one at startup
    /// and hands clones to every spawn site.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly-spawned process. The returned guard must be
    /// kept alive for the lifetime of the child; on drop it marks the
    /// entry `Killed` unless `complete`/`timed_out`/`killed` was called.
    pub fn register(
        &self,
        pid: u32,
        kind: ProcessKind,
        command: impl Into<String>,
        session_id: Option<String>,
    ) -> ProcessGuard {
        let now = now_unix();
        let mut inner = self.lock();
        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        let id = inner.next_id;
        let cmd_trunc = truncate_command(command.into());
        let entry = ProcessEntry {
            id,
            pid,
            kind,
            command: cmd_trunc,
            session_id,
            started_at: now,
            start_fingerprint: pid_start_fingerprint(pid),
            finished_at: None,
            exit_code: None,
            status: ProcessStatus::Running,
            pinned: false,
        };
        inner.running.insert(id, entry);
        ProcessGuard {
            registry: self.clone(),
            id,
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Snapshot of all currently running entries.
    pub fn running(&self) -> Vec<ProcessEntry> {
        let inner = self.lock();
        let mut out: Vec<_> = inner.running.values().cloned().collect();
        out.sort_by_key(|e| e.id);
        out
    }

    /// Full snapshot — running + finished — for status displays.
    pub fn snapshot(&self) -> RegistrySnapshot {
        let inner = self.lock();
        let mut running: Vec<_> = inner.running.values().cloned().collect();
        running.sort_by_key(|e| e.id);
        let finished = inner.finished.iter().cloned().collect();
        RegistrySnapshot { running, finished }
    }

    /// Pin a running entry. Pinned entries are exempt from auto-reap
    /// and do not contribute to doctor Warn/Fail counts.
    pub fn pin(&self, id: u64) -> Result<()> {
        let mut inner = self.lock();
        let entry = inner
            .running
            .get_mut(&id)
            .ok_or_else(|| anyhow!("process_registry: unknown id {id}"))?;
        entry.pinned = true;
        Ok(())
    }

    /// Clear the pin on a running entry.
    pub fn unpin(&self, id: u64) -> Result<()> {
        let mut inner = self.lock();
        let entry = inner
            .running
            .get_mut(&id)
            .ok_or_else(|| anyhow!("process_registry: unknown id {id}"))?;
        entry.pinned = false;
        Ok(())
    }

    /// Probe every running entry's PID with `kill(pid, 0)`. Entries
    /// whose PID has vanished get marked `Dead` and moved to the
    /// finished queue. Returns the count moved.
    pub fn reap_dead(&self, now: i64) -> usize {
        let candidates: Vec<(u64, u32)> = {
            let inner = self.lock();
            inner
                .running
                .values()
                .filter(|e| e.pid != 0) // Browser etc. have pid=0; nothing to probe.
                .map(|e| (e.id, e.pid))
                .collect()
        };
        let mut reaped = 0usize;
        for (id, pid) in candidates {
            if !pid_alive(pid) {
                let mut inner = self.lock();
                if let Some(mut entry) = inner.running.remove(&id) {
                    entry.status = ProcessStatus::Dead;
                    entry.finished_at = Some(now);
                    entry.pinned = false;
                    push_finished(&mut inner.finished, entry);
                    reaped += 1;
                }
            }
        }
        reaped
    }

    /// Auto-reap pass. SIGTERM (then SIGKILL after grace) every running
    /// entry past `policy.max_age_secs` unless an exemption applies.
    /// Returns a structured report so the maintenance sweep can count
    /// kills vs. exemptions and detect repeat offenders.
    pub fn auto_reap(&self, now: i64, policy: &AutoReapPolicy) -> AutoReapReport {
        let mut report = AutoReapReport::default();
        if policy.max_age_secs <= 0 {
            return report;
        }
        let candidates: Vec<ProcessEntry> = {
            let inner = self.lock();
            inner
                .running
                .values()
                .filter(|e| e.age_secs(now) > policy.max_age_secs)
                .cloned()
                .collect()
        };

        let mut to_kill: Vec<ProcessEntry> = Vec::new();
        for entry in candidates {
            if entry.kind.is_auto_reap_exempt() {
                report.exempted_by_kind += 1;
                continue;
            }
            if entry.pinned {
                report.exempted_by_pin += 1;
                continue;
            }
            if matches_any(&entry.command, &policy.never_kill_patterns) {
                report.exempted_by_pattern += 1;
                continue;
            }
            to_kill.push(entry);
        }

        let mut reaped_norm: Vec<String> = Vec::with_capacity(to_kill.len());
        for entry in to_kill {
            reaped_norm.push(normalize_command(&entry.command));
            let outcome = send_term_then_kill(
                entry.pid,
                policy.sigterm_grace_secs,
                entry.start_fingerprint,
            );
            let finished = finalize_killed(self, entry.id, now);
            if let Some(mut f) = finished {
                match outcome {
                    KillOutcome::TermSufficed => {
                        f.status = ProcessStatus::Killed;
                        report.terminated.push(f);
                    }
                    KillOutcome::NeededKill => {
                        f.status = ProcessStatus::Killed;
                        report.killed.push(f);
                    }
                    KillOutcome::SignalFailed => {
                        // Couldn't even send SIGTERM (no permission, weird
                        // PID); mark Dead so the sweep still cleans it up.
                        f.status = ProcessStatus::Dead;
                        report.killed.push(f);
                    }
                    KillOutcome::Recycled => {
                        // PID belonged to a different process — we signaled
                        // nothing. Drop our stale entry as Dead and count it so
                        // the sweep is honest about what it did (and didn't) do.
                        tracing::warn!(
                            pid = entry.pid,
                            id = entry.id,
                            "process_registry: PID recycled before reap; skipped kill"
                        );
                        f.status = ProcessStatus::Dead;
                        report.recycled_skipped += 1;
                    }
                }
            }
        }

        // Stash for the next sweep's repeat-offender check.
        let mut inner = self.lock();
        inner.last_auto_reaped_commands = reaped_norm;
        report
    }

    /// Returns the normalized command tokens auto-reaped on the previous
    /// `auto_reap` call. The maintenance sweep diffs two consecutive
    /// passes to detect "you keep killing the same workload — whitelist it."
    pub fn previous_auto_reaped_commands(&self) -> Vec<String> {
        self.lock().last_auto_reaped_commands.clone()
    }

    /// Ids of all currently running entries. The turn-scoped baseline: any
    /// running entry present *now* predates the turn and must survive an
    /// abandoned-turn reap.
    pub fn snapshot_running_ids(&self) -> HashSet<u64> {
        self.lock().running.keys().copied().collect()
    }

    /// Open a turn for `session_id`: bump the session's turn generation and
    /// capture the running-id baseline. Call at turn start; keep the returned
    /// [`TurnBaseline`] for the life of the turn. On abandonment, pass it to
    /// [`ProcessRegistry::reap_abandoned_turn`]; on normal completion just
    /// drop it — backgrounded processes are supposed to outlive their turn.
    pub fn begin_turn(&self, session_id: &str) -> TurnBaseline {
        let mut inner = self.lock();
        let generation = {
            let g = inner
                .turn_generations
                .entry(session_id.to_string())
                .or_insert(0);
            *g = g.wrapping_add(1);
            *g
        };
        TurnBaseline {
            session_id: session_id.to_string(),
            generation,
            baseline_ids: inner.running.keys().copied().collect(),
        }
    }

    /// Reap background processes spawned during an *abandoned* turn (user
    /// /stop, session cancel/replacement, deadline abort) — the agent loop
    /// was interrupted, so nobody is left to drive or kill them.
    ///
    /// Kill set: running entries owned by `baseline.session_id` that are
    /// absent from the baseline (i.e. started during the turn). Exemptions
    /// mirror `auto_reap`: kind-exempt (Browser), user pin, never-kill
    /// pattern match. Already-dead PIDs are moved to `finished` as `Dead`
    /// without signaling.
    ///
    /// Generation guard: if [`ProcessRegistry::begin_turn`] has been called
    /// again for this session since `baseline` was captured, a replacement
    /// turn owns the session — this reap returns immediately as a no-op
    /// (`stale_generation = true`) so it cannot kill the replacement turn's
    /// legitimate spawns.
    pub fn reap_abandoned_turn(
        &self,
        baseline: &TurnBaseline,
        never_kill_patterns: &[Regex],
        sigterm_grace_secs: u64,
    ) -> AbandonedTurnReport {
        let now = now_unix();
        let mut report = AbandonedTurnReport::default();
        let candidates: Vec<ProcessEntry> = {
            let inner = self.lock();
            // Generation check happens under the same lock that `begin_turn`
            // writes under, so "stale" and "enumerate" cannot interleave with
            // a replacement turn's claim.
            if inner.turn_generations.get(&baseline.session_id).copied()
                != Some(baseline.generation)
            {
                report.stale_generation = true;
                return report;
            }
            inner
                .running
                .values()
                .filter(|e| e.session_id.as_deref() == Some(baseline.session_id.as_str()))
                .filter(|e| !baseline.baseline_ids.contains(&e.id))
                .cloned()
                .collect()
        };

        for entry in candidates {
            if entry.kind.is_auto_reap_exempt() {
                report.exempted_by_kind += 1;
                continue;
            }
            if entry.pinned {
                report.exempted_by_pin += 1;
                continue;
            }
            if matches_any(&entry.command, never_kill_patterns) {
                report.exempted_by_pattern += 1;
                continue;
            }
            // Already gone (or unsignalable pid 0): record as Dead, don't
            // signal — a recycled PID would be an unrelated bystander.
            if entry.pid == 0 || !pid_alive(entry.pid) {
                self.finalize(entry.id, ProcessStatus::Dead, None);
                report.already_gone += 1;
                continue;
            }
            let outcome =
                send_term_then_kill(entry.pid, sigterm_grace_secs, entry.start_fingerprint);
            let Some(mut finished) = finalize_killed(self, entry.id, now) else {
                // Entry finalized concurrently (guard drop, reap_dead) —
                // nothing further to record.
                continue;
            };
            match outcome {
                KillOutcome::TermSufficed | KillOutcome::NeededKill => {
                    tracing::info!(
                        pid = entry.pid,
                        command = %command_head(&entry.command),
                        reason = "abandoned turn",
                        "process_registry: reaped turn-scoped background process"
                    );
                    report.killed.push(finished);
                }
                KillOutcome::SignalFailed => {
                    // Couldn't signal at all — mirror auto_reap and record
                    // Dead so the entry doesn't linger as Running.
                    finished.status = ProcessStatus::Dead;
                    report.killed.push(finished);
                }
                KillOutcome::Recycled => {
                    tracing::warn!(
                        pid = entry.pid,
                        id = entry.id,
                        "process_registry: PID recycled before abandoned-turn reap; skipped kill"
                    );
                    report.recycled_skipped += 1;
                }
            }
        }
        report
    }

    /// Drop finished entries older than `ttl_secs`, and enforce the
    /// `max_entries` LRU bound. Returns the count evicted.
    pub fn prune_finished(&self, now: i64, ttl_secs: i64, max_entries: usize) -> usize {
        let mut inner = self.lock();
        let before = inner.finished.len();
        if ttl_secs > 0 {
            inner.finished.retain(|e| {
                let finished_at = e.finished_at.unwrap_or(now);
                now.saturating_sub(finished_at) < ttl_secs
            });
        }
        while inner.finished.len() > max_entries {
            inner.finished.pop_front();
        }
        before.saturating_sub(inner.finished.len())
    }

    /// Classify the registry for the doctor check. Pinned entries are
    /// counted separately so the doctor surface can downgrade them.
    pub fn classify(&self, now: i64, warn_secs: i64, fail_secs: i64) -> RegistryClassification {
        let mut class = RegistryClassification::default();
        let inner = self.lock();
        for entry in inner.running.values() {
            let age = entry.age_secs(now);
            if entry.pinned {
                class.pinned_long_running += usize::from(age >= warn_secs);
                continue;
            }
            if age >= fail_secs {
                class.failing.push(entry.clone());
            } else if age >= warn_secs {
                class.warning.push(entry.clone());
            }
        }
        class
            .warning
            .sort_by_key(|e| std::cmp::Reverse(e.age_secs(now)));
        class
            .failing
            .sort_by_key(|e| std::cmp::Reverse(e.age_secs(now)));
        class
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Mutex is uncontended in normal operation; only the maintenance
        // sweep takes it for any non-trivial duration. Poisoning would
        // indicate a panic mid-write — recover defensively so the daemon
        // doesn't cascade-fail.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("process_registry: mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    fn finalize(&self, id: u64, status: ProcessStatus, exit_code: Option<i32>) -> bool {
        let mut inner = self.lock();
        let Some(mut entry) = inner.running.remove(&id) else {
            return false;
        };
        entry.status = status;
        entry.exit_code = exit_code;
        entry.finished_at = Some(now_unix());
        entry.pinned = false;
        push_finished(&mut inner.finished, entry);
        true
    }
}

/// Output of [`ProcessRegistry::classify`] — input to the doctor check.
#[derive(Clone, Debug, Default)]
pub struct RegistryClassification {
    /// Non-pinned entries past the warn threshold but below fail.
    pub warning: Vec<ProcessEntry>,
    /// Non-pinned entries past the fail threshold.
    pub failing: Vec<ProcessEntry>,
    /// Pinned entries past the warn threshold; surfaced informationally.
    pub pinned_long_running: usize,
}

/// RAII guard returned by [`ProcessRegistry::register`]. Spawn sites
/// keep this alive for the duration of the child and call `complete` /
/// `timed_out` / `killed` on the relevant control-flow arm.
pub struct ProcessGuard {
    registry: ProcessRegistry,
    id: u64,
    completed: Arc<AtomicBool>,
}

impl ProcessGuard {
    /// The registry id of the tracked entry. Useful for the TUI pin
    /// toggle.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Mark the entry as cleanly exited.
    pub fn complete(self, exit_code: Option<i32>) {
        self.completed.store(true, Ordering::SeqCst);
        self.registry
            .finalize(self.id, ProcessStatus::Exited, exit_code);
    }

    /// Mark the entry as timed out (caller's wall-clock timeout fired).
    pub fn timed_out(self) {
        self.completed.store(true, Ordering::SeqCst);
        self.registry
            .finalize(self.id, ProcessStatus::TimedOut, None);
    }

    /// Mark the entry as explicitly killed.
    pub fn killed(self) {
        self.completed.store(true, Ordering::SeqCst);
        self.registry.finalize(self.id, ProcessStatus::Killed, None);
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::SeqCst) {
            // Spawn site dropped the guard without explicitly completing
            // it — early `?` return, panic unwind. Record as Killed so
            // the audit trail reflects "we did not see this finish."
            self.registry.finalize(self.id, ProcessStatus::Killed, None);
        }
    }
}

fn push_finished(queue: &mut VecDeque<ProcessEntry>, entry: ProcessEntry) {
    queue.push_back(entry);
}

fn truncate_command(mut cmd: String) -> String {
    if cmd.chars().count() <= COMMAND_DISPLAY_MAX_CHARS {
        return cmd;
    }
    let cut = cmd
        .char_indices()
        .nth(COMMAND_DISPLAY_MAX_CHARS)
        .map(|(i, _)| i)
        .unwrap_or_else(|| cmd.len());
    cmd.truncate(cut);
    cmd.push('…');
    cmd
}

fn matches_any(command: &str, patterns: &[Regex]) -> bool {
    patterns.iter().any(|rx| rx.is_match(command))
}

/// Reduce a command to "first token + total length" so two invocations
/// of `cargo build --release` and `cargo build --features foo` collapse
/// into one repeat-offender signal without `cargo build` and
/// `cargo test` getting conflated.
fn normalize_command(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("").to_string();
    let second = command.split_whitespace().nth(1).unwrap_or("");
    if second.is_empty() {
        first
    } else {
        format!("{first} {second}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KillOutcome {
    TermSufficed,
    NeededKill,
    SignalFailed,
    /// The PID was recycled to a different process before we signaled — we
    /// refused to signal it. Our registered process is gone.
    Recycled,
}

/// A kernel-provided process start-time fingerprint used to detect PID reuse:
/// if the same PID later reports a different start time, it is a *different*
/// process that happens to have inherited the number, and must not be signaled.
///
/// Returns `None` when the platform/kernel can't supply it; callers treat
/// `None` as "can't prove reuse" and fall back to liveness-only behavior rather
/// than skipping a real kill.
#[cfg(target_os = "linux")]
fn pid_start_fingerprint(pid: u32) -> Option<u64> {
    // `/proc/<pid>/stat` field 22 (`starttime`) is the process start time in
    // clock ticks since boot — stable for the life of the process, and fresh on
    // reuse. The `comm` field (2nd) can contain spaces and parentheses, so split
    // after the final ')': what remains starts at field 3 (state), making
    // `starttime` the 20th whitespace token (index 19).
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse::<u64>().ok()
}

#[cfg(target_os = "macos")]
fn pid_start_fingerprint(pid: u32) -> Option<u64> {
    use std::mem::{size_of, MaybeUninit};
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: pure FFI. `info` is a correctly-sized, zeroed `proc_bsdinfo`
    // buffer; `proc_pidinfo` writes at most `size` bytes into it and returns the
    // number of bytes written (== size on success, 0/-1 on failure).
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: `proc_pidinfo` filled the whole struct (checked above).
    let info = unsafe { info.assume_init() };
    // The start time (seconds + microseconds) uniquely fingerprints this PID
    // instance; combine both components so two processes started in the same
    // second still differ.
    Some((info.pbi_start_tvsec << 20) ^ info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_start_fingerprint(_pid: u32) -> Option<u64> {
    None
}

/// True if `pid` still refers to the same process instance we registered.
/// Conservative: when either fingerprint is unavailable we return `true` —
/// inability to prove reuse must not cause us to skip a legitimate kill.
fn pid_identity_matches(pid: u32, registered: Option<u64>) -> bool {
    match (registered, pid_start_fingerprint(pid)) {
        (Some(reg), Some(current)) => reg == current,
        _ => true,
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // Existence *and* not a zombie: an exited-but-unreaped child still
    // answers `kill(pid, 0)`, and a registry that believed it would keep the
    // entry `Running` until the pid was finally reaped.
    crate::util::process_tree::pid_is_running(pid)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // Borg targets unix-only for daemon workflows. Non-unix builds keep
    // the registry compiling but skip liveness probing.
    true
}

#[cfg(unix)]
fn send_term_then_kill(pid: u32, grace_secs: u64, fingerprint: Option<u64>) -> KillOutcome {
    if pid == 0 {
        return KillOutcome::SignalFailed;
    }
    // Re-validate identity immediately before SIGTERM: between registration and
    // this sweep the process may have exited and its PID been recycled to an
    // unrelated process. Signaling that would be killing a bystander.
    if !pid_identity_matches(pid, fingerprint) {
        return KillOutcome::Recycled;
    }
    // SAFETY: pure FFI; pid + signal arguments have no memory preconditions.
    let term = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if term != 0 {
        return KillOutcome::SignalFailed;
    }
    // Poll up to `grace_secs` for the process to exit. 100ms granularity
    // keeps the sweep responsive without burning CPU on a busy daemon.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(grace_secs);
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return KillOutcome::TermSufficed;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !pid_alive(pid) {
        return KillOutcome::TermSufficed;
    }
    // Re-validate once more before the hard SIGKILL. The grace window is exactly
    // when our process could have exited and the PID been reused, so escalating
    // to an unconditional SIGKILL here is the highest-risk recycle moment.
    if !pid_identity_matches(pid, fingerprint) {
        return KillOutcome::Recycled;
    }
    // SAFETY: pure FFI; pid + signal arguments have no memory preconditions.
    let kill = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if kill != 0 && pid_alive(pid) {
        return KillOutcome::SignalFailed;
    }
    KillOutcome::NeededKill
}

#[cfg(not(unix))]
fn send_term_then_kill(_pid: u32, _grace_secs: u64, _fingerprint: Option<u64>) -> KillOutcome {
    KillOutcome::SignalFailed
}

fn finalize_killed(registry: &ProcessRegistry, id: u64, now: i64) -> Option<ProcessEntry> {
    let mut inner = registry.lock();
    let mut entry = inner.running.remove(&id)?;
    entry.status = ProcessStatus::Killed;
    entry.finished_at = Some(now);
    entry.pinned = false;
    let clone = entry.clone();
    push_finished(&mut inner.finished, entry);
    Some(clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_age(reg: &ProcessRegistry, id: u64, now: i64) -> i64 {
        reg.running()
            .into_iter()
            .find(|e| e.id == id)
            .map(|e| e.age_secs(now))
            .unwrap_or(-1)
    }

    /// Backdoor for tests that need an ancient entry without sleeping
    /// or stubbing the clock.
    fn force_started_at(reg: &ProcessRegistry, id: u64, when: i64) {
        let mut inner = reg.lock();
        if let Some(e) = inner.running.get_mut(&id) {
            e.started_at = when;
        }
    }

    /// Backdoor to simulate PID recycling: overwrite the registered start-time
    /// fingerprint so it no longer matches the live process at that PID.
    fn force_fingerprint(reg: &ProcessRegistry, id: u64, fp: Option<u64>) {
        let mut inner = reg.lock();
        if let Some(e) = inner.running.get_mut(&id) {
            e.start_fingerprint = fp;
        }
    }

    #[test]
    fn register_assigns_unique_monotonic_ids() {
        let reg = ProcessRegistry::new();
        let g1 = reg.register(111, ProcessKind::RunShell, "echo a", None);
        let g2 = reg.register(222, ProcessKind::RunShell, "echo b", None);
        assert_ne!(g1.id(), g2.id());
        assert!(g2.id() > g1.id(), "ids monotonically increase");
        g1.complete(Some(0));
        let g3 = reg.register(333, ProcessKind::RunShell, "echo c", None);
        assert!(g3.id() > g2.id(), "ids never reused after completion");
    }

    #[test]
    fn complete_moves_entry_running_to_finished() {
        let reg = ProcessRegistry::new();
        let g = reg.register(111, ProcessKind::RunShell, "echo a", None);
        let id = g.id();
        assert_eq!(reg.running().len(), 1);
        g.complete(Some(0));
        assert!(reg.running().is_empty());
        let snap = reg.snapshot();
        let f = snap.finished.iter().find(|e| e.id == id).expect("finished");
        assert_eq!(f.status, ProcessStatus::Exited);
        assert_eq!(f.exit_code, Some(0));
        assert!(f.finished_at.is_some());
    }

    #[test]
    fn dropped_guard_marks_killed() {
        // Spec invariant: if a spawn site forgets to call a terminal
        // method (e.g. early `?` return), the entry MUST still show
        // up in finished as Killed — never linger as Running forever.
        let reg = ProcessRegistry::new();
        let id = {
            let g = reg.register(111, ProcessKind::RunShell, "echo a", None);
            g.id()
            // g dropped here without complete()
        };
        assert!(reg.running().is_empty(), "running drained on drop");
        let snap = reg.snapshot();
        let f = snap.finished.iter().find(|e| e.id == id).expect("found");
        assert_eq!(f.status, ProcessStatus::Killed);
    }

    #[test]
    fn timed_out_is_distinct_from_killed() {
        // The doctor surface formats TimedOut differently from Killed;
        // the lifecycle method must be preserved verbatim.
        let reg = ProcessRegistry::new();
        let g = reg.register(111, ProcessKind::RunShell, "sleep 99", None);
        let id = g.id();
        g.timed_out();
        let snap = reg.snapshot();
        let f = snap.finished.iter().find(|e| e.id == id).expect("found");
        assert_eq!(f.status, ProcessStatus::TimedOut);
    }

    #[test]
    fn reap_dead_marks_vanished_pids() {
        // init (pid 1) is always alive on unix; 0xFFFFFFFE is reliably
        // unallocated. Reap should leave init alone and move the fake
        // PID into finished as Dead.
        #[cfg(unix)]
        {
            let reg = ProcessRegistry::new();
            let _g1 = reg.register(1, ProcessKind::Hook, "init guard", None);
            let _g2 = reg.register(0xFFFF_FFFE, ProcessKind::Hook, "ghost", None);
            assert_eq!(reg.running().len(), 2);
            let reaped = reg.reap_dead(now_unix());
            assert_eq!(reaped, 1, "exactly the ghost was reaped");
            let snap = reg.snapshot();
            assert_eq!(snap.running.len(), 1);
            let ghost = snap
                .finished
                .iter()
                .find(|e| e.pid == 0xFFFF_FFFE)
                .expect("ghost in finished");
            assert_eq!(ghost.status, ProcessStatus::Dead);
            // Don't leave the init guard alive across tests.
            std::mem::forget(_g1); // intentionally; we already verified.
            std::mem::forget(_g2);
        }
    }

    #[test]
    fn prune_finished_respects_ttl() {
        let reg = ProcessRegistry::new();
        let g_old = reg.register(111, ProcessKind::RunShell, "old", None);
        let id_old = g_old.id();
        g_old.complete(Some(0));
        // Backdate the finished_at by hand.
        {
            let mut inner = reg.lock();
            let e = inner
                .finished
                .iter_mut()
                .find(|e| e.id == id_old)
                .expect("found");
            e.finished_at = Some(now_unix() - 10_000);
        }
        let g_new = reg.register(222, ProcessKind::RunShell, "new", None);
        g_new.complete(Some(0));
        // TTL = 1h, max = 256. The 10k-second-old entry exceeds TTL.
        let pruned = reg.prune_finished(now_unix(), 3600, 256);
        assert_eq!(pruned, 1);
        let snap = reg.snapshot();
        assert_eq!(snap.finished.len(), 1);
        assert_ne!(snap.finished[0].id, id_old);
    }

    #[test]
    fn prune_finished_respects_max_entries_lru() {
        let reg = ProcessRegistry::new();
        // Fill 5 entries, max 3 → oldest 2 evicted.
        let mut ids = Vec::new();
        for i in 0..5 {
            let g = reg.register(100 + i, ProcessKind::RunShell, format!("c{i}"), None);
            ids.push(g.id());
            g.complete(Some(0));
        }
        let pruned = reg.prune_finished(now_unix(), 1_000_000, 3);
        assert_eq!(pruned, 2);
        let snap = reg.snapshot();
        assert_eq!(snap.finished.len(), 3);
        // Oldest two should be gone; newest three remain in order.
        let remaining: Vec<u64> = snap.finished.iter().map(|e| e.id).collect();
        assert_eq!(remaining, vec![ids[2], ids[3], ids[4]]);
    }

    #[test]
    fn running_query_excludes_finished() {
        let reg = ProcessRegistry::new();
        let g1 = reg.register(111, ProcessKind::RunShell, "a", None);
        let g2 = reg.register(222, ProcessKind::RunShell, "b", None);
        let id1 = g1.id();
        g1.complete(Some(0));
        let running = reg.running();
        assert_eq!(running.len(), 1);
        assert_ne!(running[0].id, id1);
        g2.complete(Some(0));
    }

    #[test]
    fn classify_drives_warn_and_fail_thresholds() {
        // 3601s old → warn; 14401s old → fail. Pinned entries omitted
        // from warn/fail and counted separately.
        let reg = ProcessRegistry::new();
        let now = now_unix();
        let g_young = reg.register(11, ProcessKind::RunShell, "young", None);
        let g_warn = reg.register(12, ProcessKind::RunShell, "warn-age", None);
        let g_fail = reg.register(13, ProcessKind::RunShell, "fail-age", None);
        let g_pinned = reg.register(14, ProcessKind::RunShell, "pinned-fail", None);
        force_started_at(&reg, g_warn.id(), now - 3601);
        force_started_at(&reg, g_fail.id(), now - 14401);
        force_started_at(&reg, g_pinned.id(), now - 14401);
        reg.pin(g_pinned.id()).expect("pin");

        let class = reg.classify(now, 3600, 14400);
        assert_eq!(class.warning.len(), 1, "{:?}", class.warning);
        assert_eq!(class.warning[0].id, g_warn.id());
        assert_eq!(class.failing.len(), 1);
        assert_eq!(class.failing[0].id, g_fail.id());
        assert_eq!(class.pinned_long_running, 1);
        assert!(entry_age(&reg, g_young.id(), now) >= 0);
        // Cleanup so Drop doesn't attempt to finalize after registry
        // goes out of scope (it's fine either way, this is just hygiene).
        g_young.complete(Some(0));
        g_warn.complete(Some(0));
        g_fail.complete(Some(0));
        g_pinned.complete(Some(0));
    }

    #[test]
    fn auto_reap_disabled_when_max_age_zero() {
        let reg = ProcessRegistry::new();
        let g = reg.register(123, ProcessKind::RunShell, "ancient", None);
        force_started_at(&reg, g.id(), now_unix() - 999_999);
        let policy = AutoReapPolicy {
            max_age_secs: 0,
            never_kill_patterns: vec![],
            sigterm_grace_secs: 1,
        };
        let rep = reg.auto_reap(now_unix(), &policy);
        assert_eq!(rep.killed_count(), 0);
        assert_eq!(rep.exempted_count(), 0);
        g.complete(Some(0));
    }

    #[test]
    fn auto_reap_exempts_browser_kind() {
        // Hard-coded invariant: Browser is never auto-killed even if
        // it's been alive for days.
        let reg = ProcessRegistry::new();
        let g = reg.register(0, ProcessKind::Browser, "chromium", None);
        force_started_at(&reg, g.id(), now_unix() - 999_999);
        let policy = AutoReapPolicy {
            max_age_secs: 60,
            never_kill_patterns: vec![],
            sigterm_grace_secs: 1,
        };
        let rep = reg.auto_reap(now_unix(), &policy);
        assert_eq!(rep.killed_count(), 0);
        assert_eq!(rep.exempted_by_kind, 1);
        g.complete(Some(0));
    }

    #[test]
    fn auto_reap_exempts_pinned_entry() {
        let reg = ProcessRegistry::new();
        let g = reg.register(99999, ProcessKind::RunShell, "long-build", None);
        force_started_at(&reg, g.id(), now_unix() - 999_999);
        reg.pin(g.id()).expect("pin");
        let policy = AutoReapPolicy {
            max_age_secs: 60,
            never_kill_patterns: vec![],
            sigterm_grace_secs: 1,
        };
        let rep = reg.auto_reap(now_unix(), &policy);
        assert_eq!(rep.killed_count(), 0);
        assert_eq!(rep.exempted_by_pin, 1);
        g.complete(Some(0));
    }

    #[test]
    fn auto_reap_exempts_pattern_match_but_not_close_cousin() {
        // `cargo build*` matches `cargo build --release` but must NOT
        // match `cargo-incompatible-tool` — the glob is anchored.
        let reg = ProcessRegistry::new();
        let g_match = reg.register(11, ProcessKind::RunShell, "cargo build --release", None);
        let g_cousin = reg.register(12, ProcessKind::RunShell, "cargo-incompatible-tool", None);
        force_started_at(&reg, g_match.id(), now_unix() - 999_999);
        force_started_at(&reg, g_cousin.id(), now_unix() - 999_999);
        let policy = AutoReapPolicy {
            max_age_secs: 60,
            never_kill_patterns: compile_never_kill_patterns(&["cargo build*".into()]),
            sigterm_grace_secs: 1,
        };
        let rep = reg.auto_reap(now_unix(), &policy);
        assert_eq!(rep.exempted_by_pattern, 1);
        // The cousin should have been a kill candidate. We don't have a
        // real PID to TERM, so `send_term_then_kill` returns SignalFailed
        // → cousin moves to finished as Dead.
        assert_eq!(rep.killed_count(), 1);
        let snap = reg.snapshot();
        assert!(snap.running.iter().any(|e| e.id == g_match.id()));
        assert!(!snap.running.iter().any(|e| e.id == g_cousin.id()));
        g_match.complete(Some(0));
        // g_cousin already finalized; its Drop is a no-op on a missing id.
        drop(g_cousin);
    }

    #[test]
    fn auto_reap_kills_real_child() {
        // Spawn a real `sh -c sleep 99`, age it, auto-reap, verify the
        // PID is no longer alive.
        #[cfg(unix)]
        {
            use std::process::{Command, Stdio};
            let reg = ProcessRegistry::new();
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("sleep 99")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sh");
            let pid = child.id();
            let g = reg.register(pid, ProcessKind::RunShell, "sleep 99", None);
            force_started_at(&reg, g.id(), now_unix() - 999_999);
            let policy = AutoReapPolicy {
                max_age_secs: 60,
                never_kill_patterns: vec![],
                sigterm_grace_secs: 2,
            };
            let rep = reg.auto_reap(now_unix(), &policy);
            assert_eq!(rep.killed_count(), 1, "real child was reaped");
            // The process must actually be dead.
            let _ = child.wait();
            assert!(!pid_alive(pid), "PID still alive after auto_reap");
            // The guard was finalized by auto_reap, so completing now
            // is a no-op.
            drop(g);
        }
    }

    #[test]
    fn auto_reap_skips_recycled_pid_and_spares_the_bystander() {
        // A registered process whose PID has been recycled to a *different*
        // process must NOT be signaled. We simulate the recycle by corrupting
        // the stored start-time fingerprint so it can't match the live process,
        // then assert the real process survives the sweep.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::process::{Command, Stdio};
            let reg = ProcessRegistry::new();
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("sleep 99")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sh");
            let pid = child.id();
            // Sanity: the platform must actually produce a fingerprint, else the
            // guard can't engage and the test would be vacuous.
            assert!(
                pid_start_fingerprint(pid).is_some(),
                "platform must supply a start-time fingerprint for this test to be meaningful"
            );
            let g = reg.register(pid, ProcessKind::RunShell, "sleep 99", None);
            force_started_at(&reg, g.id(), now_unix() - 999_999);
            // Stamp a fingerprint that cannot match the live process → "recycled".
            force_fingerprint(&reg, g.id(), Some(0xDEAD_BEEF_DEAD_BEEF));

            let policy = AutoReapPolicy {
                max_age_secs: 60,
                never_kill_patterns: vec![],
                sigterm_grace_secs: 2,
            };
            let rep = reg.auto_reap(now_unix(), &policy);

            assert_eq!(
                rep.recycled_skipped, 1,
                "recycle must be detected and counted"
            );
            assert_eq!(
                rep.killed_count(),
                0,
                "no process may be killed on a recycle"
            );
            assert!(
                pid_alive(pid),
                "the bystander process must survive a recycled-PID sweep"
            );

            // Clean up the real child ourselves.
            let _ = child.kill();
            let _ = child.wait();
            drop(g);
        }
    }

    #[test]
    fn pid_identity_matches_falls_back_to_true_without_fingerprints() {
        // When the registered fingerprint is unknown we cannot prove reuse, so
        // we must NOT skip a legitimate kill — identity check returns true.
        assert!(pid_identity_matches(99999, None));
    }

    #[test]
    fn auto_reap_records_normalized_commands_for_next_sweep() {
        // The maintenance sweep diffs `previous_auto_reaped_commands`
        // across sweeps; auto_reap is responsible for stashing them.
        let reg = ProcessRegistry::new();
        let g = reg.register(
            0xFEED,
            ProcessKind::RunShell,
            "rustc --emit=link foo.rs",
            None,
        );
        force_started_at(&reg, g.id(), now_unix() - 999_999);
        let policy = AutoReapPolicy {
            max_age_secs: 60,
            never_kill_patterns: vec![],
            sigterm_grace_secs: 0,
        };
        let _ = reg.auto_reap(now_unix(), &policy);
        let last = reg.previous_auto_reaped_commands();
        assert_eq!(last, vec!["rustc --emit=link".to_string()]);
        drop(g);
    }

    /// Spawn a throwaway `sh -c sleep 99` for reap tests.
    #[cfg(unix)]
    fn spawn_sleeper() -> std::process::Child {
        use std::process::{Command, Stdio};
        Command::new("sh")
            .arg("-c")
            .arg("sleep 99")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh")
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_turn_reaps_turn_spawned_but_spares_baseline() {
        // A process running BEFORE the turn started (in the baseline) must
        // survive the abandon-reap; one spawned DURING the turn must die.
        let reg = ProcessRegistry::new();
        let sess = Some("sess-a".to_string());
        let mut pre = spawn_sleeper();
        let g_pre = reg.register(
            pre.id(),
            ProcessKind::RunShell,
            "sleep 99 #pre",
            sess.clone(),
        );

        let baseline = reg.begin_turn("sess-a");
        assert!(baseline.baseline_ids.contains(&g_pre.id()));

        let mut spawned = spawn_sleeper();
        let g_spawned = reg.register(
            spawned.id(),
            ProcessKind::RunShell,
            "sleep 99 #turn",
            sess.clone(),
        );

        let report = reg.reap_abandoned_turn(&baseline, &[], 2);
        assert_eq!(report.killed.len(), 1, "exactly the turn-spawned process");
        assert_eq!(report.killed[0].id, g_spawned.id());
        assert!(!report.stale_generation);
        let _ = spawned.wait();
        assert!(
            !pid_alive(spawned.id()),
            "turn-spawned process must be dead"
        );
        assert!(pid_alive(pre.id()), "baseline process must survive");
        // The reaped entry left the running set; the baseline one didn't.
        let running_ids = reg.snapshot_running_ids();
        assert!(running_ids.contains(&g_pre.id()));
        assert!(!running_ids.contains(&g_spawned.id()));

        let _ = pre.kill();
        let _ = pre.wait();
        drop(g_spawned); // finalized by the reap; Drop is a no-op
        g_pre.complete(Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn completed_turn_processes_survive_next_turns_abandonment() {
        // Normal completion performs no reap — and once the next turn begins,
        // the survivor sits in that turn's baseline, so even an abandonment
        // of the NEXT turn leaves it alone.
        let reg = ProcessRegistry::new();
        let sess = Some("sess-b".to_string());
        let _turn1 = reg.begin_turn("sess-b");
        let mut server = spawn_sleeper();
        let g = reg.register(server.id(), ProcessKind::RunShell, "dev server", sess);
        // Turn 1 completes normally: no reap call, baseline dropped.

        let turn2 = reg.begin_turn("sess-b");
        let report = reg.reap_abandoned_turn(&turn2, &[], 2);
        assert!(report.killed.is_empty());
        assert!(
            pid_alive(server.id()),
            "process from a completed turn must survive later abandonment"
        );
        let _ = server.kill();
        let _ = server.wait();
        g.complete(Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn stale_generation_reap_is_noop_even_with_new_processes() {
        // The dangerous race: turn 1 is abandoned, but before its reaper
        // enumerates, a replacement turn claims the session and spawns a
        // legitimate process. The stale reaper must touch NOTHING.
        let reg = ProcessRegistry::new();
        let sess = Some("sess-c".to_string());
        let turn1 = reg.begin_turn("sess-c");
        let orphan = spawn_sleeper();
        let g_orphan = reg.register(orphan.id(), ProcessKind::RunShell, "orphan", sess.clone());

        // Replacement turn claims the session and spawns its own process.
        let _turn2 = reg.begin_turn("sess-c");
        let fresh = spawn_sleeper();
        let g_fresh = reg.register(fresh.id(), ProcessKind::RunShell, "fresh", sess);

        let report = reg.reap_abandoned_turn(&turn1, &[], 2);
        assert!(report.stale_generation, "stale reap must report itself");
        assert!(report.killed.is_empty(), "stale reap must kill nothing");
        assert!(pid_alive(orphan.id()));
        assert!(pid_alive(fresh.id()));

        for (mut child, guard) in [(orphan, g_orphan), (fresh, g_fresh)] {
            let _ = child.kill();
            let _ = child.wait();
            guard.complete(Some(0));
        }
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_turn_reap_honors_pin_and_never_kill() {
        let reg = ProcessRegistry::new();
        let sess = Some("sess-d".to_string());
        let baseline = reg.begin_turn("sess-d");

        let pinned = spawn_sleeper();
        let g_pin = reg.register(pinned.id(), ProcessKind::RunShell, "pin me", sess.clone());
        reg.pin(g_pin.id()).expect("pin");
        let allowed = spawn_sleeper();
        let g_allow = reg.register(
            allowed.id(),
            ProcessKind::RunShell,
            "cargo build --release",
            sess,
        );

        let patterns = compile_never_kill_patterns(&["cargo build*".into()]);
        let report = reg.reap_abandoned_turn(&baseline, &patterns, 2);
        assert_eq!(report.killed.len(), 0);
        assert_eq!(report.exempted_by_pin, 1);
        assert_eq!(report.exempted_by_pattern, 1);
        assert!(pid_alive(pinned.id()), "pinned process must survive");
        assert!(pid_alive(allowed.id()), "never-kill process must survive");

        for (mut child, guard) in [(pinned, g_pin), (allowed, g_allow)] {
            let _ = child.kill();
            let _ = child.wait();
            guard.complete(Some(0));
        }
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_turn_reap_skips_already_dead_and_other_sessions() {
        let reg = ProcessRegistry::new();
        let baseline = reg.begin_turn("sess-e");

        // Turn-spawned but already dead: excluded from the kill sweep,
        // recorded Dead.
        let g_ghost = reg.register(
            0xFFFF_FFFE,
            ProcessKind::RunShell,
            "ghost",
            Some("sess-e".into()),
        );
        // A concurrent session's process, spawned mid-turn: not ours to kill.
        let mut other = spawn_sleeper();
        let g_other = reg.register(
            other.id(),
            ProcessKind::RunShell,
            "other-session",
            Some("sess-OTHER".into()),
        );

        let report = reg.reap_abandoned_turn(&baseline, &[], 2);
        assert_eq!(report.already_gone, 1);
        assert!(report.killed.is_empty());
        assert!(pid_alive(other.id()), "other session's process untouched");
        let snap = reg.snapshot();
        let ghost = snap
            .finished
            .iter()
            .find(|e| e.id == g_ghost.id())
            .expect("ghost moved to finished");
        assert_eq!(ghost.status, ProcessStatus::Dead);

        let _ = other.kill();
        let _ = other.wait();
        g_other.complete(Some(0));
        drop(g_ghost); // finalized by the reap; Drop is a no-op
    }

    #[test]
    fn begin_turn_generations_are_per_session() {
        // Generations advance independently per session — session B's turns
        // must not stale-out session A's baseline.
        let reg = ProcessRegistry::new();
        let a1 = reg.begin_turn("a");
        let _b1 = reg.begin_turn("b");
        let _b2 = reg.begin_turn("b");
        let report = reg.reap_abandoned_turn(&a1, &[], 1);
        assert!(
            !report.stale_generation,
            "session b's turns must not invalidate session a's baseline"
        );
        let a2 = reg.begin_turn("a");
        assert_eq!(a2.generation, a1.generation + 1);
        let report = reg.reap_abandoned_turn(&a1, &[], 1);
        assert!(report.stale_generation);
    }

    #[test]
    fn pin_invalid_id_returns_err() {
        let reg = ProcessRegistry::new();
        let err = reg.pin(424242).unwrap_err();
        assert!(err.to_string().contains("unknown id"), "got: {err}",);
    }

    #[test]
    fn pin_cleared_when_process_exits() {
        // Pin a running entry, complete it, and verify the finished
        // snapshot shows pinned = false. Otherwise a leftover pin
        // would silently exempt a future entry that reused the id
        // (it can't — ids are monotonic — but the cleanup is still
        // the right contract).
        let reg = ProcessRegistry::new();
        let g = reg.register(111, ProcessKind::RunShell, "x", None);
        let id = g.id();
        reg.pin(id).expect("pin");
        g.complete(Some(0));
        let snap = reg.snapshot();
        let f = snap.finished.iter().find(|e| e.id == id).expect("found");
        assert!(!f.pinned);
    }

    #[test]
    fn concurrent_registration_is_safe() {
        use std::thread;
        let reg = ProcessRegistry::new();
        let mut handles = Vec::new();
        for i in 0..100 {
            let r = reg.clone();
            handles.push(thread::spawn(move || {
                let g = r.register(1000 + i, ProcessKind::RunShell, format!("c{i}"), None);
                let id = g.id();
                g.complete(Some(0));
                id
            }));
        }
        let mut ids: Vec<u64> = handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 100, "every registration produced a unique id");
    }

    #[test]
    fn glob_to_regex_anchors_match() {
        let rx = glob_to_regex("cargo build*").expect("compile");
        assert!(rx.is_match("cargo build"));
        assert!(rx.is_match("cargo build --release"));
        assert!(!rx.is_match("cargo-incompatible-tool"));
        assert!(!rx.is_match("prefix cargo build"));
    }

    #[test]
    fn compile_never_kill_patterns_skips_bad_entries() {
        // Empty entries dropped without crashing the whole list.
        let compiled = compile_never_kill_patterns(&["".into(), "make*".into()]);
        assert_eq!(compiled.len(), 1);
    }

    #[test]
    fn command_is_truncated_with_ellipsis() {
        let long = "x".repeat(COMMAND_DISPLAY_MAX_CHARS + 10);
        let reg = ProcessRegistry::new();
        let g = reg.register(111, ProcessKind::RunShell, long.clone(), None);
        let cmd = reg.running()[0].command.clone();
        assert!(cmd.ends_with('…'));
        assert!(cmd.chars().count() <= COMMAND_DISPLAY_MAX_CHARS + 1);
        g.complete(Some(0));
    }

    #[test]
    fn default_patterns_match_expected_workloads() {
        // Guards against silent default changes — if someone trims the
        // default allowlist, a known long-running workload would start
        // getting reaped at 24h.
        let compiled = compile_never_kill_patterns(&default_never_kill_patterns());
        for cmd in [
            "cargo build --release",
            "cargo test --workspace",
            "cargo run",
            "cargo clippy -- -D warnings",
            "make -j8",
            "pytest -x",
            "npm run build",
            "bun test",
        ] {
            assert!(
                matches_any(cmd, &compiled),
                "default patterns should exempt {cmd}"
            );
        }
        assert!(!matches_any("rm -rf /", &compiled));
    }
}
