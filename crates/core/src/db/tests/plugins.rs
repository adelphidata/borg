use super::*;

#[test]
fn insert_and_list_plugins() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert");
    let list = db.list_plugins().expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "email/gmail"); // ordered by category, name
    assert_eq!(list[1].id, "messaging/telegram");
}

#[test]
fn delete_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    assert!(db.delete_plugin("messaging/telegram").expect("delete"));
    assert!(!db.delete_plugin("nonexistent").expect("delete missing"));
    let list = db.list_plugins().expect("list");
    assert!(list.is_empty());
}

#[test]
fn set_plugin_verified() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    let list = db.list_plugins().expect("list");
    assert!(list[0].verified_at.is_none());

    db.set_plugin_verified("messaging/telegram")
        .expect("verify");
    let list = db.list_plugins().expect("list");
    assert!(list[0].verified_at.is_some());
}

#[test]
fn insert_and_delete_credentials() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_credential(
        "messaging/telegram",
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");
    let deleted = db
        .delete_credentials_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 1);
}

#[test]
fn credential_cascade_on_plugin_delete() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_credential(
        "messaging/telegram",
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");

    db.delete_plugin("messaging/telegram").expect("delete");
    // Credential should be cascade-deleted
    let deleted = db
        .delete_credentials_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 0);
}

#[test]
fn insert_plugin_replaces_existing() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert");
    db.insert_plugin("messaging/telegram", "Telegram v2", "channel", "messaging")
        .expect("replace");
    let list = db.list_plugins().expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "Telegram v2");
}

#[test]
fn migrate_v5_creates_file_hashes_table() {
    let db = test_db();
    let mut stmt = db
        .conn
        .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='file_hashes'")
        .expect("prepare");
    let count: i64 = stmt.query_row([], |row| row.get(0)).expect("query");
    assert_eq!(count, 1);
}

#[test]
fn insert_and_get_file_hashes() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc123")
        .expect("insert hash 1");
    db.insert_file_hash("messaging/telegram", "telegram/parse_inbound.py", "def456")
        .expect("insert hash 2");
    db.insert_file_hash("messaging/telegram", "telegram/send_outbound.py", "ghi789")
        .expect("insert hash 3");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert_eq!(hashes.len(), 3);
}

#[test]
fn file_hashes_cascade_delete() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc123")
        .expect("insert hash");
    db.delete_plugin("messaging/telegram").expect("delete cust");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert!(hashes.is_empty());
}

#[test]
fn delete_file_hashes_by_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust 1");
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust 2");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "abc")
        .expect("insert hash 1");
    db.insert_file_hash("email/gmail", "gmail/tool.toml", "def")
        .expect("insert hash 2");
    db.delete_file_hashes("messaging/telegram").expect("delete");
    let t_hashes = db.get_file_hashes("messaging/telegram").expect("get");
    let g_hashes = db.get_file_hashes("email/gmail").expect("get");
    assert!(t_hashes.is_empty());
    assert_eq!(g_hashes.len(), 1);
}

#[test]
fn insert_installed_tool_and_get_plugin_id() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust");
    db.insert_installed_tool("gmail", "Gmail integration", "python", "email/gmail")
        .expect("insert tool");
    let cust_id = db.get_tool_plugin_id("gmail").expect("get");
    assert_eq!(cust_id.as_deref(), Some("email/gmail"));
}

#[test]
fn get_tool_plugin_id_returns_none_for_unknown() {
    let db = test_db();
    let cust_id = db.get_tool_plugin_id("nonexistent").expect("get");
    assert!(cust_id.is_none());
}

#[test]
fn insert_installed_channel_and_get_plugin_id() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "python",
        "messaging/telegram",
        "/webhook/telegram",
    )
    .expect("insert channel");
    let cust_id = db.get_channel_plugin_id("telegram").expect("get");
    assert_eq!(cust_id.as_deref(), Some("messaging/telegram"));
}

#[test]
fn get_channel_plugin_id_returns_none_for_unknown() {
    let db = test_db();
    let cust_id = db.get_channel_plugin_id("nonexistent").expect("get");
    assert!(cust_id.is_none());
}

#[test]
fn file_hash_upsert_on_reinstall() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert cust");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "old_hash")
        .expect("insert hash");
    db.insert_file_hash("messaging/telegram", "telegram/channel.toml", "new_hash")
        .expect("upsert hash");
    let hashes = db.get_file_hashes("messaging/telegram").expect("get");
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].1, "new_hash");
}

#[test]
fn delete_installed_channels_for_plugin() {
    let db = test_db();
    db.insert_plugin("messaging/telegram", "Telegram", "channel", "messaging")
        .expect("insert plugin");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "native",
        "messaging/telegram",
        "/webhook/telegram",
    )
    .expect("insert channel");
    assert_eq!(
        db.get_channel_plugin_id("telegram").unwrap().as_deref(),
        Some("messaging/telegram")
    );
    let deleted = db
        .delete_installed_channels_for("messaging/telegram")
        .expect("delete");
    assert_eq!(deleted, 1);
    assert!(db.get_channel_plugin_id("telegram").unwrap().is_none());
}

#[test]
fn delete_installed_tools_for_plugin() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert plugin");
    db.insert_installed_tool("gmail", "Gmail integration", "python", "email/gmail")
        .expect("insert tool");
    assert!(db.get_tool_plugin_id("gmail").unwrap().is_some());
    let deleted = db
        .delete_installed_tools_for("email/gmail")
        .expect("delete");
    assert_eq!(deleted, 1);
    assert!(db.get_tool_plugin_id("gmail").unwrap().is_none());
}

#[test]
fn full_uninstall_leaves_no_artifacts() {
    let db = test_db();
    let plugin_id = "messaging/telegram";

    // Simulate full install
    db.insert_plugin(plugin_id, "Telegram", "channel", "messaging")
        .expect("insert plugin");
    db.insert_installed_channel(
        "telegram",
        "Telegram bot",
        "native",
        plugin_id,
        "/webhook/telegram",
    )
    .expect("insert channel");
    db.insert_credential(
        plugin_id,
        "TELEGRAM_BOT_TOKEN",
        "keychain",
        Some("borg-telegram"),
        None,
    )
    .expect("insert cred");
    db.insert_file_hash(plugin_id, "telegram/channel.toml", "abc123")
        .expect("insert hash");

    // Simulate full uninstall (same operations as TUI handler)
    db.delete_plugin(plugin_id).expect("delete plugin");
    db.delete_installed_channels_for(plugin_id)
        .expect("delete channels");
    db.delete_credentials_for(plugin_id).expect("delete creds");
    db.delete_file_hashes(plugin_id).expect("delete hashes");

    // Verify nothing remains
    assert!(db.list_plugins().unwrap().is_empty());
    assert!(db.get_channel_plugin_id("telegram").unwrap().is_none());
    assert!(db.get_file_hashes(plugin_id).unwrap().is_empty());
    assert_eq!(db.delete_credentials_for(plugin_id).unwrap(), 0);
}

#[test]
fn insert_credential_round_trip() {
    let db = test_db();
    db.insert_plugin("email/gmail", "Gmail", "tool", "email")
        .expect("insert cust");
    db.insert_credential(
        "email/gmail",
        "GMAIL_TOKEN",
        "env",
        None,
        Some("GMAIL_TOKEN"),
    )
    .expect("insert cred 1");
    db.insert_credential(
        "email/gmail",
        "GMAIL_SECRET",
        "env",
        None,
        Some("GMAIL_SECRET"),
    )
    .expect("insert cred 2");
    let deleted = db.delete_credentials_for("email/gmail").expect("delete");
    assert_eq!(deleted, 2);
}
