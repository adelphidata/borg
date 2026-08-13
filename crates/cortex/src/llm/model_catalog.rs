//! Curated model catalog: embedded floor + remotely-refreshable manifest.
//!
//! Three tiers, in strictly decreasing trust:
//!
//! 1. **Embedded snapshot** (`crates/cortex/data/model-catalog.json`, compiled
//!    in via `include_str!`). Always present, needs no network, and is the
//!    reason a fresh airgapped install can finish onboarding. Every other tier
//!    can fail without the picker ever going empty.
//! 2. **Disk cache** (`~/.borg/cache/model-catalog.json`), written by a
//!    background refresh. Survives restarts so the network is hit at most once
//!    per TTL rather than once per launch.
//! 3. **Remote manifest**, fetched in the background from the release repo.
//!    Lets a maintainer rotate the recommended model without shipping a binary.
//!
//! The manifest deliberately carries **only curation** — ids, display labels,
//! and which model is the recommended default. Context windows live in
//! [`crate::llm::model_registry`] and rates in [`crate::pricing`], because those
//! are facts about a model rather than a product opinion, and a stale opinion is
//! harmless where a stale context window silently corrupts token budgets.
//!
//! Nothing here is ever on the boot path. [`curated`] and [`default_model`] read
//! process state only; the network is touched by [`spawn_refresh`], which
//! returns immediately and can fail without a user-visible effect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Schema version this build understands. A manifest declaring anything higher
/// is rejected outright rather than parsed leniently — a future breaking change
/// must not be silently half-applied by an old binary.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Compiled-in catalog. The offline floor; never empty, never fetched.
const EMBEDDED_MANIFEST: &str = include_str!("../../data/model-catalog.json");

/// Primary manifest URL, served from the release repo's default branch.
const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/adelphidata/borg/main/crates/cortex/data/model-catalog.json";

/// How long a fetched manifest counts as fresh. Past this the next
/// [`spawn_refresh`] re-fetches; the stale copy keeps serving meanwhile.
const REFRESH_TTL: Duration = Duration::from_secs(60 * 60);

/// After a failed fetch, suppress further attempts for this long so a blackholed
/// network can't turn every `/model` invocation into a timeout.
const FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Hard cap on a single fetch. Short because nothing waits on it — a slow
/// manifest is indistinguishable from an absent one.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// One curated model as authored in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CatalogEntry {
    /// Provider-native model id, exactly as it goes on the wire.
    pub id: String,
    /// Human display name, without any badge suffix.
    pub label: String,
    /// Optional short qualifier rendered in parentheses (`"reasoning"`,
    /// `"fast, cheap"`). Ignored for the default entry, which renders
    /// `(recommended)` instead — two parentheticals read as noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Marks the provider's recommended pick. At most one per provider;
    /// validation rejects a manifest that labels two.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

/// One provider's curated list. Order is meaningful — it is the picker order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderBlock {
    pub models: Vec<CatalogEntry>,
}

/// The manifest as authored and as served. Unknown fields at every level are
/// ignored on purpose, so annotations can be added without a version bump.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    pub providers: std::collections::BTreeMap<String, ProviderBlock>,
}

impl Manifest {
    /// Reject anything this build cannot faithfully serve.
    ///
    /// Validation is deliberately strict: a manifest is remote input that
    /// drives what model a user's requests are billed against, so a
    /// half-understood one must fall back to the embedded floor rather than
    /// partially apply.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version == 0 || self.version > SUPPORTED_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported catalog schema version {} (this build supports {})",
                self.version,
                SUPPORTED_SCHEMA_VERSION
            );
        }
        if self.providers.is_empty() {
            anyhow::bail!("catalog manifest declares no providers");
        }
        for (provider, block) in &self.providers {
            if block.models.is_empty() {
                anyhow::bail!("provider `{provider}` has an empty model list");
            }
            let mut defaults = 0usize;
            for entry in &block.models {
                validate_model_id(&entry.id)
                    .map_err(|e| anyhow::anyhow!("provider `{provider}`: {e}"))?;
                if entry.label.trim().is_empty() {
                    anyhow::bail!("provider `{provider}`: model `{}` has no label", entry.id);
                }
                if entry.default {
                    defaults += 1;
                }
            }
            if defaults > 1 {
                anyhow::bail!(
                    "provider `{provider}` marks {defaults} models as default; expected at most 1"
                );
            }
        }
        Ok(())
    }

    /// Curated `(id, label)` pairs for `provider`, in authored order.
    pub fn curated(&self, provider: &str) -> Vec<(String, String)> {
        self.providers
            .get(provider)
            .map(|b| {
                b.models
                    .iter()
                    .map(|e| (e.id.clone(), render_label(e, false)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Full curated entries for `provider`, in authored order.
    pub fn curated_entries(&self, provider: &str) -> Vec<CatalogEntry> {
        self.providers
            .get(provider)
            .map(|b| b.models.clone())
            .unwrap_or_default()
    }

    /// The provider's recommended model id, if it declares one.
    pub fn default_model(&self, provider: &str) -> Option<String> {
        self.providers
            .get(provider)?
            .models
            .iter()
            .find(|e| e.default)
            .map(|e| e.id.clone())
    }

    /// Every provider key this manifest knows, sorted.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

/// Lay a fetched manifest over the compiled-in floor.
///
/// A provider present in `remote` replaces the embedded block wholesale, so a
/// maintainer can still retire an id or move the recommended pick. A provider
/// *absent* from `remote` keeps its embedded block.
///
/// Without this, a manifest naming only `{"foo": …}` validates cleanly and then
/// empties every real picker — `curated("anthropic")` goes to zero rows, the
/// unknown-provider fallback lands on an equally empty `openrouter`, and the
/// disk cache makes it survive restarts. That is the module's stated invariant
/// ("every other tier can fail without the picker ever going empty") failing to
/// a manifest that never had to be malformed to do it. Remote curation may
/// reshape a provider; it may not delete one.
fn overlay_on_floor(remote: Manifest) -> Manifest {
    let mut merged = embedded().clone();
    merged.version = remote.version;
    merged.updated_at = remote.updated_at;
    for (provider, block) in remote.providers {
        merged.providers.insert(provider, block);
    }
    merged
}

/// Model ids are interpolated into generated TOML and sent as URL path
/// segments, so the character set is pinned rather than trusted.
///
/// The character set alone is not enough. `.` and `/` are both legitimate in
/// real ids (`gpt-4.1`, `deepseek/deepseek-v4-flash`), but a URL parser
/// collapses dot segments before the request leaves the process, so an id of
/// `../../../../upload/v1/files` rewrites which endpoint the user's API key is
/// presented to. The blast radius stays on the provider's own host, so this is
/// endpoint selection rather than credential theft — but the manifest is remote
/// input, and remote input does not get to choose the path.
pub fn validate_model_id(model_id: &str) -> anyhow::Result<()> {
    if model_id.is_empty() {
        anyhow::bail!("model id cannot be empty");
    }
    if !model_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '.' | '_' | ':'))
    {
        anyhow::bail!("model id contains invalid characters: {model_id}");
    }
    if model_id.starts_with('/') {
        anyhow::bail!("model id cannot start with `/`: {model_id}");
    }
    for segment in model_id.split('/') {
        if segment.is_empty() {
            anyhow::bail!("model id has an empty path segment: {model_id}");
        }
        if segment == "." || segment == ".." {
            anyhow::bail!("model id has a relative path segment: {model_id}");
        }
    }
    Ok(())
}

/// Compose the string a picker shows for `entry`.
///
/// `free` comes from a live provider catalog (OpenRouter publishes per-model
/// pricing), not from the manifest — a model's price is not curation.
pub fn render_label(entry: &CatalogEntry, free: bool) -> String {
    let mut out = entry.label.clone();
    if entry.default {
        out.push_str(" (recommended)");
    } else if let Some(note) = entry.note.as_deref().filter(|n| !n.trim().is_empty()) {
        out.push_str(&format!(" ({note})"));
    }
    if free {
        out.push_str(" (free)");
    }
    out
}

/// Parse and validate the compiled-in snapshot.
///
/// Panics only if the shipped JSON is malformed, which is a build-time defect
/// caught by [`tests::embedded_manifest_is_valid`] — there is no runtime path
/// that can reach a bad embedded manifest.
fn embedded() -> &'static Manifest {
    // The input is compiled into the binary, so neither call can fail at
    // runtime for anything an operator did — a bad manifest is a build defect,
    // caught by `embedded_manifest_is_valid`. There is no sensible fallback
    // either: this *is* the fallback every other tier degrades to.
    #[allow(clippy::expect_used)]
    static EMBEDDED: LazyLock<Manifest> = LazyLock::new(|| {
        let parsed: Manifest = serde_json::from_str(EMBEDDED_MANIFEST)
            .expect("embedded model-catalog.json must parse");
        parsed
            .validate()
            .expect("embedded model-catalog.json must validate");
        parsed
    });
    &EMBEDDED
}

#[derive(Debug)]
struct CatalogState {
    manifest: Manifest,
    /// When `manifest` was obtained. `None` means it is the embedded floor,
    /// which is never fresh and never expires.
    fetched_at: Option<SystemTime>,
    last_failure: Option<SystemTime>,
}

static STATE: LazyLock<RwLock<CatalogState>> = LazyLock::new(|| {
    // Overlaid on the way in as well as on the fetch path: the cache file is
    // user-writable, so a hand-edited or downgraded one must not be able to
    // drop a provider the binary ships.
    let (manifest, fetched_at) = load_disk_cache()
        .map(|(m, t)| (overlay_on_floor(m), Some(t)))
        .unwrap_or_else(|| (embedded().clone(), None));
    RwLock::new(CatalogState {
        manifest,
        fetched_at,
        last_failure: None,
    })
});

/// Guards against two concurrent background refreshes doing the same work.
static REFRESH_INFLIGHT: AtomicBool = AtomicBool::new(false);

fn cache_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::config::Config::data_dir()?
        .join("cache")
        .join("model-catalog.json"))
}

fn load_disk_cache() -> Option<(Manifest, SystemTime)> {
    match cache_path() {
        Ok(path) => load_disk_cache_at(&path),
        Err(e) => {
            tracing::warn!("cannot resolve borg data dir; using embedded model catalog: {e}");
            None
        }
    }
}

/// Read the disk cache, returning the manifest and its mtime.
///
/// Any failure — missing file, unreadable, malformed, schema too new — yields
/// `None` so the caller falls back to the embedded floor. A corrupt cache must
/// degrade, never crash: the file lives in a user-writable directory, so its
/// contents are untrusted input.
fn load_disk_cache_at(path: &std::path::Path) -> Option<(Manifest, SystemTime)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let manifest: Manifest = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("model catalog disk cache is malformed: {e}; using embedded catalog");
            return None;
        }
    };
    if let Err(e) = manifest.validate() {
        tracing::warn!("model catalog disk cache failed validation: {e}; using embedded catalog");
        return None;
    }
    Some((manifest, mtime))
}

fn store_disk_cache(manifest: &Manifest) {
    let Ok(path) = cache_path() else {
        tracing::warn!("cannot resolve borg data dir; skipping model catalog cache write");
        return;
    };
    store_disk_cache_at(&path, manifest);
}

/// Persist a fetched manifest. Best-effort: the cache is an optimization, so a
/// write failure is logged and swallowed rather than propagated to a caller
/// that has already succeeded.
fn store_disk_cache_at(path: &std::path::Path, manifest: &Manifest) {
    let Some(dir) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("failed to create model catalog cache dir: {e}");
        return;
    }
    let body = match serde_json::to_string(manifest) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("failed to serialize model catalog: {e}");
            return;
        }
    };
    // Write-then-rename so a crash mid-write can't leave a truncated file that
    // the next boot would have to discard. The pid keeps two borg processes
    // refreshing at once from interleaving into one temp file and renaming the
    // mixed bytes into place.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &body) {
        tracing::warn!("failed to write model catalog cache: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!("failed to commit model catalog cache: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Curated `(id, label)` pairs for `provider`, in picker order.
///
/// Returns an empty vec only when the provider is genuinely absent from the
/// catalog; a provider present in the embedded floor always yields entries,
/// whatever the network did.
pub fn curated(provider: &str) -> Vec<(String, String)> {
    with_manifest(|m| m.curated(provider))
}

/// Full curated entries for `provider`, for callers that need the structured
/// form (e.g. a live catalog filter that recomputes badges).
pub fn curated_entries(provider: &str) -> Vec<CatalogEntry> {
    with_manifest(|m| m.curated_entries(provider))
}

/// The provider's recommended model id, if it declares one.
///
/// Network-free by construction — this is read on resolution paths where a
/// stall would be user-visible.
pub fn default_model(provider: &str) -> Option<String> {
    with_manifest(|m| m.default_model(provider))
}

/// Every provider key the catalog knows, sorted.
pub fn providers() -> Vec<String> {
    with_manifest(Manifest::provider_names)
}

/// The compiled-in floor.
///
/// Exposed so tests — here and in `interface` — can assert against the catalog
/// this binary actually ships. The free accessors above read process state,
/// which on a developer machine or a CI runner that has started borg once is
/// whatever landed in `~/.borg/cache/model-catalog.json`. A guard suite pointed
/// at that is environment noise in both directions: a real regression in the
/// shipped JSON gets masked by a fresh cache, and an upstream manifest edit
/// reds an unrelated diff.
pub fn embedded_manifest() -> &'static Manifest {
    embedded()
}

/// Run `f` against the active manifest, falling back to the embedded floor if
/// the state lock is poisoned. A poisoned lock must not take down onboarding.
fn with_manifest<T>(f: impl FnOnce(&Manifest) -> T) -> T {
    match STATE.read() {
        Ok(guard) => f(&guard.manifest),
        Err(e) => {
            tracing::warn!("model catalog state lock poisoned: {e}; using embedded catalog");
            f(embedded())
        }
    }
}

/// True when a refresh should be attempted now.
///
/// Both `duration_since` calls can fail when a stored timestamp is in the
/// future — a clock that stepped backwards, or a cache file copied from another
/// machine. Every such case resolves to "due", because the alternative is a
/// process that never refreshes again until the clock catches up. A spurious
/// refresh costs one HTTP request; a wedged one costs correctness for the life
/// of the process.
fn should_refresh(state: &CatalogState, now: SystemTime) -> bool {
    if let Some(failed_at) = state.last_failure {
        let since_failure = now.duration_since(failed_at).unwrap_or(FAILURE_BACKOFF);
        if since_failure < FAILURE_BACKOFF {
            return false;
        }
    }
    match state.fetched_at {
        // Embedded floor: always worth trying to improve on.
        None => true,
        Some(at) => now.duration_since(at).unwrap_or(REFRESH_TTL) >= REFRESH_TTL,
    }
}

/// Kick off a background manifest refresh if one is due.
///
/// Returns immediately in every case. No caller waits on the result: the
/// catalog is already serving the best copy it has.
pub fn spawn_refresh() {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let due = match STATE.read() {
        Ok(guard) => should_refresh(&guard, SystemTime::now()),
        Err(e) => {
            tracing::warn!("model catalog state lock poisoned; skipping refresh: {e}");
            false
        }
    };
    if !due {
        return;
    }
    // Claim the slot before spawning so a burst of callers produces one fetch.
    if REFRESH_INFLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    tokio::spawn(async move {
        // Releases the slot on the way out however this task ends. A panic or a
        // runtime shutdown that skipped a bare `store(false)` would wedge the
        // flag set and stop this process ever refreshing again.
        let _slot = InflightSlot;
        let result = fetch_manifest(MANIFEST_URL, FETCH_TIMEOUT).await;
        apply_fetch_result(result);
    });
}

/// Clears [`REFRESH_INFLIGHT`] on drop.
struct InflightSlot;

impl Drop for InflightSlot {
    fn drop(&mut self) {
        REFRESH_INFLIGHT.store(false, Ordering::Release);
    }
}

/// Commit a fetch outcome to process state, recording failures so the backoff
/// window can suppress a retry storm.
fn apply_fetch_result(result: anyhow::Result<Manifest>) {
    match result {
        Ok(manifest) => {
            // Overlay before caching, so the file on disk is already the shape
            // the next boot should serve and the floor is re-applied exactly
            // once rather than compounding across restarts.
            let manifest = overlay_on_floor(manifest);
            store_disk_cache(&manifest);
            commit_manifest(manifest);
        }
        Err(e) => {
            tracing::warn!("model catalog refresh failed: {e}; keeping current catalog");
            record_failure();
        }
    }
}

/// Adopt a fetched manifest as the process-wide catalog and clear the backoff.
fn commit_manifest(manifest: Manifest) {
    match STATE.write() {
        Ok(mut guard) => {
            guard.manifest = manifest;
            guard.fetched_at = Some(SystemTime::now());
            guard.last_failure = None;
        }
        Err(e) => tracing::warn!("model catalog state lock poisoned on write: {e}"),
    }
}

/// Arm the failure backoff. The catalog itself is left alone — a failed refresh
/// keeps serving whatever it already had.
fn record_failure() {
    match STATE.write() {
        Ok(mut guard) => guard.last_failure = Some(SystemTime::now()),
        // Without the timestamp the backoff cannot arm, so a blackholed network
        // would be retried on every call rather than every `FAILURE_BACKOFF`.
        Err(e) => {
            tracing::warn!("model catalog state lock poisoned; refresh backoff not armed: {e}")
        }
    }
}

/// Fetch and validate a manifest. Every failure mode — transport, status,
/// malformed body, unsupported version — surfaces as `Err` so the caller keeps
/// what it already had.
async fn fetch_manifest(url: &str, timeout: Duration) -> anyhow::Result<Manifest> {
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;
    let manifest: Manifest = resp.json().await?;
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, label: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            label: label.to_string(),
            note: None,
            default: false,
        }
    }

    fn manifest_with(models: Vec<CatalogEntry>) -> Manifest {
        Manifest {
            version: 1,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            providers: std::collections::BTreeMap::from([(
                "openrouter".to_string(),
                ProviderBlock { models },
            )]),
        }
    }

    // --- embedded floor -------------------------------------------------

    /// The shipped snapshot is the airgapped guarantee. If it stops parsing or
    /// validating, a fresh install has no picker at all — so this is a build
    /// gate, not a smoke test.
    #[test]
    fn embedded_manifest_is_valid() {
        let parsed: Manifest =
            serde_json::from_str(EMBEDDED_MANIFEST).expect("embedded catalog must parse");
        parsed.validate().expect("embedded catalog must validate");
        assert_eq!(parsed.version, SUPPORTED_SCHEMA_VERSION);
    }

    /// Every provider must offer a recommended pick — onboarding preselects it,
    /// and a provider without one silently lands the user on whatever happens
    /// to be first.
    #[test]
    fn every_embedded_provider_declares_exactly_one_default() {
        for (provider, block) in &embedded().providers {
            let defaults: Vec<&str> = block
                .models
                .iter()
                .filter(|e| e.default)
                .map(|e| e.id.as_str())
                .collect();
            assert_eq!(
                defaults.len(),
                1,
                "provider `{provider}` must declare exactly one default, got {defaults:?}"
            );
        }
    }

    /// The recommended pick the product decided on. Pinned so a catalog edit
    /// that moves it is a deliberate, reviewed change rather than a side effect
    /// of reordering a list.
    #[test]
    fn deepseek_v4_flash_is_the_recommended_default() {
        let floor = embedded_manifest();
        assert_eq!(
            floor.default_model("openrouter").as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
        assert_eq!(
            floor.default_model("deepseek").as_deref(),
            Some("deepseek-v4-flash")
        );
    }

    /// A default must actually be in its own provider's list — otherwise
    /// `default_model` names a model the picker never offers.
    #[test]
    fn embedded_defaults_are_listed_in_their_provider() {
        let floor = embedded_manifest();
        for provider in floor.provider_names() {
            let Some(def) = floor.default_model(&provider) else {
                continue;
            };
            let ids: Vec<String> = floor
                .curated_entries(&provider)
                .into_iter()
                .map(|e| e.id)
                .collect();
            assert!(
                ids.contains(&def),
                "provider `{provider}` default `{def}` is not in its own list"
            );
        }
    }

    // --- validation -----------------------------------------------------

    /// A newer schema must be refused wholesale. Parsing it leniently would let
    /// an old binary apply half of a breaking change.
    #[test]
    fn rejects_future_schema_version() {
        let mut m = manifest_with(vec![entry("a/b", "A B")]);
        m.version = SUPPORTED_SCHEMA_VERSION + 1;
        let err = m.validate().expect_err("future version must be rejected");
        assert!(err.to_string().contains("unsupported catalog schema"));
    }

    #[test]
    fn rejects_zero_schema_version() {
        let mut m = manifest_with(vec![entry("a/b", "A B")]);
        m.version = 0;
        assert!(m.validate().is_err());
    }

    #[test]
    fn rejects_empty_provider_map() {
        let m = Manifest {
            version: 1,
            updated_at: String::new(),
            providers: std::collections::BTreeMap::new(),
        };
        let err = m.validate().expect_err("empty provider map must fail");
        assert!(err.to_string().contains("no providers"));
    }

    /// An empty list would render an empty picker — worse than falling back.
    #[test]
    fn rejects_provider_with_no_models() {
        let m = manifest_with(vec![]);
        let err = m.validate().expect_err("empty model list must fail");
        assert!(err.to_string().contains("empty model list"));
    }

    /// Ids reach generated TOML and request URLs, so injection-shaped ids are
    /// refused rather than escaped.
    ///
    /// The traversal cases are the ones a character allowlist alone misses:
    /// `.` and `/` are both legal in real ids, but a URL parser collapses dot
    /// segments, so `../../../../upload/v1/files` picks a different endpoint on
    /// the provider host with the user's key attached.
    #[test]
    fn rejects_unsafe_model_ids() {
        for bad in [
            "",
            "a b",
            "a\"b",
            "a\nb",
            "a\\b",
            "a$(whoami)",
            "a;rm -rf /",
            "../../../../upload/v1/files",
            "..",
            ".",
            "vendor/../../etc",
            "/leading-slash",
            "vendor//model",
            "vendor/./model",
            "trailing/",
        ] {
            let m = manifest_with(vec![entry(bad, "Label")]);
            assert!(
                m.validate().is_err(),
                "id {bad:?} should be rejected but passed validation"
            );
        }
    }

    #[test]
    fn accepts_realistic_model_ids() {
        for good in [
            "deepseek/deepseek-v4-flash",
            "claude-opus-5",
            "anthropic.claude-sonnet-5",
            "gpt-5.5-pro",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "meta-llama/llama-4-scout-17b-16e-instruct",
        ] {
            let m = manifest_with(vec![entry(good, "Label")]);
            m.validate()
                .unwrap_or_else(|e| panic!("id {good:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn rejects_blank_label() {
        let m = manifest_with(vec![entry("a/b", "   ")]);
        let err = m.validate().expect_err("blank label must fail");
        assert!(err.to_string().contains("no label"));
    }

    /// Two defaults is ambiguous — which one does onboarding preselect? Refuse
    /// rather than pick by list order.
    #[test]
    fn rejects_two_defaults_in_one_provider() {
        let mut a = entry("a/b", "A");
        let mut b = entry("c/d", "C");
        a.default = true;
        b.default = true;
        let err = manifest_with(vec![a, b])
            .validate()
            .expect_err("two defaults must fail");
        assert!(err.to_string().contains("expected at most 1"));
    }

    /// A provider with no default is legal at the schema level (the embedded
    /// floor is held to the stricter rule by its own test).
    #[test]
    fn allows_provider_with_no_default() {
        manifest_with(vec![entry("a/b", "A")])
            .validate()
            .expect("no default is valid");
    }

    /// Annotations must be addable without a version bump, so unknown keys are
    /// dropped rather than treated as a parse failure.
    #[test]
    fn ignores_unknown_fields_at_every_level() {
        let raw = r#"{
            "version": 1,
            "updated_at": "2026-01-01T00:00:00Z",
            "future_top_level": {"anything": true},
            "providers": {
                "openrouter": {
                    "future_provider_key": 7,
                    "models": [
                        {"id": "a/b", "label": "A B", "future_model_key": ["x"]}
                    ]
                }
            }
        }"#;
        let m: Manifest = serde_json::from_str(raw).expect("unknown fields must be ignored");
        m.validate().expect("valid despite unknown fields");
        assert_eq!(m.providers["openrouter"].models[0].id, "a/b");
    }

    /// A manifest round-trips so the disk cache written from a fetched value
    /// reloads identically, and absent optional fields stay absent on the way
    /// out — the `skip_serializing_if` attributes are what is under test, since
    /// a cache that re-emitted `"note": null` and `"default": false` on every
    /// entry would drift from the authored file it mirrors.
    #[test]
    fn manifest_round_trips_and_omits_absent_optional_fields() {
        let mut tagged = entry("a/b", "A B");
        tagged.note = Some("fast".to_string());
        tagged.default = true;
        let plain = entry("c/d", "C D");
        let original = manifest_with(vec![tagged, plain]);

        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Manifest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, original);

        let plain_encoded = serde_json::to_string(&entry("c/d", "C D")).unwrap();
        assert_eq!(plain_encoded, r#"{"id":"c/d","label":"C D"}"#);
    }

    // --- label rendering ------------------------------------------------

    /// Table-driven: the default badge outranks a note, and the live-only
    /// `(free)` tag composes with either.
    #[test]
    fn render_label_composes_badges() {
        let cases: &[(Option<&str>, bool, bool, &str)] = &[
            (None, false, false, "Model"),
            (Some("reasoning"), false, false, "Model (reasoning)"),
            (None, true, false, "Model (recommended)"),
            // default outranks note — two parentheticals read as noise
            (Some("reasoning"), true, false, "Model (recommended)"),
            (None, false, true, "Model (free)"),
            (Some("coding"), false, true, "Model (coding) (free)"),
            (None, true, true, "Model (recommended) (free)"),
            // a whitespace-only note must not render an empty "()"
            (Some("  "), false, false, "Model"),
        ];
        for (note, is_default, free, expected) in cases {
            let e = CatalogEntry {
                id: "a/b".to_string(),
                label: "Model".to_string(),
                note: note.map(str::to_string),
                default: *is_default,
            };
            assert_eq!(
                render_label(&e, *free),
                *expected,
                "note={note:?} default={is_default} free={free}"
            );
        }
    }

    // --- accessors ------------------------------------------------------

    /// Curated order is the picker order, and the default renders with the
    /// recommended badge in place.
    #[test]
    fn curated_preserves_order_and_renders_labels() {
        let floor = embedded_manifest();
        let got = floor.curated("openrouter");
        assert!(
            !got.is_empty(),
            "embedded openrouter list must be non-empty"
        );
        assert_eq!(got[0].0, "deepseek/deepseek-v4-flash");
        assert_eq!(got[0].1, "DeepSeek V4 Flash (recommended)");
        let entries = floor.curated_entries("openrouter");
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        let curated_ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, curated_ids, "both accessors must agree on order");
    }

    /// An unknown provider yields nothing rather than panicking — callers pass
    /// user-supplied provider strings.
    #[test]
    fn unknown_provider_yields_no_models_and_no_default() {
        assert!(curated("not-a-provider").is_empty());
        assert!(curated_entries("not-a-provider").is_empty());
        assert_eq!(default_model("not-a-provider"), None);
    }

    /// Anthropic ids must be the current generation, not a stale one — a
    /// retired id 404s at inference time.
    #[test]
    fn anthropic_list_leads_with_current_generation() {
        let ids: Vec<String> = embedded_manifest()
            .curated_entries("anthropic")
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids.first().map(String::as_str), Some("claude-opus-5"));
        for expected in ["claude-sonnet-5", "claude-haiku-4-5"] {
            assert!(
                ids.iter().any(|i| i == expected),
                "anthropic list is missing {expected}: {ids:?}"
            );
        }
    }

    // --- overlay --------------------------------------------------------

    /// The invariant the module doc claims: no accepted manifest can empty a
    /// picker. A manifest naming only an unrelated provider validates cleanly,
    /// so without the overlay it would commit and take every real list to zero.
    #[test]
    fn a_manifest_that_names_no_real_provider_cannot_empty_the_pickers() {
        let remote = Manifest {
            version: 1,
            updated_at: "2026-09-01T00:00:00Z".to_string(),
            providers: std::collections::BTreeMap::from([(
                "some-new-provider".to_string(),
                ProviderBlock {
                    models: vec![entry("vendor/model", "Model")],
                },
            )]),
        };
        remote
            .validate()
            .expect("this manifest is well-formed — that is the point");

        let merged = overlay_on_floor(remote);
        for provider in embedded_manifest().provider_names() {
            assert!(
                !merged.curated(&provider).is_empty(),
                "provider `{provider}` was emptied by a manifest that never mentioned it"
            );
        }
        assert!(
            !merged.curated("some-new-provider").is_empty(),
            "a genuinely new provider must still come through"
        );
    }

    /// The overlay must not freeze curation: a provider the manifest *does*
    /// name is replaced wholesale, so ids can be retired and the recommended
    /// pick can move without shipping a binary.
    #[test]
    fn a_named_provider_is_replaced_wholesale_not_merged_entrywise() {
        let mut fresh = entry("anthropic/claude-next", "Claude Next");
        fresh.default = true;
        let remote = Manifest {
            version: 1,
            updated_at: "2026-09-01T00:00:00Z".to_string(),
            providers: std::collections::BTreeMap::from([(
                "anthropic".to_string(),
                ProviderBlock {
                    models: vec![fresh],
                },
            )]),
        };

        let merged = overlay_on_floor(remote);
        let ids: Vec<String> = merged
            .curated_entries("anthropic")
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["anthropic/claude-next".to_string()]);
        assert_eq!(
            merged.default_model("anthropic").as_deref(),
            Some("anthropic/claude-next"),
            "the remote manifest must be able to move the recommended pick"
        );
        // Untouched providers keep the floor.
        assert_eq!(
            merged.curated("openai"),
            embedded_manifest().curated("openai")
        );
    }

    /// Applying the overlay to an already-overlaid manifest must be a no-op, or
    /// the disk cache would compound the floor back in on every restart and a
    /// deliberate retirement would resurrect itself.
    #[test]
    fn overlaying_twice_is_the_same_as_overlaying_once() {
        let remote = Manifest {
            version: 1,
            updated_at: "2026-09-01T00:00:00Z".to_string(),
            providers: std::collections::BTreeMap::from([(
                "anthropic".to_string(),
                ProviderBlock {
                    models: vec![entry("anthropic/only-one", "Only One")],
                },
            )]),
        };
        let once = overlay_on_floor(remote);
        let twice = overlay_on_floor(once.clone());
        assert_eq!(once, twice);
    }

    // --- refresh scheduling ---------------------------------------------

    fn state_at(fetched_at: Option<SystemTime>, last_failure: Option<SystemTime>) -> CatalogState {
        CatalogState {
            manifest: embedded().clone(),
            fetched_at,
            last_failure,
        }
    }

    /// Table-driven scheduling rules: refresh when serving the embedded floor
    /// or a stale copy, hold when fresh, and hold inside the failure backoff
    /// regardless of staleness.
    #[test]
    fn should_refresh_respects_ttl_and_backoff() {
        let now = SystemTime::now();
        let ago = |d: Duration| now.checked_sub(d).unwrap();
        let cases: &[(&str, Option<SystemTime>, Option<SystemTime>, bool)] = &[
            ("embedded floor refreshes", None, None, true),
            (
                "fresh copy holds",
                Some(ago(Duration::from_secs(60))),
                None,
                false,
            ),
            (
                "stale copy refreshes",
                Some(ago(REFRESH_TTL + Duration::from_secs(1))),
                None,
                true,
            ),
            (
                "exactly at TTL refreshes",
                Some(ago(REFRESH_TTL)),
                None,
                true,
            ),
            (
                "recent failure suppresses refresh of a stale copy",
                Some(ago(REFRESH_TTL * 2)),
                Some(ago(Duration::from_secs(30))),
                false,
            ),
            (
                "recent failure suppresses refresh of the embedded floor",
                None,
                Some(ago(Duration::from_secs(30))),
                false,
            ),
            (
                "expired backoff allows refresh again",
                None,
                Some(ago(FAILURE_BACKOFF + Duration::from_secs(1))),
                true,
            ),
        ];
        for (name, fetched_at, last_failure, expected) in cases {
            assert_eq!(
                should_refresh(&state_at(*fetched_at, *last_failure), now),
                *expected,
                "{name}"
            );
        }
    }

    /// A clock that stepped backwards (or a cache file copied from another
    /// machine) leaves a timestamp in the future, and `duration_since` fails.
    /// Every such case must resolve to "due": treating it as not-due would wedge
    /// refresh for the life of the process, which is a far worse failure than
    /// one extra HTTP request.
    #[test]
    fn a_future_timestamp_never_wedges_refresh() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        let cases: &[(&str, Option<SystemTime>, Option<SystemTime>)] = &[
            ("fetched in the future", Some(future), None),
            ("failed in the future", None, Some(future)),
            ("both in the future", Some(future), Some(future)),
        ];
        for (name, fetched_at, last_failure) in cases {
            assert!(
                should_refresh(&state_at(*fetched_at, *last_failure), now),
                "{name}: a future timestamp must not suppress refresh"
            );
        }
    }

    // --- disk cache -----------------------------------------------------

    /// The cache is written by one process and read by the next, so a
    /// round-trip that loses a field would silently downgrade a refreshed
    /// catalog back toward the floor.
    #[test]
    fn disk_cache_round_trips_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache").join("model-catalog.json");
        let mut e = entry("vendor/model", "Vendor Model");
        e.note = Some("reasoning".to_string());
        let mut d = entry("vendor/default", "Vendor Default");
        d.default = true;
        let original = manifest_with(vec![d, e]);

        store_disk_cache_at(&path, &original);
        let (loaded, _mtime) = load_disk_cache_at(&path).expect("cache must load back");
        assert_eq!(loaded, original);
    }

    /// The parent directory does not exist on a fresh install. Writing must
    /// create it rather than silently drop every refresh.
    #[test]
    fn disk_cache_write_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("catalog.json");
        store_disk_cache_at(&path, &manifest_with(vec![entry("a/b", "A B")]));
        assert!(path.exists(), "write must create parent directories");
    }

    /// A hand-edited or truncated cache must degrade to the embedded floor.
    /// This file sits in a user-writable directory, so garbage is expected.
    #[test]
    fn corrupt_disk_cache_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(&str, &str)] = &[
            ("not json at all", "}{ this is not json"),
            ("truncated mid-object", r#"{"version": 1, "providers": {"#),
            ("wrong shape", r#"{"hello": "world"}"#),
            ("empty file", ""),
        ];
        for (name, body) in cases {
            let path = dir.path().join(format!("{}.json", name.replace(' ', "_")));
            std::fs::write(&path, body).unwrap();
            assert!(
                load_disk_cache_at(&path).is_none(),
                "{name}: corrupt cache must not load"
            );
        }
    }

    /// A cache written by a newer build is refused for the same reason a
    /// fetched manifest is — an old binary must not half-apply a new schema.
    #[test]
    fn disk_cache_with_future_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let mut m = manifest_with(vec![entry("a/b", "A B")]);
        m.version = SUPPORTED_SCHEMA_VERSION + 1;
        std::fs::write(&path, serde_json::to_string(&m).unwrap()).unwrap();
        assert!(load_disk_cache_at(&path).is_none());
    }

    /// A semantically invalid cache (two defaults) must not reach state either
    /// — validation is applied on the read path, not only on fetch.
    #[test]
    fn disk_cache_failing_validation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let mut a = entry("a/b", "A");
        let mut b = entry("c/d", "C");
        a.default = true;
        b.default = true;
        std::fs::write(
            &path,
            serde_json::to_string(&manifest_with(vec![a, b])).unwrap(),
        )
        .unwrap();
        assert!(load_disk_cache_at(&path).is_none());
    }

    #[test]
    fn missing_disk_cache_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_disk_cache_at(&dir.path().join("absent.json")).is_none());
    }

    /// The happy path renames the temp file away rather than leaving litter.
    #[test]
    fn a_successful_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        store_disk_cache_at(&path, &manifest_with(vec![entry("a/b", "A B")]));
        assert!(path.exists(), "cache file must be committed");
        assert!(
            temp_siblings(dir.path()).is_empty(),
            "temp file must be renamed away, not left behind"
        );
    }

    /// The failure branch itself: rename cannot succeed when the destination is
    /// a directory, so this drives the `remove_file` cleanup the happy-path test
    /// never reaches — and proves a failed refresh cannot litter the cache dir
    /// with a temp file per attempt.
    #[test]
    fn a_failed_commit_removes_its_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the destination makes `rename` fail on every platform
        // we ship, without needing to manipulate permissions.
        let path = dir.path().join("catalog.json");
        std::fs::create_dir(&path).unwrap();

        store_disk_cache_at(&path, &manifest_with(vec![entry("a/b", "A B")]));

        assert!(path.is_dir(), "the blocking directory should be untouched");
        assert!(
            temp_siblings(dir.path()).is_empty(),
            "a failed rename must clean up its temp file, got {:?}",
            temp_siblings(dir.path())
        );
    }

    /// Every `*.tmp` file in `dir`, so the temp-file assertions do not have to
    /// predict the pid embedded in the name.
    fn temp_siblings(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "tmp"))
            .collect()
    }

    // --- commit path ----------------------------------------------------

    /// `commit_manifest`, `record_failure`, and `spawn_refresh` all mutate the
    /// process-wide `STATE`, so they cannot run concurrently with each other.
    static STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the lock and puts `STATE` back the way it was found, so a commit
    /// test cannot leak a manifest into an unrelated test's view.
    struct StateGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: (Manifest, Option<SystemTime>, Option<SystemTime>),
    }

    impl StateGuard {
        fn take() -> Self {
            let lock = STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let g = STATE.read().unwrap();
            let saved = (g.manifest.clone(), g.fetched_at, g.last_failure);
            drop(g);
            Self { _lock: lock, saved }
        }
    }

    impl Drop for StateGuard {
        fn drop(&mut self) {
            let mut g = STATE.write().unwrap_or_else(|e| e.into_inner());
            g.manifest = self.saved.0.clone();
            g.fetched_at = self.saved.1;
            g.last_failure = self.saved.2;
        }
    }

    /// A successful refresh must be what later reads see, must mark the catalog
    /// fresh so the TTL can hold, and must clear any armed backoff.
    #[test]
    fn a_successful_commit_replaces_the_catalog_and_clears_the_backoff() {
        let _guard = StateGuard::take();
        record_failure();
        assert!(
            STATE.read().unwrap().last_failure.is_some(),
            "precondition: backoff armed"
        );

        let fresh = overlay_on_floor(Manifest {
            version: 1,
            updated_at: "2026-09-01T00:00:00Z".to_string(),
            providers: std::collections::BTreeMap::from([(
                "anthropic".to_string(),
                ProviderBlock {
                    models: vec![entry("anthropic/from-the-network", "From The Network")],
                },
            )]),
        });
        commit_manifest(fresh);

        assert_eq!(
            curated_entries("anthropic")
                .into_iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec!["anthropic/from-the-network".to_string()],
            "readers must see the committed manifest"
        );
        let g = STATE.read().unwrap();
        assert!(g.fetched_at.is_some(), "commit must mark the catalog fresh");
        assert!(g.last_failure.is_none(), "commit must clear the backoff");
        drop(g);

        // The floor still fills providers the manifest did not name.
        assert!(!curated("openai").is_empty());
    }

    /// A failed refresh must arm the backoff and leave the served catalog
    /// untouched — losing the network is not a reason to lose the picker.
    #[test]
    fn a_failed_refresh_arms_the_backoff_and_keeps_the_catalog() {
        let _guard = StateGuard::take();
        let before = curated_entries("anthropic");

        apply_fetch_result(Err(anyhow::anyhow!("connection refused")));

        assert_eq!(
            curated_entries("anthropic"),
            before,
            "a failed fetch must not disturb the served catalog"
        );
        let g = STATE.read().unwrap();
        assert!(g.last_failure.is_some(), "failure must arm the backoff");
        assert!(
            !should_refresh(&g, SystemTime::now()),
            "the armed backoff must actually suppress the next attempt"
        );
    }

    /// The `compare_exchange` dedup: while one refresh holds the slot, another
    /// caller must not start a second fetch.
    #[tokio::test]
    async fn a_second_refresh_is_a_no_op_while_one_is_in_flight() {
        let _guard = StateGuard::take();
        // Serving the embedded floor with no recorded failure means "due".
        {
            let mut g = STATE.write().unwrap();
            g.fetched_at = None;
            g.last_failure = None;
        }
        assert!(should_refresh(&STATE.read().unwrap(), SystemTime::now()));

        // Simulate a refresh already running.
        REFRESH_INFLIGHT.store(true, Ordering::Release);
        spawn_refresh();
        assert!(
            REFRESH_INFLIGHT.load(Ordering::Acquire),
            "the in-flight slot must still be held by the first refresh"
        );
        REFRESH_INFLIGHT.store(false, Ordering::Release);
    }

    /// The slot must be released however the task ends, or the process never
    /// refreshes again after a single panicking fetch.
    #[test]
    fn the_inflight_slot_is_released_even_on_unwind() {
        REFRESH_INFLIGHT.store(false, Ordering::Release);
        let result = std::panic::catch_unwind(|| {
            let _slot = InflightSlot;
            REFRESH_INFLIGHT.store(true, Ordering::Release);
            panic!("fetch blew up");
        });
        assert!(result.is_err(), "the panic must have happened");
        assert!(
            !REFRESH_INFLIGHT.load(Ordering::Acquire),
            "a panicking refresh must not wedge the slot set forever"
        );
    }

    /// Nothing may refresh off a runtime — `spawn_refresh` is called from sync
    /// picker code, and `tokio::spawn` panics with no reactor.
    #[test]
    fn refresh_outside_a_runtime_is_a_no_op_rather_than_a_panic() {
        let _guard = StateGuard::take();
        {
            let mut g = STATE.write().unwrap();
            g.fetched_at = None;
            g.last_failure = None;
        }
        REFRESH_INFLIGHT.store(false, Ordering::Release);
        spawn_refresh();
        assert!(
            !REFRESH_INFLIGHT.load(Ordering::Acquire),
            "no runtime means no fetch was started"
        );
    }

    // --- fetch ----------------------------------------------------------

    mod fetch {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn server_returning(body: serde_json::Value, status: u16) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/catalog.json"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            server
        }

        fn url(server: &MockServer) -> String {
            format!("{}/catalog.json", server.uri())
        }

        fn valid_body() -> serde_json::Value {
            serde_json::json!({
                "version": 1,
                "updated_at": "2026-08-10T00:00:00Z",
                "providers": {
                    "openrouter": {
                        "models": [
                            {"id": "vendor/new-model", "label": "New Model", "default": true}
                        ]
                    }
                }
            })
        }

        #[tokio::test]
        async fn fetch_returns_validated_manifest() {
            let server = server_returning(valid_body(), 200).await;
            let got = fetch_manifest(&url(&server), Duration::from_secs(2))
                .await
                .expect("valid manifest must fetch");
            assert_eq!(got.providers["openrouter"].models[0].id, "vendor/new-model");
        }

        #[tokio::test]
        async fn fetch_errors_on_http_500() {
            let server = server_returning(serde_json::json!({}), 500).await;
            let err = fetch_manifest(&url(&server), Duration::from_secs(2))
                .await
                .expect_err("500 must error");
            assert!(
                err.downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_status),
                "expected a status error, got: {err}"
            );
        }

        #[tokio::test]
        async fn fetch_errors_on_malformed_body() {
            let server = server_returning(serde_json::json!({"nope": true}), 200).await;
            let err = fetch_manifest(&url(&server), Duration::from_secs(2))
                .await
                .expect_err("malformed body must error");
            assert!(
                err.downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_decode),
                "expected a decode error, got: {err}"
            );
        }

        /// A well-formed but semantically invalid manifest must be rejected at
        /// the fetch boundary, so it can never reach process state.
        #[tokio::test]
        async fn fetch_rejects_unsupported_schema_version() {
            let mut body = valid_body();
            body["version"] = serde_json::json!(SUPPORTED_SCHEMA_VERSION + 1);
            let server = server_returning(body, 200).await;
            let err = fetch_manifest(&url(&server), Duration::from_secs(2))
                .await
                .expect_err("future schema must error");
            assert!(err.to_string().contains("unsupported catalog schema"));
        }

        #[tokio::test]
        async fn fetch_rejects_unsafe_ids_from_the_network() {
            let mut body = valid_body();
            body["providers"]["openrouter"]["models"][0]["id"] =
                serde_json::json!("evil\"; rm -rf /");
            let server = server_returning(body, 200).await;
            let err = fetch_manifest(&url(&server), Duration::from_secs(2))
                .await
                .expect_err("unsafe id must error");
            assert!(err.to_string().contains("invalid characters"));
        }

        #[tokio::test]
        async fn fetch_errors_on_timeout() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/catalog.json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(valid_body())
                        .set_delay(Duration::from_millis(500)),
                )
                .mount(&server)
                .await;
            let err = fetch_manifest(&url(&server), Duration::from_millis(50))
                .await
                .expect_err("short timeout must error");
            assert!(
                err.downcast_ref::<reqwest::Error>()
                    .is_some_and(|e| e.is_timeout() || e.is_request()),
                "expected a transport error, got: {err}"
            );
        }
    }

    /// The point of a single catalog is that the metadata tables cannot drift
    /// away from it. Every curated id must resolve to a real registry row —
    /// a miss falls silently to `DEFAULT_CONTEXT_WINDOW` and budgets the model
    /// wrong for the rest of its life.
    mod drift_guards {
        use super::*;
        use crate::llm::model_registry;

        /// Always the compiled-in floor, never process state. These guards
        /// assert on the catalog this binary ships; reading `STATE` would make
        /// them depend on whatever `~/.borg/cache/model-catalog.json` happens
        /// to hold on the machine running the suite.
        fn floor() -> &'static Manifest {
            embedded_manifest()
        }

        /// Providers whose ids the direct-cost tables are expected to price.
        /// OpenRouter is excluded by design (its per-route surcharge is not
        /// encoded), Ollama is local and free, Azure routes by deployment
        /// name, and Groq/Bedrock ids are not direct-provider keys.
        const PRICED_PROVIDERS: &[&str] = &["anthropic", "openai", "gemini", "xai", "deepseek"];

        /// Curated ids we knowingly ship without a rate, because no published
        /// rate card covers them yet. Spend on these is invisible to the
        /// budget, so the list is a debt register, not an escape hatch: add the
        /// rate rows and delete the entry as soon as pricing is published.
        const UNPRICED_BY_DESIGN: &[&str] = &[
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.1-codex",
        ];

        #[test]
        fn every_curated_id_resolves_to_an_explicit_registry_row() {
            let mut missing = Vec::new();
            for provider in floor().provider_names() {
                for entry in floor().curated_entries(&provider) {
                    if model_registry::lookup(&entry.id).is_none() {
                        missing.push(format!("{provider}: {}", entry.id));
                    }
                }
            }
            assert!(
                missing.is_empty(),
                "curated ids with no registry row (they would take the default \
                 context window and be budgeted wrong):\n  {}",
                missing.join("\n  ")
            );
        }

        #[test]
        fn every_curated_id_on_a_priced_provider_has_a_rate() {
            let mut missing = Vec::new();
            for provider in PRICED_PROVIDERS {
                for entry in floor().curated_entries(provider) {
                    // A priced model returns a cost for a nonzero token count;
                    // an unpriced one returns `None` and its spend never lands
                    // in the budget.
                    if UNPRICED_BY_DESIGN.contains(&entry.id.as_str()) {
                        continue;
                    }
                    if crate::pricing::estimate_cost(&entry.id, 1_000_000, 0).is_none() {
                        missing.push(format!("{provider}: {}", entry.id));
                    }
                }
            }
            assert!(
                missing.is_empty(),
                "curated ids on a priced provider with no rate row (cost events \
                 are skipped for these):\n  {}",
                missing.join("\n  ")
            );
        }

        /// The debt register must not outlive the debt. An entry that is no
        /// longer curated, or that has since gained a rate, is a stale excuse
        /// that would hide the next genuinely unpriced model.
        #[test]
        fn the_unpriced_register_carries_no_stale_entries() {
            let curated: Vec<String> = PRICED_PROVIDERS
                .iter()
                .flat_map(|p| floor().curated_entries(p))
                .map(|e| e.id)
                .collect();
            for id in UNPRICED_BY_DESIGN {
                assert!(
                    curated.iter().any(|c| c == id),
                    "{id} is on the unpriced register but no longer curated"
                );
                assert!(
                    crate::pricing::estimate_cost(id, 1_000_000, 0).is_none(),
                    "{id} now has a rate — drop it from the unpriced register"
                );
            }
        }

        /// Bedrock ids carry a geo prefix and a `-vN:M` suffix. The registry has
        /// to see through both, or every Bedrock deployment budgets every model
        /// at the default window.
        #[test]
        fn bedrock_shaped_ids_resolve_to_their_real_window() {
            let row = model_registry::lookup("us.anthropic.claude-3-7-sonnet-20250219-v1:0")
                .expect("bedrock id must resolve");
            assert_eq!(row.slug, "claude-3-7-sonnet");
        }

        /// The recommended pick is the one id a fresh install will actually
        /// run, so it has to be complete on every surface.
        #[test]
        fn the_default_pick_is_fully_described_on_every_provider() {
            for provider in floor().provider_names() {
                let id = floor()
                    .default_model(&provider)
                    .unwrap_or_else(|| panic!("{provider} has no default model"));
                let row = model_registry::lookup(&id)
                    .unwrap_or_else(|| panic!("{provider} default {id} has no registry row"));
                assert!(
                    row.context_window > 0 && row.max_context_window >= row.context_window,
                    "{provider} default {id} has an incoherent window: {row:?}"
                );
            }
        }
    }
}
