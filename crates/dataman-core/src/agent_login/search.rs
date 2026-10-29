//! Elasticsearch and OpenSearch with security on: a role and a user. The role names the indices
//! agents may use (those that exist now); hidden fields are left out by field level security.
//! OpenSearch masks fields itself (`masked_fields`, a hash); Elasticsearch cannot mask, so a
//! masked field is left out there too. A role with field level security can't write those
//! indices: writes there are refused by the server. Without security there are no users at all:
//! the option says so.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::*;
use crate::backend::search::{Client, HttpError, Wire};

fn base(os: bool) -> &'static str {
    if os { "/_plugins/_security/api" } else { "/_security" }
}

fn role_path(os: bool, user: &str) -> String {
    if os { format!("/_plugins/_security/api/roles/{}", enc(user)) } else { format!("/_security/role/{}", enc(user)) }
}

fn user_path(os: bool, user: &str) -> String {
    if os { format!("/_plugins/_security/api/internalusers/{}", enc(user)) } else { format!("/_security/user/{}", enc(user)) }
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub struct Desired {
    pub role: Value,
    pub access: Vec<Access>,
    pub hidden: Vec<String>,
    pub not_granted: Vec<NotGranted>,
    pub lines: Vec<String>,
}

pub fn desired(spec: &Spec, inv: &Inventory, os: bool, fls: bool) -> Desired {
    let mut inv = inv.clone();
    with_rule_fields(spec, &mut inv);
    let (all, hidden) = access(spec, &inv);
    let (mut entries, mut lines, mut ok, mut not_granted) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut whole_read, mut whole_write) = (Vec::new(), Vec::new());
    for a in all {
        if !spec.rows {
            continue;
        }
        if a.whole {
            if a.write { whole_write.push(a.name.clone()) } else { whole_read.push(a.name.clone()) }
            ok.push(a);
            continue;
        }
        if !fls {
            not_granted.push(NotGranted { object: a.name.clone(), reason: "field level security needs a Platinum or Enterprise license (or a trial)".into(), sql: Vec::new() });
            continue;
        }
        let mut except: Vec<String> = a.hidden.clone();
        let masked: Vec<String> = a.masked.iter().map(|(c, _)| c.clone()).collect();
        if !os {
            except.extend(masked.iter().cloned());
        }
        // A field's sub-fields (`city.raw`) go with it.
        let except: Vec<String> = except.iter().flat_map(|f| [f.clone(), format!("{f}.*")]).collect();
        let mut e = if os {
            json!({ "index_patterns": [a.name], "allowed_actions": ["read", "indices:admin/mappings/get", "indices:monitor/stats"], "fls": except.iter().map(|f| format!("~{f}")).collect::<Vec<_>>() })
        } else {
            json!({ "names": [a.name], "privileges": ["read", "view_index_metadata"], "field_security": { "grant": ["*"], "except": except } })
        };
        if os && !masked.is_empty() {
            e["masked_fields"] = json!(masked);
        }
        lines.push(format!("read ON {}", a.name));
        for f in &except {
            lines.push(format!("except {f} ON {}", a.name));
        }
        if os {
            for f in &masked {
                lines.push(format!("masked {f} ON {}", a.name));
            }
        }
        entries.push(e);
        ok.push(a);
    }
    let mut push_whole = |names: &[String], write: bool| {
        if names.is_empty() {
            return;
        }
        for n in names {
            lines.push(format!("read ON {n}"));
            if write {
                lines.push(format!("write ON {n}"));
            }
        }
        let e = if os {
            let mut acts = vec!["read", "indices:admin/mappings/get", "indices:monitor/stats"];
            if write {
                acts.push("write");
            }
            json!({ "index_patterns": names, "allowed_actions": acts })
        } else {
            let mut privs = vec!["read", "view_index_metadata"];
            if write {
                privs.extend(["index", "delete"]);
            }
            json!({ "names": names, "privileges": privs })
        };
        entries.insert(0, e);
    };
    push_whole(&whole_write, true);
    push_whole(&whole_read, false);
    // `GET /` (the server's name and version, read when Dataman connects) and nothing more,
    // unless activity is on.
    let mut cluster: Vec<&str> = vec!["cluster:monitor/main"];
    if spec.activity {
        cluster.push(if os { "cluster_monitor" } else { "monitor" });
    }
    for c in &cluster {
        lines.push(format!("cluster {c}"));
    }
    let role = if os {
        json!({ "description": format!("{MARK} conn={}", spec.conn), "cluster_permissions": cluster, "index_permissions": entries })
    } else {
        json!({ "cluster": cluster, "indices": entries, "metadata": { "dataman": MARK, "conn": spec.conn } })
    };
    lines.sort();
    Desired { role, access: ok, hidden, not_granted, lines }
}

fn rules(spec: &Spec, d: &Desired, os: bool, prod: bool) -> Vec<RuleLine> {
    let mut v = vec![line("A login apart from yours", "a security user and its role", Who::Database)];
    if !spec.rows {
        v.push(line("Structure only", "no index granted", Who::Database));
    } else if spec.write {
        v.push(line("Documents: read and write", "read, index and delete; no index creation, mappings or settings", Who::Database));
    } else {
        v.push(line("Documents: read only", "read only", Who::Database));
    }
    if !d.hidden.is_empty() {
        v.push(line(hidden_label(&d.hidden), "not in the role", Who::Database));
    }
    v.extend(column_lines(
        &d.access,
        ("field level security (those indices are read only)", Who::Database),
        if os { ("OpenSearch masked_fields: a hash, not Partial", Who::Partly) } else { ("left out: Elasticsearch can't mask", Who::Partly) },
        spec.rows,
    ));
    v.push(line(if spec.code { "Scripts" } else { "Run code off" }, "scripts in searches can't be turned off per user", Who::Dataman));
    v.push(line(if spec.activity { "Server activity" } else { "No server activity" }, if spec.activity { "cluster monitor" } else { "no cluster privileges" }, Who::Database));
    v.push(line(time_label(spec.time_limit_s), "no time limit per user", Who::Dataman));
    v.extend(dataman_lines(spec, prod));
    v
}

fn http(group: &str, method: &str, path: String, body: Option<Value>, shown_body: Option<Value>) -> Step {
    let text = |b: &Option<Value>| match b {
        Some(b) => format!("{method} {path}\n{}", serde_json::to_string_pretty(b).unwrap_or_default()),
        None => format!("{method} {path}"),
    };
    Step { shown: text(&shown_body.clone().or_else(|| body.clone())), copy: text(&body), action: Action::Http { method: method.into(), path, body }, group: group.into() }
}

fn user_body(spec: &Spec, hash: &str, os: bool) -> (Value, Value) {
    let (real, shown) = if os {
        let b = |h: &str| json!({ "hash": h, "opendistro_security_roles": [spec.user], "attributes": { "dataman": MARK, "conn": spec.conn } });
        (b(hash), b("$2a$10$…"))
    } else {
        let b = |h: &str| json!({ "password_hash": h, "roles": [spec.user], "metadata": { "dataman": MARK, "conn": spec.conn } });
        (b(hash), b("$2a$10$…"))
    };
    (real, shown)
}

pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, password: &str, prod: bool) -> Plan {
    let os = probe.flavor.opensearch;
    let d = desired(spec, inv, os, probe.flavor.superuser);
    let mut steps = vec![http("the role: indices and fields the agent may use", "PUT", role_path(os, &spec.user), Some(d.role.clone()), None)];
    let (real, shown) = user_body(spec, &hash::bcrypt(password), os);
    steps.push(http("the login (its password sent as a bcrypt hash)", "PUT", user_path(os, &spec.user), Some(real), Some(shown)));
    if !d.hidden.is_empty() {
        steps.push(Step::note("", format!("hidden: {} → not in the role", short_list(&d.hidden))));
    }
    seal(spec, d, probe, steps, prod)
}

fn seal(spec: &Spec, d: Desired, probe: &Probe, steps: Vec<Step>, prod: bool) -> Plan {
    let os = probe.flavor.opensearch;
    let rules = rules(spec, &d, os, prod);
    Plan {
        engine: Some(if os { Engine::OpenSearch } else { Engine::Elasticsearch }),
        user: spec.user.clone(),
        steps,
        rules,
        lines: d.lines,
        not_granted: d.not_granted,
        lacks: probe.lacks.clone(),
        made: vec![format!("role:{}", spec.user)],
        granted: d.access.len(),
        hidden: d.hidden,
        manual: probe.manual.clone(),
        ..Default::default()
    }
    .seal()
}

/// The role replaced whole (atomic).
pub fn sync_plan(spec: &Spec, inv: &Inventory, probe: &Probe, prod: bool) -> Plan {
    let os = probe.flavor.opensearch;
    let d = desired(spec, inv, os, probe.flavor.superuser);
    let steps = vec![http("the role, replaced whole", "PUT", role_path(os, &spec.user), Some(d.role.clone()), None)];
    seal(spec, d, probe, steps, prod)
}

pub fn remove_plan(user: &str, os: bool) -> Plan {
    let steps = vec![http("the login", "DELETE", user_path(os, user), None, None), http("its role", "DELETE", role_path(os, user), None, None)];
    Plan { engine: Some(if os { Engine::OpenSearch } else { Engine::Elasticsearch }), user: user.into(), steps, ..Default::default() }
}

pub fn rotate_step(spec_user: &str, conn: &str, password: &str, os: bool) -> Step {
    let h = hash::bcrypt(password);
    if os {
        let b = |h: &str| json!({ "hash": h, "opendistro_security_roles": [spec_user], "attributes": { "dataman": MARK, "conn": conn } });
        return http("a new password", "PUT", user_path(true, spec_user), Some(b(&h)), Some(b("$2a$10$…")));
    }
    http("a new password", "PUT", format!("/_security/user/{}/_password", enc(spec_user)), Some(json!({ "password_hash": h })), Some(json!({ "password_hash": "$2a$10$…" })))
}

// ---- Live ----

pub(crate) async fn call(c: &Client, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    c.call(method, path, body.cloned().map(Wire::Json)).await
}

fn status(e: &anyhow::Error) -> Option<u16> {
    e.chain().find_map(|c| c.downcast_ref::<HttpError>()).map(|h| h.status)
}

pub(crate) async fn probe(c: &Client, user: &str, conn: &str) -> Result<Probe> {
    let os = c.version.starts_with("OpenSearch");
    let mut p = Probe { version: c.version.clone(), tls: c.tls_active, ..Default::default() };
    p.flavor.opensearch = os;
    let who = if os { call(c, "GET", "/_plugins/_security/authinfo", None).await } else { call(c, "GET", "/_security/_authenticate", None).await };
    let who = match who {
        Ok(v) => v,
        Err(e) => {
            let text = format!("{e:#}");
            if status(&e) == Some(404) || text.contains("security is disabled") || text.contains("Security must be explicitly enabled") || text.contains("no handler found") || text.contains("security_exception") && text.contains("disabled") {
                p.manual = Some(Manual::NoSecurity);
                p.lacks.push("security is off on this server: there are no users to create (turn security on, then come back)".into());
                p.checks.push((false, "security off".into()));
                return Ok(p);
            }
            return Err(e);
        }
    };
    p.login = (if os { who["user_name"].as_str() } else { who["username"].as_str() }).unwrap_or("").to_string();
    let can = if os {
        // Only an admin of the security plugin may read roles through its REST API.
        call(c, "GET", &format!("{}/roles", base(true)), None).await.is_ok()
    } else {
        call(c, "POST", "/_security/user/_has_privileges", Some(&json!({ "cluster": ["manage_security"] }))).await.map(|v| v["has_all_requested"] == json!(true)).unwrap_or(false)
    };
    p.can_create = can;
    if !can {
        p.lacks.push(format!("{} can't manage security: it needs {}", p.login, if os { "the security REST API (all_access or security_rest_api_access)" } else { "the manage_security cluster privilege" }));
    }
    // Field level security: OpenSearch has it; Elasticsearch from Platinum (or a trial).
    let fls = os || {
        let l = call(c, "GET", "/_license", None).await.unwrap_or(Value::Null);
        let t = l["license"]["type"].as_str().unwrap_or("basic");
        !matches!(t, "basic" | "standard" | "gold")
    };
    p.flavor.superuser = fls;
    if !fls {
        p.checks.push((false, "basic license: no field level security".into()));
    }
    p.checks.push((p.can_create, if p.can_create { format!("{} can manage security", p.login) } else { format!("{} can't manage security", p.login) }));
    p.checks.push((p.tls, if p.tls { "TLS on".into() } else { "TLS off".into() }));
    p.exists = exists(c, user, conn, os).await?;
    Ok(p)
}

pub(crate) async fn exists(c: &Client, user: &str, conn: &str, os: bool) -> Result<Option<bool>> {
    match call(c, "GET", &user_path(os, user), None).await {
        Ok(v) => {
            let u = &v[user];
            let meta = if os { &u["attributes"] } else { &u["metadata"] };
            Ok(Some(meta["dataman"] == MARK && meta["conn"] == conn))
        }
        Err(e) if status(&e) == Some(404) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Open indices but the system's (`.`), with their fields.
pub(crate) async fn inventory(c: &Client) -> Result<Inventory> {
    let mut out = Inventory::default();
    for i in crate::backend::search::indices(c).await? {
        if i.name.starts_with('.') {
            continue;
        }
        let fields = crate::backend::search::fields(c, &i.name).await.unwrap_or_default();
        out.objects.push(Obj { schema: String::new(), name: i.name, kind: ObjKind::Index, columns: fields.into_iter().map(|(f, _)| Col::new(&f)).filter(|c| c.name != "_id").collect(), grantable: true, owner: String::new(), sequences: Vec::new() });
    }
    Ok(out)
}

/// The role as the server has it, as lines.
pub(crate) async fn have(c: &Client, user: &str, os: bool) -> Result<Vec<String>> {
    let v = call(c, "GET", &role_path(os, user), None).await?;
    let r = &v[user];
    let mut lines = Vec::new();
    if os {
        for c in r["cluster_permissions"].as_array().cloned().unwrap_or_default() {
            lines.push(format!("cluster {}", c.as_str().unwrap_or("")));
        }
        for e in r["index_permissions"].as_array().cloned().unwrap_or_default() {
            let acts: Vec<String> = e["allowed_actions"].as_array().cloned().unwrap_or_default().iter().filter_map(|a| a.as_str().map(str::to_string)).collect();
            for n in e["index_patterns"].as_array().cloned().unwrap_or_default() {
                let n = n.as_str().unwrap_or("");
                for a in acts.iter().filter(|a| matches!(a.as_str(), "read" | "write")) {
                    lines.push(format!("{a} ON {n}"));
                }
                for f in e["fls"].as_array().cloned().unwrap_or_default() {
                    lines.push(format!("except {} ON {n}", f.as_str().unwrap_or("").trim_start_matches('~')));
                }
                for f in e["masked_fields"].as_array().cloned().unwrap_or_default() {
                    lines.push(format!("masked {} ON {n}", f.as_str().unwrap_or("")));
                }
            }
        }
    } else {
        for c in r["cluster"].as_array().cloned().unwrap_or_default() {
            lines.push(format!("cluster {}", c.as_str().unwrap_or("")));
        }
        for e in r["indices"].as_array().cloned().unwrap_or_default() {
            let privs: Vec<String> = e["privileges"].as_array().cloned().unwrap_or_default().iter().filter_map(|a| a.as_str().map(str::to_string)).collect();
            for n in e["names"].as_array().cloned().unwrap_or_default() {
                let n = n.as_str().unwrap_or("");
                lines.push(format!("read ON {n}"));
                if privs.iter().any(|p| p == "index") {
                    lines.push(format!("write ON {n}"));
                }
                for f in e["field_security"]["except"].as_array().cloned().unwrap_or_default() {
                    lines.push(format!("except {} ON {n}", f.as_str().unwrap_or("")));
                }
            }
        }
    }
    lines.sort();
    lines.dedup();
    if lines.is_empty() && r.is_null() {
        return Err(anyhow!("the role {user} is not on this server"));
    }
    Ok(lines)
}
