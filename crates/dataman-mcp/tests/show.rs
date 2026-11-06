//! `show_in_dataman` with an app there (a stand-in that records where it was sent), and the
//! quickstart's connections as the approved rules see them.

use std::sync::{Arc, Mutex};

use dataman_core::approved::Approved;
use dataman_core::{ConfigStore, ConnectionProfile, Dirs, Environment, McpAccess, MemorySecrets, SecretStore, SettingsStore};
use dataman_mcp::hub::SHOWS;
use dataman_mcp::{Hub, Place, Registry, Server};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

#[derive(Default)]
struct App {
    places: Mutex<Vec<Place>>,
    busy: Mutex<bool>,
}

impl dataman_mcp::Front for App {
    fn approve(&self, _: dataman_mcp::Approval) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }
    fn show(&self, place: Place) -> BoxFuture<'static, Result<(), String>> {
        if *self.busy.lock().unwrap() {
            return Box::pin(async { Err("a sheet is open".to_string()) });
        }
        self.places.lock().unwrap().push(place);
        Box::pin(async { Ok(()) })
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    dirs: Dirs,
    hub: Arc<Hub>,
    reg: Arc<Registry>,
    app: Arc<App>,
}

impl Rig {
    fn new(saved: Vec<ConnectionProfile>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("d"));
        let store = ConfigStore::open(&dirs).unwrap();
        for p in saved {
            store.upsert(p).unwrap();
        }
        let hub = Hub::new(dirs.clone(), Arc::new(MemorySecrets::default())).unwrap();
        let app = Arc::new(App::default());
        hub.set_front(Some(app.clone()));
        let reg = dataman_mcp::registry(&hub.dirs);
        Rig { _dir: dir, dirs, hub, reg, app }
    }

    fn call(&self, args: Value) -> (bool, String) {
        let server = Server::new(self.hub.clone(), self.reg.clone(), "stdio");
        let tool = self.reg.find("show_in_dataman").unwrap();
        let r = self.hub.handle().block_on(server.run_tool(tool, args.as_object().unwrap().clone(), "show-test".into(), Default::default()));
        let v = serde_json::to_value(r).unwrap();
        (v["isError"] == json!(true), v["content"][0]["text"].as_str().unwrap_or_default().to_string())
    }

    fn places(&self) -> Vec<Place> {
        self.app.places.lock().unwrap().clone()
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.dirs.connections_file()).unwrap_or_default()
    }
}

fn conn(id: &str, tier: Option<McpAccess>) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(id);
    (p.id, p.host, p.user, p.database, p.mcp) = (id.into(), format!("{id}.internal"), "app".into(), "app".into(), tier);
    p
}

#[test]
fn each_destination_arrives_as_its_place() {
    let r = Rig::new(vec![conn("shop", Some(McpAccess::Read))]);
    r.hub.set_show_limit(100, std::time::Duration::from_secs(60));
    let before = r.file();
    let (err, text) = r.call(json!({"destination": "connection", "connection": "shop"}));
    assert!(!err && text.contains("\"shown\":\"connection\"") && text.contains("\"connection\":\"shop\""), "{text}");
    assert!(!text.contains("shop.internal"), "no more than list_connections: {text}");
    r.call(json!({"destination": "agents_protection", "connection": "shop"}));
    r.call(json!({"destination": "agent_login", "connection": "shop"}));
    r.call(json!({"destination": "agents_general"}));
    r.call(json!({"destination": "settings_mcp"}));
    r.call(json!({"destination": "outside_review"}));
    let p = r.places();
    assert!(
        matches!(
            p.as_slice(),
            [Place::Connection(a), Place::Protection(b), Place::AgentLogin(c), Place::AgentsGeneral, Place::SettingsMcp, Place::OutsideReview] if [a, b, c] == ["shop", "shop", "shop"]
        ),
        "{p:?}"
    );
    assert_eq!(r.file(), before, "nothing saved");
    let log = std::fs::read_to_string(r.dirs.mcp_audit_file()).unwrap();
    let done: Vec<Value> = log.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|l| l["outcome"] != "started").collect();
    assert!(done.iter().any(|l| l["tool"] == "show_in_dataman" && l["connection"] == "shop" && l["client"] == "show-test" && l["outcome"] == "ok"), "{log}");
    std::mem::forget(r.hub);
}

#[test]
fn a_new_connection_is_filled_marked_and_never_saved() {
    let r = Rig::new(vec![]);
    let (err, text) = r.call(json!({"destination": "new_connection", "name": "orders", "engine": "mysql", "host": "db.acme.io", "user": "app", "database": "orders"}));
    assert!(!err && text.contains("\"connection\":\"orders\""), "{text}");
    let places = r.places();
    let [Place::NewConnection(p)] = places.as_slice() else { panic!("{places:?}") };
    assert_eq!((p.name.as_str(), p.engine.slug(), p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()), ("orders", "mysql", "db.acme.io", 3306, "app", "orders"));
    assert!(p.agent_made, "the dialog says who filled it");
    assert_eq!(p.mcp, None, "agents get what a new connection gets");
    assert_eq!(r.hub.core.secrets.get(&p.id).unwrap(), None, "no password anywhere");
    assert!(ConfigStore::open(&r.dirs).unwrap().connections().is_empty(), "nothing saved");
    std::mem::forget(r.hub);
}

/// The twin of a closed connection is told nothing different, and saving it gives agents nothing.
#[test]
fn a_twin_of_a_guarded_connection_starts_closed_and_says_nothing() {
    let mut prod = conn("prod", None);
    prod.env = Environment::Production;
    let r = Rig::new(vec![prod]);
    let (_, plain) = r.call(json!({"destination": "new_connection", "name": "x", "host": "other.internal", "user": "app"}));
    let (err, twin) = r.call(json!({"destination": "new_connection", "name": "x", "host": "prod.internal", "user": "app"}));
    assert!(!err && !twin.contains("prod"), "{twin}");
    assert_eq!(plain, twin, "the reply tells them apart in no way");
    let places = r.places();
    let [Place::NewConnection(a), Place::NewConnection(b)] = places.as_slice() else { panic!("{places:?}") };
    assert_eq!((a.mcp, b.mcp), (None, Some(McpAccess::Off)));
    std::mem::forget(r.hub);
}

#[test]
fn closed_connections_are_no_destination() {
    let off = conn("off", Some(McpAccess::Off));
    let mut prod = conn("prod", None);
    prod.env = Environment::Production;
    let mut refused = conn("refused", Some(McpAccess::Write));
    refused.agents_refused = true;
    let r = Rig::new(vec![off, prod, refused, conn("open", None)]);
    for dest in ["connection", "agents_protection", "agent_login"] {
        for name in ["off", "prod", "refused"] {
            let (err, text) = r.call(json!({"destination": dest, "connection": name}));
            assert!(err && text.starts_with("refused") && !text.contains(".internal"), "{dest} {name}: {text}");
        }
        let (err, text) = r.call(json!({"destination": dest, "connection": "no such"}));
        assert!(err && text.contains("no connection named"), "{text}");
    }
    assert!(r.places().is_empty(), "the app was never told: {:?}", r.places());
    std::mem::forget(r.hub);
}

#[test]
fn what_a_destination_does_not_take_is_refused() {
    let r = Rig::new(vec![conn("shop", None)]);
    for args in [
        json!({"destination": "connection", "connection": "shop", "host": "evil.example"}),
        json!({"destination": "agents_protection", "connection": "shop", "name": "x"}),
        json!({"destination": "agents_general", "connection": "shop"}),
        json!({"destination": "settings_mcp", "host": "evil.example"}),
        json!({"destination": "new_connection", "connection": "shop"}),
    ] {
        let (err, text) = r.call(args.clone());
        assert!(err && text.contains("does not take"), "{args}: {text}");
    }
    let (err, text) = r.call(json!({"destination": "new_connection", "name": "SHOP"}));
    assert!(err && text.contains("already exists"), "{text}");
    for bad in ["db.acme.io\u{202E}", "db.acme.io\u{200B}.evil", "a\nb"] {
        let (err, text) = r.call(json!({"destination": "new_connection", "host": bad}));
        assert!(err && text.contains("draw as nothing"), "{bad:?}: {text}");
    }
    assert!(r.places().is_empty(), "{:?}", r.places());
    std::mem::forget(r.hub);
}

#[test]
fn a_looping_agent_runs_out_and_a_refusing_app_costs_nothing() {
    let r = Rig::new(vec![]);
    *r.app.busy.lock().unwrap() = true;
    for _ in 0..SHOWS * 2 {
        let (err, text) = r.call(json!({"destination": "agents_general"}));
        assert!(err && text.contains("a sheet is open"), "{text}");
    }
    *r.app.busy.lock().unwrap() = false;
    for _ in 0..SHOWS {
        let (err, text) = r.call(json!({"destination": "agents_general"}));
        assert!(!err, "{text}");
    }
    let (err, text) = r.call(json!({"destination": "agents_general"}));
    assert!(err && text.contains("came forward"), "{text}");
    assert_eq!(r.places().len(), SHOWS);
    r.hub.denied("show-test");
    r.hub.set_show_limit(100, std::time::Duration::from_secs(60));
    let (err, text) = r.call(json!({"destination": "agents_general"}));
    assert!(err && text.contains("just denied"), "{text}");
    assert_eq!(r.places().len(), SHOWS);
    std::mem::forget(r.hub);
}

#[test]
fn switched_off_nothing_arrives() {
    let r = Rig::new(vec![conn("shop", None)]);
    std::fs::write(r.dirs.settings_file(), "mcp_enabled = false\n").unwrap();
    for args in [json!({"destination": "settings_mcp"}), json!({"destination": "connection", "connection": "shop"}), json!({"destination": "new_connection"})] {
        let (err, text) = r.call(args.clone());
        assert!(err && text.contains("turned off"), "{args}: {text}");
    }
    assert!(r.places().is_empty());
    std::mem::forget(r.hub);
}

/// The quickstart with the app closed: the tool process writes alone, with the keyring the app
/// reads, and when the app opens nothing waits for review. An edit by anyone else still does.
#[test]
fn a_quickstart_leaves_nothing_for_review() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs::at(dir.path().join("d"));
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets::default());
    let approved = Approved::always_in(secrets.clone());
    ConfigStore::open_with(&dirs, approved.clone()).unwrap().upsert(conn("mine", Some(McpAccess::Write))).unwrap();

    let hub = Hub::with_approved(dirs.clone(), secrets.clone(), approved.clone()).unwrap();
    let reg = dataman_mcp::registry(&hub.dirs);
    let server = Server::new(hub.clone(), reg.clone(), "stdio");
    let create = |args: Value| {
        let tool = reg.find("create_connection").unwrap();
        let r = hub.handle().block_on(server.run_tool(tool, args.as_object().unwrap().clone(), "agent".into(), Default::default()));
        let v = serde_json::to_value(r).unwrap();
        assert!(v["isError"] != json!(true), "{args}: {v}");
    };
    create(json!({"name": "pg", "host": "pg.acme.io", "user": "app", "database": "orders"}));
    create(json!({"name": "my", "engine": "mysql", "host": "my.acme.io", "user": "app"}));
    create(json!({"name": "cache", "engine": "redis", "host": "localhost", "user": "default"}));
    create(json!({"name": "live", "host": "live.acme.io", "user": "app", "env": "production"}));

    let opened = ConfigStore::open_with(&dirs, approved.clone()).unwrap();
    assert_eq!(opened.held(), vec![], "nothing waits");
    assert_eq!(opened.connections().len(), 5, "and all are there");
    assert!(opened.connections().iter().filter(|p| p.name != "mine").all(|p| p.agent_made));
    assert_eq!(SettingsStore::open_with(&dirs, approved.clone()).held(), vec![], "no setting was touched");

    let mut sneaky = opened.get("mine").unwrap();
    sneaky.mcp = Some(McpAccess::Full);
    ConfigStore::open(&dirs).unwrap().upsert(sneaky).unwrap();
    let again = ConfigStore::open_with(&dirs, approved).unwrap();
    assert_eq!(again.held().len(), 1, "an edit by anyone else waits, so this can fail");
    std::mem::forget(hub);
}
