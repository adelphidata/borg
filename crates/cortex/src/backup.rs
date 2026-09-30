//! `BORG_HOME` backup unit.
//!
//! v1 ships a single tarball backup of the entire `BORG_HOME` directory
//! (SQLite + blobs + skills + channels + hooks + config). mTLS private keys
//! live in the OS keychain and are deliberately *not* in the tarball — a
//! restore on a fresh host triggers re-pairing.
//!
//! Excluded from the archive (see `should_skip_entry`):
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
//! Tarballs are unencrypted; users who need encryption-at-rest should layer
//! it externally (full-disk encryption, encrypted backup destination).

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{AuditChainOutcome, Database};
use crate::util::hex::hex_encode;

/// Read the live `BORG_HOME/borg.db`'s schema version through a read-only
/// connection. Used by `borg backup create` to tag the new tarball without
/// the CLI having to import `Database` directly.
pub async fn live_schema_version() -> Result<u32> {
    let db = Database::open_readonly()
        .await
        .context("open db read-only")?;
    db.schema_version()
        .await
        .context("read meta.schema_version")
}

/// File names / directories excluded from a backup tarball.
///
/// The SQLite database and its WAL/SHM siblings are excluded from the raw
/// directory walk: raw-copying a live database that borgd is writing can
/// produce a torn, unrestorable image (the tarball's SHA then certifies the
/// torn bytes as "verified"). The database enters the archive exclusively via
/// the `VACUUM INTO` snapshot taken in [`create_backup`].
const EXCLUDED_NAMES: &[&str] = &[
    "borgd.sock",
    "logs",
    "backups",
    // Derived scratch, and some of it is sensitive: `cache/borg_voice` holds
    // captured microphone audio and synthesized speech between capture and
    // cleanup, and `cache/shell-spill` holds command output. None of it is
    // state a restore needs, and a portable tarball is the wrong place for it.
    "cache",
    // `$EDITOR` scratch buffers (`paths::editor_dir`). Same reasoning as
    // `cache`: transient, never state a restore needs, and it holds a message
    // the user had not yet sent. It is also live while the user is editing, so
    // a buffer can vanish under the walk mid-archive.
    "editor",
    "borg.db",
    "borg.db-wal",
    "borg.db-shm",
];

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
pub async fn create_backup(
    borg_home: &Path,
    out_dir: &Path,
    schema_version: u32,
) -> Result<PathBuf> {
    if !borg_home.is_dir() {
        bail!("borg_home {} is not a directory", borg_home.display());
    }
    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create backup output dir {}", out_dir.display()))?;
    let now = chrono::Utc::now().timestamp();
    let dest = out_dir.join(format!("borg-backup-{now}.tar.zst"));

    // Snapshot the live SQLite database FIRST, via `VACUUM INTO` over a real
    // connection — SQLite's sanctioned online-copy path. The raw directory
    // walk below deliberately excludes `borg.db`/`-wal`/`-shm` (see
    // `EXCLUDED_NAMES`): byte-copying them while borgd has writes in flight
    // can tar a torn database. Fail closed — no raw-copy fallback: a missing
    // backup beats one that reports success and cannot be restored.
    let db_path = crate::paths::db_path(borg_home);
    let snap_dir = tempfile::tempdir().context("create tempdir for db snapshot")?;
    let db_snapshot = if db_path.exists() {
        Some(snapshot_live_db(&db_path, snap_dir.path()).await?)
    } else {
        // Fresh install with no database yet — nothing to snapshot.
        None
    };

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
        if let Some(snapshot) = &db_snapshot {
            // The snapshot rides under the same archive path the raw copy
            // used, so restore/verify are unchanged.
            builder
                .append_path_with_name(snapshot, Path::new("borg_home").join("borg.db"))
                .context("append db snapshot")?;
        }
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
    //
    // Streamed into a same-directory temp file and renamed into place only
    // after a successful fsync — a crash or ENOSPC mid-stream must not leave
    // a truncated archive under the final name, where it would read as a
    // valid backup until restore day. Same pattern as
    // `coding::fs_atomic::atomic_write`.
    let staging = tempfile::Builder::new()
        .prefix(".borg-backup-")
        .suffix(".tar.zst.tmp")
        .tempfile_in(out_dir)
        .with_context(|| format!("create backup staging file in {}", out_dir.display()))?;
    // Level 3 (zstd default): favors backup speed over ratio for local snapshots.
    let zstd_writer = zstd::Encoder::new(BufWriter::new(staging.as_file().try_clone()?), 3)
        .context("init zstd encoder")?;
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
    staging
        .as_file()
        .sync_all()
        .context("fsync backup staging file")?;
    // Backups must be readable by the operator's tooling; NamedTempFile
    // creates 0600, which is the right permission for a full borg_home dump.
    staging
        .persist(&dest)
        .map_err(|e| anyhow!("publish backup tarball at {}: {}", dest.display(), e.error))?;

    Ok(dest)
}

/// Verify a `tar.zst` produced by [`create_backup`]. Extracts to a temp
/// directory, confirms the SHA-256 of the inner tar matches the manifest,
/// opens the embedded SQLite, runs `verify_audit_chain`, and reports the
/// outcome. Returns `Err` on any check that disqualifies the tarball from
/// being restorable.
pub async fn verify_backup(file: &Path) -> Result<BackupVerifyReport> {
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
    let mut archive = tar::Archive::new(BufReader::new(f));
    harden_archive(&mut archive);
    archive
        .unpack(&inner_dir)
        .context("verify: extract inner tar")?;

    let extracted_home = inner_dir.join("borg_home");
    let db_path = crate::paths::db_path(&extracted_home);
    if !db_path.exists() {
        bail!("backup is missing borg.db");
    }
    let db = Database::open_at(&db_path)
        .await
        .context("verify: open extracted SQLite")?;
    let outcome = db
        .verify_audit_chain()
        .await
        .context("verify: audit chain")?;
    if let AuditChainOutcome::Tampered { seq } = outcome {
        bail!("backup audit chain TAMPERED at seq={seq}");
    }
    let messages_count: i64 = {
        let mut rows = db
            .conn()
            .query("SELECT COUNT(*) FROM messages", ())
            .await
            .context("verify: count messages")?;
        match rows.next().await.context("verify: count messages row")? {
            Some(row) => row.get::<i64>(0).context("verify: count column")?,
            None => 0,
        }
    };

    Ok(BackupVerifyReport {
        manifest,
        audit_outcome: outcome,
        messages_count,
    })
}

/// Restore `file` into `borg_home`. Refuses if `borg_home` already
/// contains state and `force` is false. With `force = true`, renames the
/// existing dir to `<borg_home>.bak.<unix_ts>` — but only after the whole
/// archive is extracted into a staging directory, so a failed extraction
/// leaves the live directory untouched.
pub async fn restore_backup(file: &Path, borg_home: &Path, force: bool) -> Result<()> {
    let report = verify_backup(file).await?;
    restore_staged(borg_home, force, |staging| {
        let (_, body_tar) = extract_outer(file, staging)?;
        let f = fs::File::open(&body_tar).context("restore: open body.tar")?;
        let mut archive = tar::Archive::new(BufReader::new(f));
        harden_archive(&mut archive);
        archive
            .unpack(staging)
            .context("restore: extract body.tar")?;
        Ok(staging.join("borg_home"))
    })?;

    // Open the restored DB and append a `state_restored` event so the
    // audit chain captures the cutover. Best-effort — if the restore is
    // followed immediately by daemon startup, the daemon will append its
    // own daemon_start event right after.
    let db_path = crate::paths::db_path(borg_home);
    if db_path.exists() {
        let db = Database::open_at(&db_path)
            .await
            .context("restore: open restored SQLite")?;
        let payload = serde_json::json!({
            "src_file": file.display().to_string(),
            "schema_version": report.manifest.schema_version,
            "created_at": report.manifest.created_at,
            "sha256_pre_compress": report.manifest.sha256_pre_compress,
        })
        .to_string();
        if let Err(e) = db
            .append_audit_event(
                "state_restored",
                &payload,
                "system",
                chrono::Utc::now().timestamp(),
            )
            .await
        {
            tracing::warn!("restore: failed to append state_restored audit event: {e}");
        }
    }

    Ok(())
}

// ── helpers ──

/// The filesystem half of a restore. `stage` fills the staging directory and
/// returns the path of the staged home inside it.
///
/// Order is the contract: nothing in `borg_home` changes until `stage` has
/// returned `Ok`. The staging directory is a sibling of `borg_home`, so the
/// final move is a rename on one filesystem.
fn restore_staged(
    borg_home: &Path,
    force: bool,
    stage: impl FnOnce(&Path) -> Result<PathBuf>,
) -> Result<()> {
    let has_state = borg_home.exists() && has_existing_state(borg_home)?;
    if has_state && !force {
        bail!(
            "{} contains existing state — pass --force to rename it to .bak.<ts>",
            borg_home.display()
        );
    }

    let parent = match borg_home.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).with_context(|| format!("restore: create {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".borg-restore-")
        .tempdir_in(parent)
        .with_context(|| format!("restore: create staging dir in {}", parent.display()))?;
    let staged_home = stage(staging.path()).with_context(|| {
        format!(
            "restore stopped before any change to {}",
            borg_home.display()
        )
    })?;

    let bak = if has_state {
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
        Some(bak)
    } else {
        None
    };

    let Err(install_err) = install_staged_home(&staged_home, borg_home) else {
        return Ok(());
    };
    let Some(bak) = bak else {
        return Err(install_err);
    };
    match put_back_original(&bak, borg_home) {
        Ok(()) => Err(install_err.context(format!(
            "restore failed; the original state was put back at {}",
            borg_home.display()
        ))),
        Err(rollback_err) => Err(install_err.context(format!(
            "restore failed and the original state could not be put back \
             ({rollback_err:#}); it is kept at {}",
            bak.display()
        ))),
    }
}

/// Move the staged home into place. A plain rename when `borg_home` is
/// absent; else (or when the rename fails) a per-entry move into it.
fn install_staged_home(staged_home: &Path, borg_home: &Path) -> Result<()> {
    if !staged_home.is_dir() {
        bail!(
            "restore: the archive has no borg_home directory ({})",
            staged_home.display()
        );
    }
    if !borg_home.exists() {
        match fs::rename(staged_home, borg_home) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::debug!(
                "restore: rename of the staged home failed ({e}); moving entries one by one"
            ),
        }
    }
    fs::create_dir_all(borg_home)
        .with_context(|| format!("restore: create {}", borg_home.display()))?;
    move_dir_contents(staged_home, borg_home)
}

/// Undo a failed install: remove the partly filled `borg_home` and rename the
/// `.bak` directory back.
fn put_back_original(bak: &Path, borg_home: &Path) -> Result<()> {
    if borg_home.exists() {
        fs::remove_dir_all(borg_home)
            .with_context(|| format!("remove partial {}", borg_home.display()))?;
    }
    fs::rename(bak, borg_home)
        .with_context(|| format!("rename {} → {}", bak.display(), borg_home.display()))
}

/// Produce a consistent point-in-time copy of the SQLite database at
/// `db_path` into `work_dir`, using `VACUUM INTO` over a live connection.
/// Safe against concurrent writers (the statement runs inside its own read
/// transaction) and never touches the source file with raw I/O.
///
/// Opens a bare libsql connection rather than `Database::open_at` on
/// purpose: the recovering opener quarantine-renames a file it cannot read,
/// which a backup must never do to the live database. An unreadable source
/// simply fails the backup here. Unlike the in-place compaction in
/// `db::wal` (see its note on why `VACUUM INTO` + rename is unsafe there),
/// the live file is never replaced — the copy is only read into the tarball.
async fn snapshot_live_db(db_path: &Path, work_dir: &Path) -> Result<PathBuf> {
    let dest = work_dir.join("borg.db");
    let db = libsql::Builder::new_local(db_path.to_string_lossy().as_ref())
        .build()
        .await
        .context("snapshot: open source database")?;
    let conn = db.connect().context("snapshot: connect")?;
    // Wait for a writer instead of failing the instant one holds the lock: a
    // running borgd writes often enough that an unbounded-zero timeout turns a
    // routine backup into a coin flip. Bounded, so a wedged writer surfaces as
    // a timeout rather than hanging the command.
    conn.busy_timeout(SNAPSHOT_BUSY_TIMEOUT)
        .context("snapshot: set busy_timeout")?;
    // Escape single quotes for the SQL string literal. The destination is
    // our own tempdir path, but tempdirs can live under arbitrary prefixes.
    let dest_sql = dest.to_string_lossy().replace('\'', "''");
    conn.execute(&format!("VACUUM INTO '{dest_sql}'"), ())
        .await
        .map_err(|e| snapshot_error(anyhow::Error::from(e)))?;
    Ok(dest)
}

/// How long the snapshot waits for a concurrent writer before giving up.
///
/// Longer than the 5s CLI default: `VACUUM INTO` needs a read lock for the
/// whole copy, and a busy daemon can hold the write lock across several turns'
/// worth of short transactions.
const SNAPSHOT_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Turn a snapshot failure into one the operator can act on.
///
/// A raw `database is locked` from libsql names no cause and no next step; the
/// cause is almost always the daemon, and stopping it is the fix.
fn snapshot_error(err: anyhow::Error) -> anyhow::Error {
    if crate::db::is_lock_contention(&err) {
        return err.context(
            "snapshot: the database stayed locked by another process for 30s — \
             stop the daemon with `borg service stop` (or wait for the running \
             turn to finish) and run the backup again",
        );
    }
    err.context("snapshot: VACUUM INTO")
}

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
            let mut f =
                fs::File::open(&entry).with_context(|| format!("open {}", entry.display()))?;
            builder
                .append_file(&archive_path, &mut f)
                .with_context(|| format!("append file {}", entry.display()))?;
        }
        // Symlinks and special files (sockets) are skipped by design.
    }
    Ok(())
}

/// Recursive directory walk that yields every entry path. Filtering is
/// done by the caller via `should_skip_entry`; we don't prune branches
/// here because skip rules are name-based, not depth-based.
fn walk_dir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_dir_into(root, root, &mut out)?;
    Ok(out)
}

fn walk_dir_into(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for ent in fs::read_dir(current).with_context(|| format!("read_dir {}", current.display()))? {
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

/// Apply hardened-extraction settings to a `tar::Archive` before
/// `unpack`. Defends against hostile tarballs that try to escape the
/// extraction root via:
/// - Symlink-then-file pairs writing outside the dir (`set_overwrite(false)`
///   refuses to clobber existing entries, and the `tar` crate's own
///   path-traversal protection rejects `..` / absolute paths).
/// - Foreign uid/gid / sticky-bit / setuid bits in entries
///   (`set_preserve_permissions(false)` — borg's own files are
///   re-permissioned by the daemon on next start).
/// - xattr metadata that might survive across hosts in surprising ways
///   (`set_unpack_xattrs(false)`).
fn harden_archive<R: std::io::Read>(archive: &mut tar::Archive<R>) {
    archive.set_overwrite(false);
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);
}

/// Extract the outer (`metadata.json` + `body.tar`) container from a
/// `tar.zst` file into `out_dir`. Returns the parsed manifest and the
/// path to the extracted `body.tar`.
fn extract_outer(file: &Path, out_dir: &Path) -> Result<(BackupManifest, PathBuf)> {
    let f = fs::File::open(file).with_context(|| format!("open backup {}", file.display()))?;
    let dec = zstd::Decoder::new(BufReader::new(f)).context("init zstd decoder")?;
    let mut archive = tar::Archive::new(dec);
    harden_archive(&mut archive);
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
            fs::create_dir_all(parent)
                .with_context(|| format!("restore: create {}", parent.display()))?;
        }
        if let Err(e) = fs::rename(&from, &to) {
            // Cross-device rename — fall back to copy.
            // Cross-device errno (`EXDEV`) is 18 on macOS/Linux. Hard-code
            // it to avoid pulling `libc` just for the constant.
            const EXDEV: i32 = 18;
            if e.kind() == io::ErrorKind::CrossesDevices || matches!(e.raw_os_error(), Some(EXDEV))
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
    } else if meta.file_type().is_symlink() {
        // Backups archive symlinks as symlink entries (`follow_symlinks(false)`),
        // so the staged tree can contain them. Recreate the link rather than
        // silently dropping it — a restore that omits entries while returning
        // Ok would falsely report success.
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, to).with_context(|| {
            format!(
                "restore: recreate symlink {} → {}",
                to.display(),
                target.display()
            )
        })?;
        #[cfg(not(unix))]
        return Err(anyhow!(
            "restore: cannot recreate symlink {} → {} on this platform",
            to.display(),
            target.display()
        ));
    } else {
        // Sockets/FIFOs/etc. — refuse rather than silently omit, so a
        // partial restore never masquerades as a complete one.
        return Err(anyhow!(
            "restore: unsupported file type at {} (not a file, directory, or symlink)",
            from.display()
        ));
    }
    Ok(())
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

    /// A backup that loses to the daemon must say so and name the fix. The raw
    /// libsql text is `database is locked`, which tells the operator neither
    /// what holds it nor what to do, so they retry into the same failure.
    #[test]
    fn a_locked_database_reports_the_daemon_and_the_fix() {
        let locked = anyhow::Error::from(libsql::Error::SqliteFailure(
            5, // SQLITE_BUSY
            "database is locked".to_string(),
        ));
        let reported = format!("{:#}", snapshot_error(locked));
        assert!(
            reported.contains("borg service stop"),
            "must name the next step: {reported}"
        );
        assert!(
            reported.contains("locked"),
            "must keep the cause: {reported}"
        );

        // Anything else keeps its own context rather than being blamed on a
        // lock — a disk-full backup told to stop the daemon wastes the fix.
        let full = anyhow::Error::from(libsql::Error::SqliteFailure(
            13, // SQLITE_FULL
            "database or disk is full".to_string(),
        ));
        let reported = format!("{:#}", snapshot_error(full));
        assert!(
            !reported.contains("borg service stop"),
            "a disk-full failure is not lock contention: {reported}"
        );
        assert!(reported.contains("VACUUM INTO"), "{reported}");
    }

    /// The cross-device restore fallback must reproduce the staged tree
    /// faithfully — a symlink silently dropped (the old behavior: neither the
    /// file nor the dir branch matched, so the entry was skipped with `Ok`)
    /// is a partial restore reported as success.
    #[cfg(unix)]
    #[test]
    fn copy_recursive_preserves_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("skills")).unwrap();
        fs::write(src.join("skills/real.md"), b"content").unwrap();
        std::os::unix::fs::symlink("skills/real.md", src.join("alias.md")).unwrap();

        copy_recursive(&src, &dst).expect("copy must succeed");

        let link = dst.join("alias.md");
        let meta = fs::symlink_metadata(&link).expect("symlink must exist in the restored tree");
        assert!(meta.file_type().is_symlink(), "entry must stay a symlink");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            std::path::PathBuf::from("skills/real.md"),
            "link target must round-trip"
        );
        assert_eq!(
            fs::read_to_string(dst.join("skills/real.md")).unwrap(),
            "content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_recursive_refuses_special_files_instead_of_omitting_them() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("keep.txt"), b"x").unwrap();
        // Create a FIFO — a file type the restore copy cannot reproduce.
        let status = std::process::Command::new("mkfifo")
            .arg(src.join("pipe"))
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");

        let err = copy_recursive(&src, &dst)
            .expect_err("a tree containing an uncopyable entry must fail loudly, not partially");
        assert!(
            err.to_string().contains("unsupported file type"),
            "error must name the problem, got: {err}"
        );
    }

    fn populate_borg_home(home: &Path) {
        fs::create_dir_all(home.join("blobs")).unwrap();
        fs::create_dir_all(home.join("logs")).unwrap();
        fs::create_dir_all(home.join("skills/example")).unwrap();
        fs::write(home.join("blobs/a.bin"), b"blob-a").unwrap();
        fs::write(home.join("blobs/b.bin"), b"blob-b-content").unwrap();
        fs::write(home.join("skills/example/SKILL.md"), b"# example skill").unwrap();
        fs::write(home.join("hooks.json"), b"{}").unwrap();
        // Things that MUST NOT round-trip:
        fs::write(home.join("logs/daemon.log"), b"noisy log").unwrap();
        fs::write(home.join("borgd.sock"), b"not really a socket").unwrap();
        fs::write(home.join("borgd.lock"), b"flock sentinel").unwrap();
    }

    #[tokio::test]
    async fn create_then_verify_roundtrip_on_real_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();

        let db_path = home.join("borg.db");
        let db = Database::open_at(&db_path).await.unwrap();
        db.append_audit_event("evt", r#"{"i":1}"#, "p", 1_700_000_000)
            .await
            .unwrap();
        db.append_audit_event("evt", r#"{"i":2}"#, "p", 1_700_000_001)
            .await
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'user', 'hi', 1)",
                (),
            )
            .await
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'assistant', 'hello', 2)",
                (),
            )
            .await
            .unwrap();
        drop(db);

        fs::create_dir_all(home.join("logs")).unwrap();
        fs::write(home.join("logs/daemon.log"), b"noisy").unwrap();
        fs::write(home.join("borgd.lock"), b"x").unwrap();

        let path = create_backup(&home, &out, 47).await.expect("create");
        assert!(path.exists());

        let report = verify_backup(&path).await.expect("verify");
        assert_eq!(report.manifest.schema_version, 47);
        assert_eq!(report.audit_outcome, AuditChainOutcome::Ok);
        assert_eq!(report.messages_count, 2);
    }

    #[tokio::test]
    async fn restore_refuses_existing_state_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        let db_path = home.join("borg.db");
        Database::open_at(&db_path).await.unwrap();
        let backup = create_backup(&home, &out, 47).await.expect("create");

        let err = restore_backup(&backup, &home, false)
            .await
            .expect_err("must refuse");
        let msg = format!("{err}");
        assert!(
            msg.contains("existing state"),
            "expected 'existing state' in error: {msg}"
        );
    }

    #[tokio::test]
    async fn restore_with_force_renames_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        Database::open_at(&home.join("borg.db")).await.unwrap();
        fs::write(home.join("hooks.json"), b"{}").unwrap();
        let backup = create_backup(&home, &out, 47).await.expect("create");

        fs::write(home.join("hooks.json"), b"{\"before\":1}").unwrap();

        restore_backup(&backup, &home, true)
            .await
            .expect("force restore");

        let parent = home.parent().unwrap();
        let bak = fs::read_dir(parent)
            .unwrap()
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("borg_home.bak.")
            })
            .expect("expected a .bak.<ts> sibling dir");
        let preserved = fs::read(bak.path().join("hooks.json")).unwrap();
        assert_eq!(preserved, b"{\"before\":1}");

        let restored = fs::read(home.join("hooks.json")).unwrap();
        assert_eq!(restored, b"{}");
    }

    /// Every file under `root` with its content, for before/after comparison.
    fn tree_contents(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out: Vec<(PathBuf, Vec<u8>)> = walk_dir(root)
            .unwrap()
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| {
                (
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn sibling_names(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(home.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn failed_extraction_leaves_the_live_home_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        populate_borg_home(&home);
        fs::write(home.join("borg.db"), b"live database bytes").unwrap();
        let before = tree_contents(&home);
        assert!(!before.is_empty());

        let err = restore_staged(&home, true, |staging| {
            // A half-written staging tree, then the failure.
            fs::create_dir_all(staging.join("borg_home")).unwrap();
            fs::write(staging.join("borg_home/borg.db"), b"partial").unwrap();
            Err(anyhow!("unexpected end of archive member"))
        })
        .expect_err("a failed extraction must fail the restore");

        let msg = format!("{err:#}");
        assert!(msg.contains("unexpected end of archive member"), "{msg}");
        assert!(msg.contains("before any change"), "{msg}");
        assert_eq!(tree_contents(&home), before, "live home must be unchanged");
        assert_eq!(
            sibling_names(&home),
            vec!["borg_home".to_string()],
            "no .bak and no staging directory may remain"
        );
    }

    #[test]
    fn failed_install_puts_the_original_home_back() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        populate_borg_home(&home);
        fs::write(home.join("borg.db"), b"live database bytes").unwrap();
        let before = tree_contents(&home);

        // Staging reports success but the staged home is not there, so the
        // failure occurs after the live home was moved aside.
        let err = restore_staged(&home, true, |staging| Ok(staging.join("borg_home")))
            .expect_err("a missing staged home must fail the restore");

        let msg = format!("{err:#}");
        assert!(msg.contains("original state was put back"), "{msg}");
        assert_eq!(tree_contents(&home), before);
        assert_eq!(sibling_names(&home), vec!["borg_home".to_string()]);
    }

    #[test]
    fn staged_restore_replaces_the_home_and_keeps_one_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        populate_borg_home(&home);
        fs::write(home.join("borg.db"), b"old").unwrap();
        let before = tree_contents(&home);

        restore_staged(&home, true, |staging| {
            let staged = staging.join("borg_home");
            fs::create_dir_all(staged.join("skills")).unwrap();
            fs::write(staged.join("borg.db"), b"new").unwrap();
            Ok(staged)
        })
        .unwrap();

        assert_eq!(fs::read(home.join("borg.db")).unwrap(), b"new");
        assert!(
            !home.join("hooks.json").exists(),
            "old entries must not leak into the restored home"
        );
        let names = sibling_names(&home);
        let baks: Vec<&String> = names
            .iter()
            .filter(|n| n.starts_with("borg_home.bak."))
            .collect();
        assert_eq!(baks.len(), 1, "siblings: {names:?}");
        assert_eq!(names.len(), 2, "staging dir must be gone: {names:?}");
        assert_eq!(tree_contents(&tmp.path().join(baks[0])), before);
    }

    #[test]
    fn restore_into_a_home_without_state_merges_and_makes_no_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        fs::create_dir_all(home.join("logs")).unwrap();
        fs::write(home.join("logs/daemon.log"), b"log").unwrap();

        restore_staged(&home, false, |staging| {
            let staged = staging.join("borg_home");
            fs::create_dir_all(&staged).unwrap();
            fs::write(staged.join("borg.db"), b"new").unwrap();
            Ok(staged)
        })
        .unwrap();

        assert_eq!(fs::read(home.join("borg.db")).unwrap(), b"new");
        assert_eq!(fs::read(home.join("logs/daemon.log")).unwrap(), b"log");
        assert_eq!(sibling_names(&home), vec!["borg_home".to_string()]);
    }

    #[tokio::test]
    async fn verify_rejects_digest_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        Database::open_at(&home.join("borg.db")).await.unwrap();
        let backup = create_backup(&home, &out, 47).await.expect("create");

        let mut bytes = fs::read(&backup).unwrap();
        let idx = bytes.len() / 2;
        bytes[idx] ^= 0xFF;
        fs::write(&backup, &bytes).unwrap();

        let err = verify_backup(&backup).await;
        assert!(err.is_err(), "tampered backup must fail verification");
    }

    #[tokio::test]
    async fn verify_rejects_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bogus.tar.zst");
        fs::write(&bad, b"not a tarball").unwrap();
        assert!(verify_backup(&bad).await.is_err());
    }

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
            // $EDITOR scratch: the user's unsent message, live while they type.
            "editor",
            "editor/borg-ab12.md",
            // The database and its WAL/SHM siblings never enter via the raw
            // walk — only via the VACUUM INTO snapshot.
            "borg.db",
            "borg.db-wal",
            "borg.db-shm",
        ] {
            assert!(
                should_skip_entry(Path::new(skip)),
                "expected to skip {skip}"
            );
        }
        for keep in ["skills/x/SKILL.md", "blobs/a.bin", "hooks.json"] {
            assert!(
                !should_skip_entry(Path::new(keep)),
                "expected to keep {keep}"
            );
        }
    }

    #[tokio::test]
    async fn create_backup_excludes_logs_socket_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        populate_borg_home(&home);
        Database::open_at(&home.join("borg.db")).await.unwrap();

        let path = create_backup(&home, &out, 47).await.expect("create");

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
        assert!(
            !restored.join("borg.db-wal").exists() && !restored.join("borg.db-shm").exists(),
            "WAL/SHM siblings must never be tarred — the db enters only via snapshot"
        );
    }

    /// The archived database must be a consistent snapshot even while a live
    /// connection holds unmerged WAL content. A raw byte-copy of `borg.db`
    /// alone (WAL excluded) would silently lose the un-checkpointed rows —
    /// this pins that the `VACUUM INTO` snapshot carries them.
    #[tokio::test]
    async fn snapshot_captures_unmerged_wal_content_from_live_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();

        let db = Database::open_at(&home.join("borg.db")).await.unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'user', 'in-wal-1', 1)",
                (),
            )
            .await
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'assistant', 'in-wal-2', 2)",
                (),
            )
            .await
            .unwrap();
        // Keep `db` alive: the connection stays open across the backup, so
        // the rows may still sit in the WAL rather than the main file.
        let path = create_backup(&home, &out, 47).await.expect("create");
        drop(db);

        let report = verify_backup(&path).await.expect("verify");
        assert_eq!(
            report.messages_count, 2,
            "snapshot must include rows still in the live connection's WAL"
        );

        // Belt-and-braces: the extracted database passes integrity_check.
        let inspect = tempfile::tempdir().unwrap();
        let (_m, body_tar) = extract_outer(&path, inspect.path()).expect("outer");
        let inner = inspect.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        let f = fs::File::open(&body_tar).unwrap();
        tar::Archive::new(BufReader::new(f)).unpack(&inner).unwrap();
        let extracted = Database::open_at(&inner.join("borg_home/borg.db"))
            .await
            .unwrap();
        let mut rows = extracted
            .conn()
            .query("PRAGMA integrity_check", ())
            .await
            .unwrap();
        let verdict = rows
            .next()
            .await
            .unwrap()
            .unwrap()
            .get::<String>(0)
            .unwrap();
        assert_eq!(verdict, "ok", "extracted db failed integrity_check");
    }

    /// Fail closed: an unreadable/garbage database must fail the backup —
    /// never fall back to a raw byte copy that would tar the garbage and
    /// report success.
    #[tokio::test]
    async fn create_backup_fails_closed_on_unreadable_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("borg.db"), b"not really a sqlite file").unwrap();

        let err = create_backup(&home, &out, 47)
            .await
            .expect_err("garbage db must abort the backup");
        assert!(
            format!("{err:#}").contains("snapshot"),
            "error should name the snapshot step: {err:#}"
        );
        // A failed backup must publish nothing: no final-named artifact and
        // no stray staging file left in the output dir.
        let leftovers: Vec<_> = fs::read_dir(&out)
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "failed backup left artifacts: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn successful_backup_publishes_atomically_with_no_staging_residue() {
        // The archive is streamed into a same-directory temp file and renamed
        // into place — a completed backup dir must contain exactly the final
        // artifact, never a `.tmp` staging file.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("borg_home");
        let out = tmp.path().join("backups");
        fs::create_dir_all(&home).unwrap();
        populate_borg_home(&home);
        Database::open_at(&home.join("borg.db")).await.unwrap();

        let published = create_backup(&home, &out, 47).await.expect("create");
        let entries: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        assert_eq!(
            entries,
            vec![published.clone()],
            "backup dir must contain exactly the published artifact"
        );
        assert!(
            published.extension().is_some_and(|e| e == "zst"),
            "published artifact keeps the .tar.zst name: {published:?}"
        );
    }
}
