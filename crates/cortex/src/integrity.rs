use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::Database;

/// Result of verifying file integrity for a plugin.
#[derive(Debug, Clone)]
pub struct IntegrityResult {
    pub ok: bool,
    pub tampered: Vec<String>,
    pub missing: Vec<String>,
}

/// Verify the integrity of files for a given plugin by comparing
/// their SHA-256 hashes against the stored values in the database.
pub fn verify_integrity(
    db: &Database,
    plugin_id: &str,
    data_dir: &Path,
) -> Result<IntegrityResult> {
    let stored_hashes = db.get_file_hashes(plugin_id)?;

    if stored_hashes.is_empty() {
        return Ok(IntegrityResult {
            ok: true,
            tampered: Vec::new(),
            missing: Vec::new(),
        });
    }

    let mut tampered = Vec::new();
    let mut missing = Vec::new();

    for (relative_path, expected_hash) in &stored_hashes {
        let full_path = match resolve_full_path(data_dir, relative_path) {
            Some(p) => p,
            None => {
                tampered.push(relative_path.clone());
                continue;
            }
        };

        if !full_path.exists() {
            missing.push(relative_path.clone());
            continue;
        }

        match std::fs::read(&full_path) {
            Ok(content) => {
                let actual_hash = compute_sha256(&content);
                if actual_hash != *expected_hash {
                    tampered.push(relative_path.clone());
                }
            }
            Err(_) => {
                missing.push(relative_path.clone());
            }
        }
    }

    let ok = tampered.is_empty() && missing.is_empty();
    Ok(IntegrityResult {
        ok,
        tampered,
        missing,
    })
}

/// Compute the SHA-256 hex digest of a byte slice.
pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Resolve the full filesystem path for a relative template path.
/// Template paths like "telegram/channel.toml" are relative to either
/// channels/ or tools/ under data_dir. We check both locations.
/// Returns None if the path contains traversal sequences.
fn resolve_full_path(data_dir: &Path, relative_path: &str) -> Option<std::path::PathBuf> {
    if relative_path.contains("..") {
        return None;
    }
    let channels_path = data_dir.join("channels").join(relative_path);
    if channels_path.exists() {
        return Some(channels_path);
    }
    let tools_path = data_dir.join("tools").join(relative_path);
    if tools_path.exists() {
        return Some(tools_path);
    }
    // Default to channels path for missing-file detection
    Some(channels_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::test_db()
    }

    #[test]
    fn verify_integrity_all_files_match() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        // Create files
        let tools_dir = data_dir.join("tools").join("pkg");
        std::fs::create_dir_all(&tools_dir).expect("create dirs");
        std::fs::write(tools_dir.join("tool.toml"), "name = \"test\"").expect("write");
        std::fs::write(tools_dir.join("main.py"), "print('hello')").expect("write");

        // Store matching hashes
        let hash1 = compute_sha256(b"name = \"test\"");
        let hash2 = compute_sha256(b"print('hello')");
        db.insert_file_hash("test/pkg", "pkg/tool.toml", &hash1)
            .expect("insert hash");
        db.insert_file_hash("test/pkg", "pkg/main.py", &hash2)
            .expect("insert hash");

        let result = verify_integrity(&db, "test/pkg", data_dir).expect("verify");
        assert!(result.ok);
        assert!(result.tampered.is_empty());
        assert!(result.missing.is_empty());
    }

    #[test]
    fn verify_integrity_detects_tampered_file() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        let tools_dir = data_dir.join("tools").join("pkg");
        std::fs::create_dir_all(&tools_dir).expect("create dirs");
        std::fs::write(tools_dir.join("main.py"), "print('tampered')").expect("write");

        let original_hash = compute_sha256(b"print('original')");
        db.insert_file_hash("test/pkg", "pkg/main.py", &original_hash)
            .expect("insert hash");

        let result = verify_integrity(&db, "test/pkg", data_dir).expect("verify");
        assert!(!result.ok);
        assert_eq!(result.tampered, vec!["pkg/main.py"]);
        assert!(result.missing.is_empty());
    }

    #[test]
    fn verify_integrity_detects_missing_file() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        let hash = compute_sha256(b"content");
        db.insert_file_hash("test/pkg", "pkg/main.py", &hash)
            .expect("insert hash");

        let result = verify_integrity(&db, "test/pkg", data_dir).expect("verify");
        assert!(!result.ok);
        assert!(result.tampered.is_empty());
        assert_eq!(result.missing, vec!["pkg/main.py"]);
    }

    #[test]
    fn verify_integrity_mixed_pass_and_fail() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        let tools_dir = data_dir.join("tools").join("pkg");
        std::fs::create_dir_all(&tools_dir).expect("create dirs");

        // File 1: matches
        std::fs::write(tools_dir.join("good.py"), "good").expect("write");
        let good_hash = compute_sha256(b"good");
        db.insert_file_hash("test/pkg", "pkg/good.py", &good_hash)
            .expect("insert hash");

        // File 2: tampered
        std::fs::write(tools_dir.join("bad.py"), "modified").expect("write");
        let original_hash = compute_sha256(b"original");
        db.insert_file_hash("test/pkg", "pkg/bad.py", &original_hash)
            .expect("insert hash");

        // File 3: missing (not on disk)
        let missing_hash = compute_sha256(b"missing");
        db.insert_file_hash("test/pkg", "pkg/gone.py", &missing_hash)
            .expect("insert hash");

        let result = verify_integrity(&db, "test/pkg", data_dir).expect("verify");
        assert!(!result.ok);
        assert_eq!(result.tampered, vec!["pkg/bad.py"]);
        assert_eq!(result.missing, vec!["pkg/gone.py"]);
    }

    #[test]
    fn verify_integrity_no_hashes_stored() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        let result = verify_integrity(&db, "test/pkg", tmp.path()).expect("verify");
        assert!(result.ok);
    }

    #[test]
    fn verify_integrity_empty_file() {
        let db = test_db();
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        db.insert_plugin("test/pkg", "Test", "tool", "test")
            .expect("insert cust");

        let tools_dir = data_dir.join("tools").join("pkg");
        std::fs::create_dir_all(&tools_dir).expect("create dirs");
        std::fs::write(tools_dir.join("empty.txt"), "").expect("write");

        let empty_hash = compute_sha256(b"");
        db.insert_file_hash("test/pkg", "pkg/empty.txt", &empty_hash)
            .expect("insert hash");

        let result = verify_integrity(&db, "test/pkg", data_dir).expect("verify");
        assert!(result.ok);
    }
}
