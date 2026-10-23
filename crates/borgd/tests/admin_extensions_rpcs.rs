//! Integration tests for the skill and MCP-server management RPCs. Each test
//! seeds a temp BORG_HOME, serves `AdminSvc` over a UDS, and drives it with
//! the tonic client the desktop and web apps use, so the assertions cover
//! the wire mapping as well as the store underneath.
//!
//! BORG_HOME is process-global, so the tests take one mutex and run in turn.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::type_complexity,
    clippy::await_holding_lock
)]

use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::{
    Empty, McpServerPut, McpServerRef, McpServerSpec, SkillEnabledMutation, SkillPut, SkillRef,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Code;
use tower::service_fn;

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    tmp: tempfile::TempDir,
    socket: PathBuf,
    _shutdown: oneshot::Sender<()>,
}

async fn start_harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary are serialized via SERIAL.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }
    let _ = Database::open().await.expect("seed DB open");
    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let admin = AdminSvc::new(SessionRegistry::new());
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });
    let started = std::time::Instant::now();
    while UnixStream::connect(&socket).await.is_err() {
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Harness {
        tmp,
        socket,
        _shutdown: tx,
    }
}

async fn channel(socket: &Path) -> Channel {
    let p = socket.to_path_buf();
    Endpoint::try_from(protocols::TONIC_UDS_PLACEHOLDER_URL)
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let p = p.clone();
            async move {
                let s = UnixStream::connect(&p).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .unwrap()
}

fn stdio_spec(command: &str, args: &[&str]) -> McpServerSpec {
    McpServerSpec {
        enabled: true,
        transport: "stdio".into(),
        command: command.into(),
        args: args.iter().map(ToString::to_string).collect(),
        env: HashMap::from([("A".to_string(), "1".to_string())]),
        ..Default::default()
    }
}

#[tokio::test]
async fn mcp_server_rpcs_create_list_edit_test_and_remove() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let mut admin = AdminClient::new(channel(&h.socket).await);

    // Empty to start, and the templates are advertised so a client can
    // offer them without a hard-coded list.
    let list = admin.list_mcp_servers(Empty {}).await.unwrap().into_inner();
    assert!(list.servers.is_empty());
    assert_eq!(list.presets, vec!["time", "filesystem", "github", "notion"]);
    assert!(!list.mcp_enabled, "MCP is opt-in by default");

    // A bad id is refused before anything is written.
    let err = admin
        .put_mcp_server(McpServerPut {
            name: "bad name".into(),
            spec: Some(stdio_spec("/nonexistent/mcp", &[])),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    // Neither a spec nor a preset is refused too.
    let err = admin
        .put_mcp_server(McpServerPut {
            name: "empty".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    // Create from a spec. Zero knobs take the daemon's defaults.
    let created = admin
        .put_mcp_server(McpServerPut {
            name: "probe".into(),
            spec: Some(stdio_spec("/nonexistent/mcp-bin", &["--flag"])),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.detail, "/nonexistent/mcp-bin --flag");
    let spec = created.spec.unwrap();
    assert_eq!(spec.connect_timeout_ms, 60_000);
    assert_eq!(spec.rate_limit_per_minute, 60);
    assert_eq!(spec.max_tools, 32);
    assert_eq!(spec.env.get("A").map(String::as_str), Some("1"));

    // Same name again needs `overwrite`.
    let err = admin
        .put_mcp_server(McpServerPut {
            name: "probe".into(),
            spec: Some(stdio_spec("/other", &[])),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);

    // Create from a preset: the github template carries a secret
    // placeholder, never a literal token.
    let gh = admin
        .put_mcp_server(McpServerPut {
            name: "github".into(),
            preset: "github".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        gh.spec
            .unwrap()
            .env
            .get("GITHUB_PERSONAL_ACCESS_TOKEN")
            .map(String::as_str),
        Some("${SECRET:github_pat}")
    );

    // Both rows come back sorted, with what the CLI would print, and the
    // stored JSON is the shape `McpConfig` reads.
    let list = admin.list_mcp_servers(Empty {}).await.unwrap().into_inner();
    let names: Vec<&str> = list.servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["github", "probe"]);
    let config = cortex::config::Config::load_from_db().await.unwrap();
    assert!(config.mcp.servers.contains_key("probe"));
    assert!(config.mcp.servers.contains_key("github"));

    // Edit: flipping `enabled` through an overwrite keeps the rest of the
    // row, including the tool filter the spec does not carry.
    cortex::config::mcp_store::mutate_servers(|map| {
        map.get_mut("probe").unwrap().tools.exclude = vec!["dangerous_*".into()];
        Ok(())
    })
    .await
    .unwrap();
    let mut off = stdio_spec("/nonexistent/mcp-bin", &["--flag"]);
    off.enabled = false;
    off.call_timeout_ms = 9_000;
    let edited = admin
        .put_mcp_server(McpServerPut {
            name: "probe".into(),
            spec: Some(off),
            overwrite: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(edited.state, "disabled");
    let config = cortex::config::Config::load_from_db().await.unwrap();
    let probe = &config.mcp.servers["probe"];
    assert!(!probe.enabled);
    assert_eq!(probe.call_timeout_ms, 9_000);
    assert_eq!(probe.tools.exclude, vec!["dangerous_*".to_string()]);
    let list = admin.list_mcp_servers(Empty {}).await.unwrap().into_inner();
    let row = list.servers.iter().find(|s| s.name == "probe").unwrap();
    assert_eq!(row.state, "disabled");

    // A test against a command that does not exist is an answer, not an
    // RPC failure; an unknown name is NOT_FOUND.
    let result = admin
        .test_mcp_server(McpServerRef {
            name: "probe".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!result.ok);
    assert!(!result.error.is_empty(), "the failure must say why");
    let err = admin
        .test_mcp_server(McpServerRef {
            name: "ghost".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    // Remove, then removing again is NOT_FOUND.
    admin
        .remove_mcp_server(McpServerRef {
            name: "probe".into(),
        })
        .await
        .unwrap();
    let err = admin
        .remove_mcp_server(McpServerRef {
            name: "probe".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let list = admin.list_mcp_servers(Empty {}).await.unwrap().into_inner();
    assert_eq!(list.servers.len(), 1);
    assert_eq!(list.servers[0].name, "github");

    // The http path: a bearer token lands as structured auth, a bare host
    // is refused.
    let http = McpServerSpec {
        enabled: true,
        transport: "http".into(),
        url: "https://mcp.example.com/mcp".into(),
        bearer_token: "${SECRET:tok}".into(),
        headers: HashMap::from([("X-Team".to_string(), "a".to_string())]),
        ..Default::default()
    };
    let remote = admin
        .put_mcp_server(McpServerPut {
            name: "remote".into(),
            spec: Some(http.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(remote.detail, "https://mcp.example.com/mcp");
    assert_eq!(remote.spec.unwrap().bearer_token, "${SECRET:tok}");
    let err = admin
        .put_mcp_server(McpServerPut {
            name: "bare".into(),
            spec: Some(McpServerSpec {
                url: "mcp.example.com".into(),
                ..http
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn skill_rpcs_catalog_create_read_toggle_and_delete() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = start_harness().await;
    let mut admin = AdminClient::new(channel(&h.socket).await);

    // The catalog is the full view: default-off built-ins are listed as
    // off, mandatory ones are flagged, hidden ones stay hidden.
    let catalog = admin
        .list_skill_catalog(Empty {})
        .await
        .unwrap()
        .into_inner();
    let weather = catalog
        .skills
        .iter()
        .find(|s| s.name == "weather")
        .expect("weather");
    assert!(!weather.enabled && !weather.default_enabled);
    assert_eq!(weather.source, "builtin");
    let scheduler = catalog
        .skills
        .iter()
        .find(|s| s.name == "scheduler")
        .expect("scheduler");
    assert!(scheduler.mandatory && scheduler.enabled);
    assert!(
        catalog.skills.iter().all(|s| s.name != "slack"),
        "hidden skills stay hidden"
    );
    let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "sorted so a client can rank on top of a stable order"
    );

    // A built-in reads back its embedded source, with no path to edit.
    let detail = admin
        .get_skill(SkillRef {
            name: "weather".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(detail.content.starts_with("---"));
    assert!(detail.path.is_empty());

    // Writing under a built-in name, or with a mismatched frontmatter, is
    // refused and nothing lands on disk.
    let skills_root = h.tmp.path().join("skills");
    let err = admin
        .put_skill(SkillPut {
            name: "weather".into(),
            content: "---\nname: weather\ndescription: mine\n---\nbody".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = admin
        .put_skill(SkillPut {
            name: "my-release-notes".into(),
            content: "---\nname: other\ndescription: d\n---\nbody".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("other"), "{}", err.message());
    assert!(!skills_root.join("my-release-notes").exists());

    // Create a user skill: it lands at skills/<name>/SKILL.md and joins the
    // catalog as yours, enabled.
    let content =
        "---\nname: my-release-notes\ndescription: Write release notes.\n---\n\nSteps here.\n";
    let created = admin
        .put_skill(SkillPut {
            name: "my-release-notes".into(),
            content: content.into(),
        })
        .await
        .unwrap()
        .into_inner();
    let entry = created.entry.unwrap();
    assert_eq!(entry.source, "user");
    // The loader keeps an unlisted skill off; a skill created here starts
    // on, through the same per-skill setting the CLI's `enable` writes.
    assert!(entry.enabled && entry.available, "{entry:?}");
    assert!(!entry.default_enabled);
    assert_eq!(entry.description, "Write release notes.");
    assert_eq!(created.content, content);
    let file = skills_root.join("my-release-notes").join("SKILL.md");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
    assert_eq!(created.path, file.display().to_string());
    let catalog = admin
        .list_skill_catalog(Empty {})
        .await
        .unwrap()
        .into_inner();
    assert!(catalog
        .skills
        .iter()
        .any(|s| s.name == "my-release-notes" && s.source == "user"));

    // Replace it in place.
    let edited = "---\nname: my-release-notes\ndescription: Edited.\n---\n\nNew steps.\n";
    let replaced = admin
        .put_skill(SkillPut {
            name: "my-release-notes".into(),
            content: edited.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replaced.entry.unwrap().description, "Edited.");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);

    // Toggle: off writes the per-skill setting and reads back disabled; a
    // mandatory skill cannot be turned off; an unknown name is NOT_FOUND
    // and writes nothing.
    let off = admin
        .set_skill_enabled(SkillEnabledMutation {
            name: "my-release-notes".into(),
            enabled: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!off.entry.unwrap().enabled);
    let db = Database::open().await.unwrap();
    assert_eq!(
        db.get_setting("skills.entries.my-release-notes.enabled")
            .await
            .unwrap()
            .as_deref(),
        Some("false")
    );
    let on = admin
        .set_skill_enabled(SkillEnabledMutation {
            name: "weather".into(),
            enabled: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(on.entry.unwrap().enabled);
    let err = admin
        .set_skill_enabled(SkillEnabledMutation {
            name: "scheduler".into(),
            enabled: false,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    let err = admin
        .set_skill_enabled(SkillEnabledMutation {
            name: "typo".into(),
            enabled: true,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(db
        .get_setting("skills.entries.typo.enabled")
        .await
        .unwrap()
        .is_none());

    // Delete removes the directory; a built-in or a missing skill is refused.
    std::fs::write(skills_root.join("my-release-notes").join("extra.txt"), "x").unwrap();
    admin
        .delete_skill(SkillRef {
            name: "my-release-notes".into(),
        })
        .await
        .unwrap();
    assert!(!skills_root.join("my-release-notes").exists());
    let err = admin
        .delete_skill(SkillRef {
            name: "my-release-notes".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = admin
        .delete_skill(SkillRef {
            name: "weather".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = admin
        .get_skill(SkillRef {
            name: "my-release-notes".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}
