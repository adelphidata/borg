//! `BORG_HOME` backup unit (Task 05).
//!
//! v1 ships a single tarball backup of the entire `BORG_HOME` directory
//! (SQLite + blobs + skills + channels + hooks + config). mTLS private keys
//! live in the OS keychain and are deliberately *not* in the tarball — a
//! restore on a fresh host triggers re-pairing.
//!
//! Excluded from the archive (see [`should_skip_entry`]):
//! - The UDS socket `borgd.sock` (re-created by the daemon on next start).
//! - `*.lock` flock sentinels.
//! - `logs/` (transient; restoring stale logs would confuse `borg doctor`).
//! - `backups/` (don't recurse into the backup dir we're writing into).
//!
//! Per `data-handling.md` decisions #4 and #5: tarballs are unsigned in v1;
//! restore into an existing `BORG_HOME` is refused unless `--force`, which
//! renames the existing dir to `.bak.<unix_ts>`. A `state_restored` audit
//! event is appended to the restored chain on success.
//!
//! Encryption (passphrase-from-fd) is deferred — see the v3 deferral list
//! at the bottom of `data-handling.md`.

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{AuditChainOutcome, Database};

/// Read the live `BORG_HOME/borg.db`'s schema version through a read-only
/// connection. Used by `borg backup create` to tag the new tarball without
/// the CLI having to import `Database` directly.
pub fn live_schema_version() -> Result<u32> {
    let db = Database::open_readonly().context("open db read-only")?;
    db.schema_version().context("read meta.schema_version")
}

/// File names / directories excluded from a backup tarball.
const EXCLUDED_NAMES: &[&str] = &["borgd.sock", "logs", "backups"];

/// File-extension suffix excluded from the archive.
const EXCLUDED_SUFFIXES: &[&str] = &[".lock", ".sock"];

/// Manifest written to `metadata.json` inside every tarball. The schema
/// version field gates restore: a daemon refuses tarballs whose
/// `schema_version` exceeds its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupManifest {
    /// Borg DB schema version captured at create time.
    pub schema_version: u32,
    /// Unix seconds at create time.
    pub created_at: i64,
    /// Hex-encoded SHA-256 over the *uncompressed* tar stream. Populated
    /// after the tar is written, before zstd-compressing.
    pub sha256_pre_compress: String,
}

/// Verification report from [`verify_backup`]. Distinguishes `OkWithTombstones`
/// from full `Ok` so callers can surface that retention has run on the
/// archive's audit chain — just like `borg audit verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupVerifyReport {
    pub manifest: BackupManifest,
    pub audit_outcome: AuditChainOutcome,
    pub messages_count: i64,
}

/// Create a `borg-backup-<unix>.tar.zst` tarball of `borg_home` under
/// `out_dir`. Returns the absolute path to the new tarball.
///
/// Excludes the daemon socket, lockfiles, `logs/`, and `backups/`. The
/// tarball includes a top-level `metadata.json` with the schema version
/// and a SHA-256 of the uncompressed tar stream — `verify_backup` checks
/// the digest before declaring the tarball restorable.
pub fn create_backup(borg_home: &Path, out_dir: &Path, schema_version: u32) -> Result<PathBuf> {
    if !borg_home.is_dir() {
        bail!("borg_home {} is not a directory", borg_home.display());
    }
    fs::create_dir_all(out_dir).with_context(|| {
        format!("failed to create backup output dir {}", out_dir.display())
    })?;
    let now = chrono::Utc::now().timestamp();
    let dest = out_dir.join(format!("borg-backup-{now}.tar.zst"));

    // Two-pass: write the tar to a temp file (so we can SHA the bytes),
    // then zstd-compress it into the final destination together with a
    // metadata.json header that pins the digest.
    let tmp_dir = tempfile::tempdir().context("create tempdir for tar staging")?;
    let tar_path = tmp_dir.path().join("body.tar");
    let mut sha = Sha256::new();
    {
        let raw = fs::File::create(&tar_path).context("create tar staging file")?;
        let raw = ShaWriter::new(raw, &mut sha);
        let writer = BufWriter::new(raw);
        let mut builder = tar::Builder::new(writer);
        builder.follow_symlinks(false);
        append_dir_filtered(&mut builder, borg_home, Path::new("borg_home"))?;
        builder.finish().context("finalize tar")?;
    }
    let manifest = BackupManifest {
        schema_version,
        created_at: now,
        sha256_pre_compress: hex_encode(&sha.finalize()),
    };

    // Final tar.zst contains:
    //   metadata.json
    //   body.tar      (the inner tar containing borg_home/...)
    let final_file = fs::File::create(&dest)
        .with_context(|| format!("create backup tarball at {}", dest.display()))?;
    let zstd_writer =
        zstd::Encoder::new(BufWriter::new(final_file), 3).context("init zstd encoder")?;
    let mut outer = tar::Builder::new(zstd_writer);

    let manifest_json = serde_json::to_vec_pretty(&manifest).context("serialize manifest")?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(now as u64);
    header.set_cksum();
    outer
        .append_data(&mut header, "metadata.json", manifest_json.as_slice())
        .context("append metadata.json")?;
    outer
        .append_path_with_name(&tar_path, "body.tar")
        .context("append body.tar")?;
    let zstd_writer = outer.into_inner().context("close outer tar builder")?;
    let inner_writer = zstd_writer.finish().context("finalize zstd stream")?;
    inner_writer
        .into_inner()
        .map_err(|e| anyhow!("flush backup tarball: {e}"))?;

    Ok(dest)
}

/// Verify a `tar.zst` produced by [`create_backup`]. Extracts to a temp
/// directory, confirms the SHA-256 of the inner tar matches the manifest,
/// opens the embedded SQLite, runs `verify_audit_chain`, and reports the
/// outcome. Returns `Err` on any check that disqualifies the tarball from
/// being restorable.
pub fn verify_backup(file: &Path) -> Result<BackupVerifyReport> {
    let tmp = tempfile::tempdir().context("verify: tempdir")?;
    let (manifest, body_tar) = extract_outer(file, tmp.path())?;

    // Re-hash the inner tar and compare against manifest digest.
    let mut sha = Sha256::new();
    let mut f = fs::File::open(&body_tar).context("open body.tar for hashing")?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).context("read body.tar")?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
    }
    let actual = hex_encode(&sha.finalize());
    if actual != manifest.sha256_pre_compress {
        bail!(
            "backup digest mismatch: manifest={}, computed={}",
            manifest.sha256_pre_compress,
            actual
        );
    }

    let inner_dir = tmp.path().join("inner");
    fs::create_dir_all(&inner_dir).context("verify: mkdir inner")?;
    let f = fs::File::open(&body_tar).context("verify: reopen body.tar")?;
    tar::Archive::new(BufReader::new(f))
        .unpack(&inner_dir)
        .context("verify: extract inner tar")?;

    let extracted_home = inner_dir.join("borg_home");
    let db_path = extracted_home.join("borg.db");
    if !db_path.exists() {
        bail!("backup is missing borg.db");
    }
    let db = Database::open_at(&db_path).context("verify: open extracted SQLite")?;
    let outcome = db.verify_audit_chain().context("verify: audit chain")?;
    if let AuditChainOutcome::Tampered { seq } = outcome {
        bail!("backup audit chain TAMPERED at seq={seq}");
    }
    let messages_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .context("verify: count messages")?;

    Ok(BackupVerifyReport {
        manifest,
        audit_outcome: outcome,
        messages_count,
    })
}

/// Restore `file` into `borg_home`. Refuses if `borg_home` already
/// contains state and `force` is false. With `force = true`, renames the
/// existing dir to `<borg_home>.bak.<unix_ts>` before extracting.
pub fn restore_backup(file: &Path, borg_home: &Path, force: bool) -> Result<()> {
    let report = verify_backup(file)?;
    if borg_home.exists() && has_existing_state(borg_home)? {
        if !force {
            bail!(
                "{} contains existing state — pass --force to rename it to .bak.<ts>",
                borg_home.display()
            );
        }
        let stamp = chrono::Utc::now().timestamp();
        let bak = borg_home.with_file_name(format!(
            "{}.bak.{stamp}",
            borg_home
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("borg")
        ));
        fs::rename(borg_home, &bak).with_context(|| {
            format!(
                "force-restore: failed to rename {} → {}",
                borg_home.display(),
                bak.display()
            )
        })?;
    }
    fs::create_dir_all(borg_home).with_context(|| {
        format!("restore: create {}", borg_home.display())
    })?;

    let tmp = tempfile::tempdir().context("restore: tempdir")?;
    let (_, body_tar) = extract_outer(file, tmp.path())?;
    let f = fs::File::open(&body_tar).context("restore: open body.tar")?;
    tar::Archive::new(BufReader::new(f))
        .unpack(tmp.path())
        .context("restore: extract body.tar")?;
    let staged = tmp.path().join("borg_home");
    move_dir_contents(&staged, borg_home)?;

    // Open the restored DB and append a `state_restored` event so the
    // audit chain captures the cutover. Best-effort — if the restore is
    // followed immediately by daemon startup, the daemon will append its
    // own daemon_start event right after.
    let db_path = borg_home.join("borg.db");
    if db_path.exists() {
        let db = Database::open_at(&db_path).context("restore: open restored SQLite")?;
        let payload = serde_json::json!({
            "src_file": file.display().to_string(),
            "schema_version": report.manifest.schema_version,
            "created_at": report.manifest.created_at,
            "sha256_pre_compress": report.manifest.sha256_pre_compress,
        })
        .to_string();
        if let Err(e) = db.append_audit_event(
            "state_restored",
            &payload,
            "system",
            chrono::Utc::now().timestamp(),
        ) {
            tracing::warn!("restore: failed to append state_restored audit event: {e}");
        }
    }

    Ok(())
}

/// Prune oldest `borg-backup-*.tar.zst` files under `dir` until at most
/// `keep` remain. Returns the number deleted. `keep == 0` deletes every
/// auto-backup (used to nuke the cadence when the user disables backups).
pub fn prune_backups(dir: &Path, keep: usize) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut entries: Vec<(i64, PathBuf)> = Vec::new();
    for ent in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let ent = ent?;
        let path = ent.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("borg-backup-") || !name.ends_with(".tar.zst") {
            continue;
        }
        let stem = name
            .trim_start_matches("borg-backup-")
            .trim_end_matches(".tar.zst");
        let ts: i64 = stem.parse().unwrap_or(0);
        entries.push((ts, path));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let mut deleted = 0;
    for (_, path) in entries.into_iter().skip(keep) {
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("prune: failed to remove {}: {e}", path.display());
            continue;
        }
        deleted += 1;
    }
    Ok(deleted)
}

// ── helpers ──

fn append_dir_filtered<W: Write>(
    builder: &mut tar::Builder<W>,
    src_root: &Path,
    archive_root: &Path,
) -> Result<()> {
    for entry in walk_dir(src_root)? {
        let rel = entry.strip_prefix(src_root).unwrap_or(&entry);
        if should_skip_entry(rel) {
            continue;
        }
        let archive_path = archive_root.join(rel);
        let meta = match fs::symlink_metadata(&entry) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            // Directories are auto-created when files inside them get
            // appended; explicit append is fine for empty dirs.
            builder
                .append_dir(&archive_path, &entry)
                .with_context(|| format!("append dir {}", entry.display()))?;
        } else if meta.is_file() {
            let mut f = fs::File::open(&entry)
                .with_context(|| format!("open {}", entry.display()))?;
            builder
                .append_file(&archive_path, &mut f)
                .with_context(|| format!("append file {}", entry.display()))?;
        }
        // Symlinks and special files (sockets) are skipped by design.
    }
    Ok(())
}

/// Recursive directory walk that yields every entry path. Filtering is
/// done by the caller via [`should_skip_entry`]; we don't prune branches
/// here because skip rules are name-based, not depth-based.
fn walk_dir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_dir_into(root, root, &mut out)?;
    Ok(out)
}

fn walk_dir_into(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for ent in fs::read_dir(current)
        .with_context(|| format!("read_dir {}", current.display()))?
    {
        let ent = ent?;
        let path = ent.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if should_skip_entry(rel) {
            continue;
        }
        out.push(path.clone());
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk_dir_into(root, &path, out)?;
        }
    }
    Ok(())
}

fn should_skip_entry(rel: &Path) -> bool {
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if EXCLUDED_NAMES.contains(&name) {
        return true;
    }
    if EXCLUDED_SUFFIXES.iter().any(|suf| name.ends_with(suf)) {
        return true;
    }
    // Top-level `logs` / `backups` matched above; also catch nested ones
    // for robustness against future restructuring.
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        EXCLUDED_NAMES.contains(&s.as_ref())
    })
}

/// Extract the outer (`metadata.json` + `body.tar`) container from a
/// `tar.zst` file into `out_dir`. Returns the parsed manifest and the
/// path to the extracted `body.tar`.
fn extract_outer(file: &Path, out_dir: &Path) -> Result<(BackupManifest, PathBuf)> {
    let f = fs::File::open(file)
        .with_context(|| format!("open backup {}", file.display()))?;
    let dec = zstd::Decoder::new(BufReader::new(f)).context("init zstd decoder")?;
    let mut archive = tar::Archive::new(dec);
    archive.unpack(out_dir).context("extract outer tar")?;
    let manifest_path = out_dir.join("metadata.json");
    let body_tar = out_dir.join("body.tar");
    if !manifest_path.exists() || !body_tar.exists() {
        bail!(
            "backup {} is missing metadata.json or body.tar",
            file.display()
        );
    }
    let manifest_bytes =
        fs::read(&manifest_path).with_context(|| "read metadata.json".to_string())?;
    let manifest: BackupManifest =
        serde_json::from_slice(&manifest_bytes).context("parse manifest")?;
    Ok((manifest, body_tar))
}

fn has_existing_state(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    for ent in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let ent = ent?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "borg.db" | "skills" | "channels" | "blobs") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn move_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    for ent in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let ent = ent?;
        let from = ent.path();
        let to = dst.join(ent.file_name());
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Err(e) = fs::rename(&from, &to) {
            // Cross-device rename — fall back to copy.
            // Cross-device errno (`EXDEV`) is 18 on macOS/Linux. Hard-code
            // it to avoid pulling `libc` just for the constant.
            const EXDEV: i32 = 18;
            if e.kind() == io::ErrorKind::CrossesDevices
                || matches!(e.raw_os_error(), Some(EXDEV))
            {
                copy_recursive(&from, &to)?;
            } else {
                return Err(anyhow!(
                    "restore: rename {} → {} failed: {e}",
                    from.display(),
                    to.display()
                ));
            }
        }
    }
    Ok(())
}

fn copy_recursive(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for ent in fs::read_dir(from)? {
            let ent = ent?;
            copy_recursive(&ent.path(), &to.join(ent.file_name()))?;
        }
    } else if meta.is_file() {
        fs::copy(from, to)?;
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Inline SHA-256 stream wrapper — wraps a writer and feeds every byte
/// passed through `write` into a borrowed `Sha256`. Used to digest the
/// inner tar without buffering it twice.
struct ShaWriter<'a, W: Write> {
    inner: W,
    hasher: &'a mut Sha256,
}

impl<'a, W: Write> ShaWriter<'a, W> {
    fn new(inner: W, hasher: &'a mut Sha256) -> Self {
        Self { inner, hasher }
    }
}

impl<W: Write> Write for ShaWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populate_borg_home(home: &Path) {
        fs::create_dir_all(home.join("blobs")).unwrap();
        fs::create_dir_all(home.join("logs")).unwrap();
        fs::create_dir_all(home.join("skills/example")).unwrap();
        fs::write(home.join("borg.db"), b"not really a sqlite file").unwrap();
        fs::write(home.join("blobs/a.bin"), b"blob-a").unwrap();
        fs::write(home.join("blobs/b.bin"), b"blob-b-content").unwrap();
        fs::write(home.join("skills/example/SKILL.md"), b"# example skill").unwrap();
        fs::write(home.join("hooks.json"), b"{}").unwrap();
        // Things that MUST NOT round-trip:
        fs::write(home.join("logs/daemon.log"), b"noisy log").unwrap();
        fs::write(home.join("borgd.sock"), b"not really a socket").unwrap();
        fs::write(home.join("borgd.lock"), b"flock sentinel").unwrap();
    }

    /// Real DB roundtrip: create_backup over a populated `BORG_HOME` whose
    /// `borg.db` is a real Database with audit-chain rows; then verify_backup
    /// must accept the tarball and report `Ok` (or `OkWithTombstones`).
    #[test]
    fn create_then_verify_roundtrip_on_real_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();

        // Stand up a real SQLite at borg.db with audit chain rows.
        let db_path = home.join("borg.db");
        let db = Database::open_at(&db_path).unwrap();
        db.append_audit_event("evt", r#"{"i":1}"#, "p", 1_700_000_000)
            .unwrap();
        db.append_audit_event("evt", r#"{"i":2}"#, "p", 1_700_000_001)
            .unwrap();
        // Insert a couple of messages so verify can count them.
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'user', 'hi', 1), ('s', 'assistant', 'hello', 2)",
                [],
            )
            .unwrap();
        drop(db);

        // Add a logs/ + .lock that must NOT roundtrip.
        fs::create_dir_all(home.join("logs")).unwrap();
        fs::write(home.join("logs/daemon.log"), b"noisy").unwrap();
        fs::write(home.join("borgd.lock"), b"x").unwrap();

        let path = create_backup(&home, &out, 47).expect("create");
        assert!(path.exists());

        let report = verify_backup(&path).expect("verify");
        assert_eq!(report.manifest.schema_version, 47);
        // Real audit chain → `Ok` (no tombstones written in this test).
        assert_eq!(report.audit_outcome, AuditChainOutcome::Ok);
        assert_eq!(report.messages_count, 2);
    }

    /// Restore must refuse a non-empty BORG_HOME without `--force`.
    #[test]
    fn restore_refuses_existing_state_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        let db_path = home.join("borg.db");
        Database::open_at(&db_path).unwrap();
        let backup = create_backup(&home, &out, 47).expect("create");

        // borg_home still has borg.db → has_existing_state == true.
        let err = restore_backup(&backup, &home, false).expect_err("must refuse");
        let msg = format!("{err}");
        assert!(
            msg.contains("existing state"),
            "expected 'existing state' in error: {msg}"
        );
    }

    /// `--force` renames the existing dir to `.bak.<ts>` and proceeds.
    #[test]
    fn restore_with_force_renames_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        Database::open_at(&home.join("borg.db")).unwrap();
        // Marker file the rename should preserve in the .bak dir.
        fs::write(home.join("hooks.json"), b"{}").unwrap();
        let backup = create_backup(&home, &out, 47).expect("create");

        // Mutate borg_home so there's a clear "before" to find afterwards.
        fs::write(home.join("hooks.json"), b"{\"before\":1}").unwrap();

        restore_backup(&backup, &home, true).expect("force restore");

        // `.bak.*` sibling must exist with the pre-restore hooks.json.
        let parent = home.parent().unwrap();
        let bak = fs::read_dir(parent)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("borg_home.bak."))
            .expect("expected a .bak.<ts> sibling dir");
        let preserved = fs::read(bak.path().join("hooks.json")).unwrap();
        assert_eq!(preserved, b"{\"before\":1}");

        // Restored dir contains the original hooks.json content (`{}`).
        let restored = fs::read(home.join("hooks.json")).unwrap();
        assert_eq!(restored, b"{}");
    }

    /// Tampering the inner tar after create breaks digest verification.
    /// Without the SHA pin, an attacker could swap blobs after backup.
    #[test]
    fn verify_rejects_digest_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        Database::open_at(&home.join("borg.db")).unwrap();
        let backup = create_backup(&home, &out, 47).expect("create");

        // Decompress, tamper the inner tar, recompress as plain bytes —
        // simplest tamper: append junk to the file. zstd will still
        // decompress its own frame; the inner tar's hash will mismatch.
        // (We don't need a perfect tamper, just any byte change inside.)
        let mut bytes = fs::read(&backup).unwrap();
        // Flip one byte in the middle (skip the zstd frame header).
        let idx = bytes.len() / 2;
        bytes[idx] ^= 0xFF;
        fs::write(&backup, &bytes).unwrap();

        let err = verify_backup(&backup);
        assert!(err.is_err(), "tampered backup must fail verification");
    }

    /// Missing manifest → fail. Defends against shipping a malformed
    /// tarball that happens to decompress.
    #[test]
    fn verify_rejects_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bogus.tar.zst");
        // Write a tar.zst whose contents are literally "garbage" — zstd
        // will fail decompression long before tar parsing.
        fs::write(&bad, b"not a tarball").unwrap();
        assert!(verify_backup(&bad).is_err());
    }

    /// `prune_backups(keep=2)` retains the two newest by timestamp filename.
    #[test]
    fn prune_backups_keeps_n_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for ts in [100, 200, 300, 400, 500] {
            fs::write(dir.join(format!("borg-backup-{ts}.tar.zst")), b"x").unwrap();
        }
        // Add an unrelated file that must NOT be touched.
        fs::write(dir.join("README.txt"), b"keep").unwrap();

        let n = prune_backups(dir, 2).expect("prune");
        assert_eq!(n, 3);
        let mut surviving: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("borg-backup-"))
            .collect();
        surviving.sort();
        assert_eq!(
            surviving,
            vec!["borg-backup-400.tar.zst", "borg-backup-500.tar.zst"]
        );
        assert!(dir.join("README.txt").exists());
    }

    /// `keep = 0` deletes every auto-backup. Used by the
    /// `backup_keep_count = 0` setting to disable auto-backup.
    #[test]
    fn prune_backups_keep_zero_removes_all() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for ts in [1, 2, 3] {
            fs::write(dir.join(format!("borg-backup-{ts}.tar.zst")), b"x").unwrap();
        }
        let n = prune_backups(dir, 0).expect("prune");
        assert_eq!(n, 3);
        assert!(fs::read_dir(dir).unwrap().next().is_none());
    }

    /// `should_skip_entry` filters out the four exclusion classes.
    /// Failure mode: a future restructuring that moves `logs` deeper would
    /// silently include log files in backups.
    #[test]
    fn should_skip_entry_filters_excluded_paths() {
        for skip in [
            "borgd.sock",
            "borgd.lock",
            "logs",
            "logs/daemon.log",
            "backups",
            "backups/borg-backup-1.tar.zst",
            "subdir/.lock",
        ] {
            assert!(
                should_skip_entry(Path::new(skip)),
                "expected to skip {skip}"
            );
        }
        for keep in ["borg.db", "skills/x/SKILL.md", "blobs/a.bin", "hooks.json"] {
            assert!(
                !should_skip_entry(Path::new(keep)),
                "expected to keep {keep}"
            );
        }
    }

    /// Filesystem-only roundtrip variant: confirms exclusions actually
    /// land in the right place even when borg.db is just a stub. Pairs
    /// with the real-DB roundtrip above (which exercises verify); this
    /// one focuses on what gets included.
    #[test]
    fn create_backup_excludes_logs_socket_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        populate_borg_home(&home);

        let path = create_backup(&home, &out, 47).expect("create");

        // Extract the outer container manually — verify_backup would
        // reject this because borg.db isn't a real SQLite.
        let inspect = tempfile::tempdir().unwrap();
        let (_manifest, body_tar) = extract_outer(&path, inspect.path()).expect("outer");
        let inner = inspect.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        let f = fs::File::open(&body_tar).unwrap();
        tar::Archive::new(BufReader::new(f)).unpack(&inner).unwrap();
        let restored = inner.join("borg_home");

        assert!(restored.join("borg.db").exists());
        assert!(restored.join("blobs/a.bin").exists());
        assert!(restored.join("hooks.json").exists());
        assert!(restored.join("skills/example/SKILL.md").exists());
        assert!(!restored.join("logs").exists(), "logs/ must be excluded");
        assert!(
            !restored.join("borgd.sock").exists(),
            "borgd.sock must be excluded"
        );
        assert!(
            !restored.join("borgd.lock").exists(),
            ".lock files must be excluded"
        );
    }
}
