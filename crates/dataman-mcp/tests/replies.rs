//! The replies kept next to the audit log hold what the agent received and nothing else: hidden
//! columns absent, sensitive columns masked, no raw error. Skips without the e2e PostgreSQL.

use std::sync::Arc;
use std::time::Duration;

use dataman_core::{Dirs, MemorySecrets};
use dataman_mcp::{Approval, Front, Hub, Registry, Server};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Yes;

impl Front for Yes {
    fn approve(&self, _: Approval) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
}

fn pg() -> Option<(String, u16)> {
    let v = std::env::var("DATAMAN_PG").unwrap_or_else(|_| "127.0.0.1:55432".into());
    let (h, p) = v.rsplit_once(':')?;
    let p: u16 = p.parse().ok()?;
    std::net::TcpStream::connect_timeout(&format!("{h}:{p}").parse().ok()?, Duration::from_secs(1)).ok()?;
    Some((h.to_string(), p))
}

fn call(s: &Server, tool: &str, args: Value) -> Result<Value, String> {
    let t = s.registry.find(tool).unwrap();
    let r = s.hub.core.block_on(s.run_tool(t, args.as_object().unwrap().clone(), "replies-test".into(), CancellationToken::new()));
    let text = match &r.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("{other:?}"),
    };
    if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
}

#[test]
fn replies_hold_only_what_the_agent_received() {
    let Some((h, p)) = pg() else {
        eprintln!("skipping: no PostgreSQL");
        return;
    };
    let s = format!("mcp_replies_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("dataman");
    std::fs::create_dir_all(&cfg).unwrap();
    let conn = |id: &str, extra: &str| format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{h}\"\nport = {p}\nuser = \"postgres\"\ndatabase = \"dataman\"\nsslmode = \"disable\"\n{extra}\n");
    let rules = format!(
        "mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"people.email\", how = \"partial\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"people.secret\", kind = \"column\" }}]\n"
    );
    std::fs::write(cfg.join("connections.toml"), [conn("admin", "mcp = \"full\""), conn("agent", &rules)].concat()).unwrap();
    let pgpass = dir.path().join("pgpass");
    std::fs::write(&pgpass, format!("{h}:{p}:*:postgres:dataman\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let hub = Hub::new(Dirs::at(&cfg), Arc::new(MemorySecrets::default())).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    hub.set_mask_key(dataman_core::privacy::HashKey::fixed(b"replies-test"));
    hub.set_front(Some(Arc::new(Yes)));
    let srv = Server::new(hub, Arc::new(Registry::standard(None)), "stdio");
    call(
        &srv,
        "execute",
        json!({"connection": "admin", "sql": format!(
            "CREATE SCHEMA {s}; CREATE TABLE {s}.people (id int PRIMARY KEY, name text, email text UNIQUE, secret text); \
             INSERT INTO {s}.people VALUES (1, 'Ana', 'ana@acme.com', 'hunter2-raw')"
        )}),
    )
    .unwrap();

    let got = call(&srv, "run_query", json!({"connection": "agent", "sql": format!("SELECT * FROM {s}.people")})).unwrap();
    let err = call(&srv, "execute", json!({"connection": "agent", "sql": format!("INSERT INTO {s}.people (id, name, email) VALUES (2, 'Bia', 'ana@acme.com')")})).unwrap_err();

    let log = cfg.join("mcp-audit.jsonl");
    let entries: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let id_of = |tool: &str, ok: bool| entries.iter().rev().find(|e| e["tool"] == tool && e["connection"] == "agent" && (e["outcome"] == "ok") == ok).and_then(|e| e["id"].as_str()).map(str::to_string);
    let read_id = id_of("run_query", true).expect("the read is logged with an id");
    let kept = dataman_mcp::audit::reply(&log, &read_id).expect("its reply is on disk");
    assert_eq!(serde_json::from_str::<Value>(&kept.reply).unwrap(), got, "the reply on disk is exactly what the agent received");
    assert!(kept.reply.contains("a•••@acme.com"), "{}", kept.reply);
    for raw in ["ana@acme.com", "hunter2-raw", "secret"] {
        assert!(!kept.reply.contains(raw), "{raw} must not be on disk: {}", kept.reply);
    }
    let failed_id = id_of("execute", false).expect("the failed write is logged");
    assert!(dataman_mcp::audit::reply(&log, &failed_id).is_none(), "an error keeps no reply");
    // The statements callers wrote stay in the log as they were sent; what came from the database
    // (rows, error values) never reaches the disk unfiltered.
    let replies = std::fs::read_to_string(dataman_mcp::audit::replies_path(&log)).unwrap();
    assert!(!replies.contains("ana@acme.com") && !replies.contains("hunter2-raw"), "{replies}");
    let logged = entries.iter().rev().find(|e| e["id"] == failed_id.as_str()).and_then(|e| e["error"].as_str()).unwrap_or_default().to_string();
    assert!(err.contains("‹value›") && logged.contains("‹value›") && !logged.contains("ana@acme.com"), "{logged}");

    call(&srv, "execute", json!({"connection": "admin", "sql": format!("DROP SCHEMA {s} CASCADE")})).unwrap();
    srv.hub.close_all();
    std::mem::forget(srv);
}
