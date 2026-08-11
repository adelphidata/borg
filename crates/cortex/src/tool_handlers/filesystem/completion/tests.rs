use std::fs;
use std::path::Path;

use super::*;
use crate::config::SecurityConfig;

/// The shipped blocklist, so these tests fail if a default ever stops covering
/// what a completion menu must never name.
fn security() -> SecurityConfig {
    SecurityConfig::default()
}

fn complete(dir: &Path, prefix: &str) -> Vec<PathSuggestion> {
    let ctx = ExecutionContext::for_workspace(dir.to_path_buf(), dir.to_path_buf()).unwrap();
    let s = security();
    complete_paths(prefix, &ctx, &s.blocked_paths, &s.allowed_paths, 20).unwrap()
}

fn paths(rows: &[PathSuggestion]) -> Vec<&str> {
    rows.iter().map(|r| r.path.as_str()).collect()
}

#[test]
fn offers_the_names_a_prefix_starts() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "").unwrap();
    fs::write(dir.path().join("mailer.rs"), "").unwrap();
    fs::write(dir.path().join("other.rs"), "").unwrap();

    assert_eq!(paths(&complete(dir.path(), "ma")), ["mailer.rs", "main.rs"]);
}

#[test]
fn marks_a_directory_and_offers_it_first() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("safety.md"), "").unwrap();

    let rows = complete(dir.path(), "s");
    // The trailing slash is what lets a client keep the menu open and go
    // deeper instead of ending the token on a directory.
    assert_eq!(paths(&rows), ["src/", "safety.md"]);
    assert!(rows[0].is_dir);
    assert!(!rows[1].is_dir);
}

#[test]
fn descends_into_a_named_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "").unwrap();

    // The path comes back spelled the way it was typed, so it can be written
    // straight back into the box.
    assert_eq!(paths(&complete(dir.path(), "src/ma")), ["src/main.rs"]);
    assert_eq!(paths(&complete(dir.path(), "src/")), ["src/main.rs"]);
}

#[test]
fn cannot_be_walked_out_of_the_workspace() {
    let outer = tempfile::tempdir().unwrap();
    fs::write(outer.path().join("secrets.txt"), "").unwrap();
    let inner = outer.path().join("workspace");
    fs::create_dir(&inner).unwrap();
    fs::write(inner.join("secrets.txt"), "").unwrap();

    assert!(complete(&inner, "../").is_empty());
    assert!(complete(&inner, "../sec").is_empty());
    assert!(complete(&inner, "../../").is_empty());
    // The same name inside the workspace is still fine — the refusal is about
    // where it is, not what it is called.
    assert_eq!(paths(&complete(&inner, "sec")), ["secrets.txt"]);
}

#[test]
fn refuses_an_absolute_or_home_relative_prefix() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "").unwrap();

    assert!(complete(dir.path(), "/etc/").is_empty());
    assert!(complete(dir.path(), "/").is_empty());
    assert!(complete(dir.path(), "~/.ssh/").is_empty());
}

#[test]
fn never_names_a_credential_the_read_tools_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    for name in [".env", ".env.local", "credentials.json", "id_rsa", "private_key.pem"] {
        fs::write(dir.path().join(name), "").unwrap();
    }
    fs::write(dir.path().join(".editorconfig"), "").unwrap();

    // Typing the dot is what asks for hidden files, so this is the query that
    // would expose `.env.local` if the dotted-family rule were not applied.
    let dotted = complete(dir.path(), ".");
    assert_eq!(paths(&dotted), [".editorconfig"]);

    assert!(complete(dir.path(), "cred").is_empty());
    assert!(complete(dir.path(), "id_").is_empty());
    assert!(complete(dir.path(), "private").is_empty());
}

#[test]
fn keeps_a_blocked_directory_out_along_with_what_is_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".ssh")).unwrap();
    fs::write(dir.path().join(".ssh/config"), "").unwrap();

    assert!(complete(dir.path(), ".ss").is_empty());
    assert!(complete(dir.path(), ".ssh/").is_empty());
}

#[test]
fn hides_dotfiles_until_the_dot_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("README.md"), "").unwrap();

    assert_eq!(paths(&complete(dir.path(), "")), ["README.md"]);
    assert_eq!(paths(&complete(dir.path(), ".")), [".git/"]);
}

#[cfg(unix)]
#[test]
fn refuses_a_symlink_that_points_out_of_the_workspace() {
    let outer = tempfile::tempdir().unwrap();
    fs::write(outer.path().join("secrets.txt"), "").unwrap();
    let inner = outer.path().join("workspace");
    fs::create_dir(&inner).unwrap();
    // A name inside the workspace that resolves outside it: the one way
    // containment fails if the check runs on the joined path rather than the
    // resolved one.
    std::os::unix::fs::symlink(outer.path().join("secrets.txt"), inner.join("shortcut.txt"))
        .unwrap();
    fs::write(inner.join("shortlist.txt"), "").unwrap();

    assert_eq!(paths(&complete(&inner, "short")), ["shortlist.txt"]);
}

#[test]
fn contains_the_workspace_even_when_the_session_does_not_enforce_a_root() {
    let outer = tempfile::tempdir().unwrap();
    fs::write(outer.path().join("secrets.txt"), "").unwrap();
    let inner = outer.path().join("workspace");
    fs::create_dir(&inner).unwrap();

    // `unrestricted` is what a session opened with no workspace context gets.
    // Completion must not inherit that permission — the menu is a disclosure
    // surface whatever the session's own enforcement flag says.
    let ctx = ExecutionContext::unrestricted(inner.clone());
    let s = security();
    let rows = complete_paths("../sec", &ctx, &s.blocked_paths, &s.allowed_paths, 20).unwrap();
    assert!(rows.is_empty());
}

#[test]
fn honours_the_cap_the_caller_asks_for() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..30 {
        fs::write(dir.path().join(format!("file-{i:02}.txt")), "").unwrap();
    }

    let ctx = ExecutionContext::for_workspace(dir.path().to_path_buf(), dir.path().to_path_buf())
        .unwrap();
    let s = security();
    let rows = complete_paths("file", &ctx, &s.blocked_paths, &s.allowed_paths, 5).unwrap();
    assert_eq!(paths(&rows), [
        "file-00.txt",
        "file-01.txt",
        "file-02.txt",
        "file-03.txt",
        "file-04.txt"
    ]);
}

#[test]
fn never_returns_more_than_the_ceiling_however_large_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..(MAX_PATH_SUGGESTIONS + 10) {
        fs::write(dir.path().join(format!("file-{i:03}.txt")), "").unwrap();
    }

    let ctx = ExecutionContext::for_workspace(dir.path().to_path_buf(), dir.path().to_path_buf())
        .unwrap();
    let s = security();
    let rows = complete_paths("file", &ctx, &s.blocked_paths, &s.allowed_paths, 10_000).unwrap();
    assert_eq!(rows.len(), MAX_PATH_SUGGESTIONS);
}

#[test]
fn answers_nothing_for_a_directory_that_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    assert!(complete(dir.path(), "nope/th").is_empty());
}
