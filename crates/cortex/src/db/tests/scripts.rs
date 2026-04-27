use super::*;

// ── Scripts CRUD tests ──

#[test]
fn scripts_crud() {
    let db = test_db();
    let s = NewScript {
        id: "s1",
        name: "test-script",
        description: "A test script",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "abc123",
        created_at: 1000,
        updated_at: 1000,
    };
    db.create_script(&s).unwrap();

    // get by name
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.name, "test-script");
    assert_eq!(row.description, "A test script");
    assert_eq!(row.runtime, "python");
    assert_eq!(row.hmac, "abc123");
    assert_eq!(row.run_count, 0);
    assert!(!row.network_access);
    assert!(!row.ephemeral);

    // list
    let scripts = db.list_scripts().unwrap();
    assert_eq!(scripts.len(), 1);

    // update hmac
    db.update_script_hmac("s1", "def456", 2000).unwrap();
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.hmac, "def456");
    assert_eq!(row.updated_at, 2000);

    // record run
    db.record_script_run("s1").unwrap();
    let row = db.get_script_by_name("test-script").unwrap().unwrap();
    assert_eq!(row.run_count, 1);
    assert!(row.last_run_at.is_some());

    // delete
    db.delete_script("s1").unwrap();
    assert!(db.get_script_by_name("test-script").unwrap().is_none());
}

#[test]
fn scripts_name_uniqueness() {
    let db = test_db();
    let s = NewScript {
        id: "s1",
        name: "dup",
        description: "",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h1",
        created_at: 1000,
        updated_at: 1000,
    };
    db.create_script(&s).unwrap();

    let s2 = NewScript {
        id: "s2",
        name: "dup",
        description: "",
        runtime: "python",
        entrypoint: "main.py",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h2",
        created_at: 1000,
        updated_at: 1000,
    };
    assert!(db.create_script(&s2).is_err());
}

#[test]
fn delete_ephemeral_scripts() {
    let db = test_db();
    // Old ephemeral
    db.create_script(&NewScript {
        id: "e1",
        name: "old-ephemeral",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: true,
        hmac: "h",
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();
    // Recent ephemeral
    db.create_script(&NewScript {
        id: "e2",
        name: "new-ephemeral",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: true,
        hmac: "h",
        created_at: 9999,
        updated_at: 9999,
    })
    .unwrap();
    // Non-ephemeral
    db.create_script(&NewScript {
        id: "p1",
        name: "persistent",
        description: "",
        runtime: "bash",
        entrypoint: "run.sh",
        sandbox_profile: "default",
        network_access: false,
        fs_read: "[]",
        fs_write: "[]",
        ephemeral: false,
        hmac: "h",
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();

    let deleted = db.delete_ephemeral_scripts_older_than(5000).unwrap();
    assert_eq!(deleted, 1);
    let remaining = db.list_scripts().unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|s| s.name == "new-ephemeral"));
    assert!(remaining.iter().any(|s| s.name == "persistent"));
}

#[test]
fn migrate_v19_creates_scripts_table() {
    let db = test_db();
    let version: u32 = db
        .get_meta("schema_version")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(version, Database::CURRENT_VERSION);

    // Verify the scripts table exists
    let count: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='scripts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "scripts table should exist after migration");
}
