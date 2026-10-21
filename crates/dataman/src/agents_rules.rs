//! Agents › Permissions (what agents may do on a connection, and how much) and Agents › Privacy
//! (what they may see there). Both write the connection's `mcp_rules`; no MCP tool can.

use dataman_core::privacy::{self, AgentView, ItemKind, Mask, Names, ReadFrom, RedisCat, SensitiveCol, Visible, VisibleItem, VisibleMode};
use dataman_core::{ConnectionProfile, Engine, McpAccess};
use gpui::{AnyElement, ClickEvent, Context, SharedString, Window, div, prelude::*};

use crate::agents::{Act, AgentsPage, Tip, checkbox, dropdown, field, label_tip, seg};
use crate::agents_suggest::Which;
use crate::assets::{Icon, icon};
use crate::theme::{Theme, px};
use crate::ui;

/// One "Agents may" row: its name, what it covers on this engine, its rule's value, the tier it
/// needs, and why it cannot be changed here (Privacy forces it off).
struct May {
    id: &'static str,
    name: &'static str,
    what: String,
    on: bool,
    needs: McpAccess,
    forced: Option<Tip>,
}

/// What a row does on the connection's tier, in a word or two (shown on its right).
pub(crate) fn may_state(tier: McpAccess, needs: McpAccess, on: bool, forced: bool, prod: bool) -> (&'static str, bool) {
    match () {
        _ if tier == McpAccess::Off => ("MCP is off", false),
        _ if forced => ("off · Privacy", false),
        _ if tier < needs => (needs_label(needs), false),
        _ if !on => ("never", true),
        _ if needs == McpAccess::Full || (needs == McpAccess::Write && prod) => ("asks you first", true),
        _ => ("allowed", true),
    }
}

fn needs_label(a: McpAccess) -> &'static str {
    match a {
        McpAccess::Full => "needs Full",
        McpAccess::Write => "needs Write",
        _ => "needs Read",
    }
}

fn forced_tip() -> Tip {
    Tip::new("Off while data is kept from agents")
        .lead("Server activity shows other sessions' statements and the values in them.")
        .lead("It stays off while this connection hides or masks data in Privacy.")
}

fn tier_tip() -> Tip {
    Tip::new("MCP tier")
        .lead("How far agents can go on this connection.")
        .item("Off", "Agents can't use it.")
        .item("Read", "Reads only.")
        .item("Write", "Also inserts, and updates or deletes with a real WHERE, up to Rows per write.")
        .item("Full", "Also destructive statements (DROP, TRUNCATE, a DELETE without WHERE…). Each one waits for your Allow.")
        .note("On production, every write waits for your Allow.")
}

fn may_tip() -> Tip {
    Tip::new("Agents may")
        .lead("Rules on top of the MCP tier: switch off what agents should never do here.")
        .item("Checked", "Allowed as far as the tier lets. On the right: allowed, or asks you first.")
        .item("Unchecked", "Never, even with your Allow.")
        .item("Greyed", "The tier doesn't reach it (the right says which one it needs), or Privacy keeps it off.")
}

fn limits_tip() -> Tip {
    Tip::new("Limits")
        .lead("Per call, for agents only.")
        .item("Time per call", "Longest a statement runs before it is stopped. An agent can ask for less.")
        .item("Rows per write", "A write that changes more rows waits for your Allow. Default: 100.")
        .item("Rows per read", "Most rows an agent gets back from one read. Default: 1,000.")
        .item("Size per read", "Most data from one read; past it the reply is cut and the agent is told.")
}

fn read_from_tip() -> Tip {
    Tip::new("Read from")
        .lead("Which members of the replica set answer agents' reads.")
        .item("Primary", "Always the primary.")
        .item("Prefer secondary", "A secondary when one is up, otherwise the primary. Keeps agents' load off the primary.")
        .item("Secondary", "Secondaries only. Reads fail while none is up.")
}

fn activity_of(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => "pg_stat_activity, pg_locks",
        Engine::MySql => "processlist, performance_schema",
        Engine::SqlServer => "sys.dm_exec_sessions, sys.dm_tran_locks",
        Engine::MongoDb => "currentOp, serverStatus, profiler",
        Engine::Elasticsearch | Engine::OpenSearch => "_tasks, _nodes, _cat",
        Engine::Redis => "",
    }
}

fn mays(p: &ConnectionProfile) -> Vec<May> {
    let r = &p.mcp_rules;
    // DDL and procedures are destructive (Full, asked); code in MongoDB and search reads is a read.
    let code_needs = if p.engine.is_sql() { McpAccess::Full } else { McpAccess::Read };
    let (structure, code) = match p.engine {
        Engine::MongoDb => ("createIndex, drop, renameCollection, $out, $merge", "scripts, $where, $function, $accumulator, mapReduce"),
        Engine::Elasticsearch | Engine::OpenSearch => ("index creation and deletion, mappings, settings", "scripts, painless, runtime fields"),
        Engine::SqlServer => ("CREATE, ALTER, DROP, TRUNCATE, GRANT", "EXEC procedures, functions with side effects"),
        _ => ("CREATE, ALTER, DROP, TRUNCATE, GRANT", "DO blocks, CALL, functions with side effects"),
    };
    let forced = r.activity_forced_off().then(forced_tip);
    vec![
        May { id: "structure", name: "Change structure", what: structure.into(), on: r.structure, needs: McpAccess::Full, forced: None },
        May { id: "code", name: "Run code", what: code.into(), on: r.code, needs: code_needs, forced: None },
        May { id: "activity", name: "See server activity", what: activity_of(p.engine).into(), on: r.activity_on(), needs: McpAccess::Read, forced },
    ]
}

fn redis_needs(c: RedisCat) -> McpAccess {
    match c {
        RedisCat::Read => McpAccess::Read,
        RedisCat::Write | RedisCat::Keyspace | RedisCat::Pubsub => McpAccess::Write,
        RedisCat::Scripting | RedisCat::Dangerous => McpAccess::Full,
    }
}

fn tier_label(a: McpAccess) -> &'static str {
    match a {
        McpAccess::Off => "Off",
        McpAccess::Read => "Read",
        McpAccess::Write => "Write",
        McpAccess::Full => "Full",
    }
}

fn placeholder(e: Engine) -> &'static str {
    match e {
        Engine::MongoDb => "collection, collection.field or *",
        Engine::Elasticsearch | Engine::OpenSearch => "index, index.field or *",
        Engine::Redis => "key pattern or key.field",
        _ => "table, table.column or *",
    }
}

fn kb(n: u64) -> String {
    format!("{} KB", n / 1024)
}

impl AgentsPage {
    pub(crate) fn selected_profile(&mut self, cx: &mut Context<Self>) -> Option<ConnectionProfile> {
        let all = self.profiles(cx);
        if self.conn.as_ref().is_none_or(|id| !all.iter().any(|p| &p.id == id)) {
            self.conn = all.first().map(|p| p.id.clone());
        }
        let id = self.conn.clone()?;
        all.into_iter().find(|p| p.id == id)
    }

    fn render_conn_list(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.profiles(cx).into_iter().map(|p| {
            let sel = self.conn.as_deref() == Some(p.id.as_str());
            let a = p.mcp_access();
            let hov = t.hov;
            let id = p.id.clone();
            let sid = SharedString::from(format!("agents-conn-{}", p.name));
            div()
                .children(ui::here(&sid.clone().into()))
                .id(sid)
                .h(px(34.))
                .flex_none()
                .pl(px(32.))
                .pr(px(16.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .when(sel, |d| d.bg(t.prs))
                .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                .children(crate::assets::small_logo(p.engine, 14.))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                .child(div().text_size(px(11.)).text_color(if a == McpAccess::Full { t.warn } else { t.tx3 }).child(a.label()))
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.focus_list(window, cx);
                    v.conn = Some(id.clone());
                    v.allow_error = None;
                    v.yolo_confirm = false;
                    v.scan_note = None;
                    cx.notify();
                }))
        });
        div().id("agents-conns").w(px(250.)).flex_none().py(px(4.)).flex().flex_col().overflow_y_scroll().track_scroll(&self.conns_scroll).border_r_1().border_color(t.line).children(rows).into_any_element()
    }

    fn no_conns(&self, t: &Theme, title: &str) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, title))
            .child(div().flex_1().flex().items_center().justify_center().text_color(t.tx3).child("No saved connections."))
            .into_any_element()
    }

    fn set_rule(&mut self, f: impl FnOnce(&mut privacy::AgentRules), cx: &mut Context<Self>) {
        if let Some(id) = self.conn.clone() {
            self.edit_conn(&id, |p| f(&mut p.mcp_rules), cx);
        }
    }

    // ---- Permissions ----

    pub(crate) fn render_permissions(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = self.selected_profile(cx) else { return self.no_conns(t, "Permissions") };
        let r = p.mcp_rules.clone();
        let tiers: Vec<(McpAccess, &str)> = McpAccess::ALL.iter().map(|a| (*a, tier_label(*a))).collect();
        let mut left = div().flex_1().min_w_0().flex().flex_col().gap(px(24.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(label_tip(t, "agents-tier-tip", "MCP", tier_tip()))
                .child(seg(t, "agents-tier", &tiers, p.mcp_access(), false, cx, |v, a, cx| {
                    if let Some(id) = v.conn.clone() {
                        v.edit_conn(&id, |p| p.mcp = Some(a), cx);
                    }
                }))
                .when(p.env.is_production(), |d| d.child(div().text_size(px(11.)).text_color(t.tx3).child("Production: every write asks."))),
        );
        let tier = p.mcp_access();
        let prod = p.env.is_production();
        let may_rows: Vec<AnyElement> = if p.engine == Engine::Redis {
            RedisCat::ALL
                .iter()
                .map(|c| {
                    let c = *c;
                    let never = (c == RedisCat::Dangerous).then(|| {
                        Tip::new("@dangerous never runs for agents").lead("FLUSHALL, FLUSHDB, CONFIG, DEBUG, KEYS and the like are refused whatever the tier, so an agent can't wipe or reconfigure the server.")
                    });
                    let id = format!("agents-may-{}", c.label().trim_start_matches('@'));
                    self.may_row(t, &id, c.label(), c.examples(), r.redis.allows(c), redis_needs(c), tier, prod, never, cx, move |r| {
                        let on = r.redis.allows(c);
                        r.redis.set(c, !on);
                    })
                })
                .collect()
        } else {
            mays(&p)
                .into_iter()
                .map(|m| {
                    let id = m.id;
                    self.may_row(t, &format!("agents-may-{id}"), m.name, &m.what, m.on, m.needs, tier, prod, m.forced, cx, move |r| match id {
                        "structure" => r.structure = !r.structure,
                        "code" => r.code = !r.code,
                        _ => r.activity = !r.activity,
                    })
                })
                .collect()
        };
        left = left.child(div().flex().flex_col().gap(px(12.)).child(label_tip(t, "agents-may-tip", "AGENTS MAY", may_tip())).children(may_rows));
        if p.engine == Engine::MongoDb {
            let opts: Vec<(ReadFrom, &str)> = ReadFrom::ALL.iter().map(|f| (*f, f.label())).collect();
            left = left.child(div().flex().flex_col().gap(px(8.)).child(label_tip(t, "agents-read-from-tip", "READ FROM", read_from_tip())).child(seg(t, "agents-read-from", &opts, r.read_from.unwrap_or(ReadFrom::Primary), false, cx, |v, f, cx| {
                v.set_rule(|r| r.read_from = (f != ReadFrom::Primary).then_some(f), cx);
            })));
        }
        let limit_row = |label: &str, el: gpui::Div| div().flex().flex_row().items_center().gap(px(12.)).child(div().flex_1().text_color(t.tx2).child(label.to_string())).child(el);
        let times: Vec<(u64, String)> = privacy::TIME_LIMITS.iter().map(|s| (*s, if *s < 60 { format!("{s} s") } else { format!("{} min", s / 60) })).collect();
        let times: Vec<(u64, &str)> = times.iter().map(|(v, l)| (*v, l.as_str())).collect();
        let count = |n: u64| if n >= 1000 && n.is_multiple_of(1000) { format!("{}k", n / 1000) } else { n.to_string() };
        // A value that came from the old global setting shows as a choice of its own.
        let with = |list: &[u64], cur: Option<u64>| -> Vec<u64> {
            let mut v = list.to_vec();
            if let Some(c) = cur.filter(|c| !v.contains(c)) {
                v.push(c);
                v.sort();
            }
            v
        };
        let writes: Vec<(u64, String)> = std::iter::once((0, "Default".to_string())).chain(with(&privacy::WRITE_ROWS, p.mcp_row_limit).into_iter().map(|n| (n, count(n)))).collect();
        let writes: Vec<(u64, &str)> = writes.iter().map(|(v, l)| (*v, l.as_str())).collect();
        let reads: Vec<(u64, String)> = std::iter::once((0, "Default".to_string())).chain(with(&privacy::READ_ROWS, r.read_rows).into_iter().map(|n| (n, count(n)))).collect();
        let reads: Vec<(u64, &str)> = reads.iter().map(|(v, l)| (*v, l.as_str())).collect();
        let sizes: Vec<(u64, String)> = std::iter::once((0, "Default".to_string())).chain(privacy::READ_BYTES.iter().map(|n| (*n, kb(*n)))).collect();
        let sizes: Vec<(u64, &str)> = sizes.iter().map(|(v, l)| (*v, l.as_str())).collect();
        let limits = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(label_tip(t, "agents-limits-tip", "LIMITS", limits_tip()))
            .child(limit_row("Time per call", seg(t, "agents-time", &times, r.time_limit_s.unwrap_or(300), true, cx, |v, s, cx| v.set_rule(|r| r.time_limit_s = (s != 300).then_some(s), cx))))
            .child(limit_row("Rows per write", seg(t, "agents-wrows", &writes, p.mcp_row_limit.unwrap_or(0), true, cx, |v, n, cx| {
                if let Some(id) = v.conn.clone() {
                    v.edit_conn(&id, |p| p.mcp_row_limit = (n != 0).then_some(n), cx);
                }
            })))
            .child(limit_row("Rows per read", seg(t, "agents-rrows", &reads, r.read_rows.unwrap_or(0), true, cx, |v, n, cx| v.set_rule(|r| r.read_rows = (n != 0).then_some(n), cx))))
            .child(limit_row("Size per read", seg(t, "agents-rbytes", &sizes, r.read_bytes.unwrap_or(0), true, cx, |v, n, cx| v.set_rule(|r| r.read_bytes = (n != 0).then_some(n), cx))));
        let mut right = div().w(px(380.)).flex_none().flex().flex_col().gap(px(24.)).child(limits);
        if dataman_core::MCP_ADVANCED {
            if p.engine.is_sql() {
                right = right.child(self.render_allowed(t, &p, window, cx));
            }
            right = right.child(self.render_yolo(t, &p, cx));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "Permissions"))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .border_t_1()
                    .border_color(t.line)
                    .child(self.render_conn_list(t, cx))
                    .child(div().id("agents-perm-body").flex_1().min_w_0().overflow_y_scroll().flex().flex_row().gap(px(40.)).px(px(32.)).py(px(24.)).child(left).child(right)),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn may_row(
        &self,
        t: &Theme,
        id: &str,
        name: &str,
        what: &str,
        on: bool,
        needs: McpAccess,
        tier: McpAccess,
        prod: bool,
        forced: Option<Tip>,
        cx: &mut Context<Self>,
        flip: impl Fn(&mut privacy::AgentRules) + 'static,
    ) -> AnyElement {
        let (state, live) = may_state(tier, needs, on, forced.is_some(), prod);
        let sid = SharedString::from(id.to_string());
        let row = div()
            .children(ui::here(&sid.clone().into()))
            .id(sid)
            .flex()
            .flex_row()
            .gap(px(10.))
            .child(div().pt(px(1.)).child(checkbox(t, on && live, !live)))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .min_w_0()
                    .child(div().text_color(if live { t.tx } else { t.tx3 }).child(name.to_string()))
                    .when(!what.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(t.tx3).child(what.to_string()))),
            )
            .child(div().pt(px(1.)).flex_none().text_size(px(11.)).text_color(t.tx3).child(state));
        let tip = forced.or_else(|| {
            (tier != McpAccess::Off && tier < needs).then(|| {
                Tip::new(format!("{name} {}", needs_label(needs)))
                    .lead(format!("This connection's MCP tier is {}; {} starts at {}.", tier_label(tier), name.to_lowercase(), tier_label(needs)))
                    .note("Raise the tier above to use it. The box keeps your choice for when you do.")
            })
        });
        match (tip, live) {
            (Some(tip), false) => row.tooltip(tip.view()).into_any_element(),
            (_, false) => row.into_any_element(),
            _ => row.on_click(cx.listener(move |v, _, _, cx| v.set_rule(&flip, cx))).into_any_element(),
        }
    }

    fn render_allowed(&self, t: &Theme, p: &ConnectionProfile, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = p.mcp_allowed.iter().enumerate().map(|(k, s)| {
            div()
                .h(px(26.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_1()
                .border_color(t.line)
                .text_size(px(11.5))
                .text_color(t.tx2)
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(s.clone()))
                .child(ui::icon_button(t, ("agents-allowed-rm", k), Icon::Close, 20., 10.).on_click(cx.listener(move |v, _, _, cx| {
                    if let Some(id) = v.conn.clone() {
                        v.edit_conn(&id, |p| {
                            if k < p.mcp_allowed.len() {
                                p.mcp_allowed.remove(k);
                            }
                        }, cx);
                    }
                })))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(ui::caps(t, "ALLOWED WITHOUT ASKING"))
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(field(t, "agents-field-allowed", &self.allow_input, window, cx).flex_1())
                    .child(ui::button(t, "agents-allowed-add", ui::Btn::Secondary).h(px(28.)).child("+ Add").on_click(cx.listener(|v, _, _, cx| v.add_allowed(cx)))),
            )
            .children(self.allow_error.clone().map(|e| div().text_size(px(11.)).text_color(t.err).child(e)))
            .into_any_element()
    }

    pub(crate) fn add_allowed(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx) else { return };
        let text = self.allow_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let d = p.engine.dialect();
        let entry = dataman_core::guard::allowlist_entry(&text, d)
            .and_then(|e| if p.mcp_allowed.iter().any(|a| dataman_core::guard::allowlist_match(a, &e, d)) { Err("already allowed".to_string()) } else { Ok(e) });
        match entry {
            Ok(e) => {
                self.edit_conn(&p.id, |p| p.mcp_allowed.push(e), cx);
                self.allow_input.update(cx, |i, cx| i.set_text("", cx));
                self.allow_error = None;
            }
            Err(e) => self.allow_error = Some(e),
        }
        cx.notify();
    }

    fn render_yolo(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let on = p.mcp_yolo;
        let confirming = self.yolo_confirm;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("agents-yolo")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(div().flex_1().flex().flex_col().gap(px(3.)).child(div().text_color(if on { t.err } else { t.tx }).child("YOLO")).child(div().text_size(px(11.)).text_color(t.tx3).child("Writes run without asking.")))
                    .child(crate::agents::switch(t, on).w(px(30.)))
                    .on_click(cx.listener(|v, _, _, cx| {
                        let Some(p) = v.selected_profile(cx) else { return };
                        // On production YOLO takes a second, explicit yes.
                        if !p.mcp_yolo && p.env.is_production() && !v.yolo_confirm {
                            v.yolo_confirm = true;
                            cx.notify();
                            return;
                        }
                        v.yolo_confirm = false;
                        v.edit_conn(&p.id, |p| p.mcp_yolo = !p.mcp_yolo, cx);
                    })),
            )
            .when(confirming, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .text_color(t.err)
                        .child(div().flex_1().child("Production: agents would write here without anyone asking."))
                        .child(ui::button(t, "agents-yolo-yes", ui::Btn::Danger).h(px(24.)).child("Turn on").on_click(cx.listener(|v, _, _, cx| {
                            v.yolo_confirm = false;
                            if let Some(id) = v.conn.clone() {
                                v.edit_conn(&id, |p| p.mcp_yolo = true, cx);
                            }
                        }))),
                )
            })
            .into_any_element()
    }

    // ---- Privacy ----

    pub(crate) fn render_privacy(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = self.selected_profile(cx) else { return self.no_conns(t, "Privacy") };
        let r = p.mcp_rules.clone();
        let names = Names::of(p.engine);
        let place = placeholder(p.engine);
        if self.vis_input.read(cx).text().is_empty() {
            self.vis_input.update(cx, |i, cx| i.set_placeholder(place, cx));
        }
        let sens_place = if p.engine.is_sql() { "table.column or *pattern*" } else { "collection.field or *pattern*" };
        self.sens_input.update(cx, |i, cx| i.set_placeholder(sens_place, cx));
        let views: Vec<(AgentView, &str)> = AgentView::ALL.iter().map(|v| (*v, v.label())).collect();
        let sees = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-sees-tip",
                "THE AGENT SEES",
                Tip::new("What the agent sees")
                    .lead("How much of this connection's data reaches an agent. Your own tabs always show everything.")
                    .item("Data", "Rows as stored. Sensitive data types (cards, CPF, keys…) are still masked inside any value.")
                    .item("Masked data", "Rows come back with the sensitive columns below masked. The connection's host, port and user aren't shown to agents.")
                    .item("Structure only", "Tables, columns, types and query plans. No rows are read or written; only the structure changes Permissions allow.")
                    .note("What Visible to agents hides stays hidden in every mode."),
            ))
            .child(seg(t, "agents-view", &views, r.view, false, cx, |v, view, cx| v.set_rule(|r| r.view = view, cx)));
        let sens_rows = r.sensitive.iter().enumerate().map(|(k, s)| {
            let id = SharedString::from(format!("agents-sens-mask-{k}"));
            let items: Vec<(SharedString, bool, Act)> = Mask::ALL.iter().map(|m| (m.label().into(), *m == s.how, Act::SensMask(k, *m))).collect();
            let open = id.clone();
            let hov = t.hov;
            let rid = SharedString::from(format!("agents-sens-{k}"));
            div()
                .id(rid.clone())
                .group(rid.clone())
                .h(px(32.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .border_b_1()
                .border_color(t.line)
                .child(
                    div()
                        .children(ui::here(&SharedString::from(format!("agents-sens-check-{k}")).into()))
                        .id(SharedString::from(format!("agents-sens-check-{k}")))
                        .child(checkbox(t, s.on, false))
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.set_rule(|r| {
                                if let Some(s) = r.sensitive.get_mut(k) {
                                    s.on = !s.on;
                                }
                            }, cx)
                        })),
                )
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if s.on { t.tx } else { t.tx3 }).child(s.col.clone()))
                .child(
                    div()
                        .id(SharedString::from(format!("agents-sens-rm-{k}")))
                        .invisible()
                        .group_hover(rid, |s| s.visible())
                        .px(px(4.))
                        .text_color(t.tx3)
                        .hover(move |s| s.bg(hov))
                        .child("×")
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.set_rule(|r| {
                                if k < r.sensitive.len() {
                                    r.sensitive.remove(k);
                                }
                            }, cx)
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("agents-sens-mask-btn-{k}")))
                        .child(dropdown(t, self, id, s.how.label(), s.on))
                        .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.open_menu(open.clone(), 120., items.clone(), cx))),
                )
        });
        let sensitive = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().flex_1().child(label_tip(
                        t,
                        "agents-sens-tip",
                        "SENSITIVE COLUMNS",
                        Tip::new("Sensitive columns")
                            .lead("Columns whose values are masked when the agent sees Masked data.")
                            .item("Hidden", "The value becomes ‹hidden›.")
                            .item("Partial", "Keeps the end: a•••@acme.com, •••• 4242.")
                            .item("Hash", "A stable code, ‹h:7Q2KX9RM4A›. The same value always gives the same code, so an agent can still match rows.")
                            .lead("Agents can select these columns as they are and filter on them in WHERE; they can't rename, group, sort or compute with them.")
                            .example("users.email · *cpf* (any column with cpf in its name)")
                            .note("Scan again suggests columns by their names. Check what it adds."),
                    )))
                    .child(
                        div()
                            .children(ui::here(&"agents-scan".into()))
                            .id("agents-scan")
                            .text_size(px(11.5))
                            .text_color(t.tx2)
                            .hover(|s| s.opacity(0.75))
                            .child("Scan again")
                            .on_click(cx.listener(|v, _, _, cx| v.scan(cx))),
                    ),
            )
            .children(self.scan_note.clone().map(|n| div().text_size(px(11.)).text_color(t.tx3).child(n)))
            .child(div().flex().flex_col().children(sens_rows))
            .child(
                div()
                    .mt(px(4.))
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(self.suggest_field(t, Which::Sensitive, field(t, "agents-field-sensitive", &self.sens_input, window, cx), window, cx))
                    .child(ui::button(t, "agents-sens-add", ui::Btn::Secondary).h(px(28.)).px(px(10.)).child("Add").on_click(cx.listener(|v, _, _, cx| v.add_sensitive(cx)))),
            );
        let left = div().flex_1().min_w_0().flex().flex_col().gap(px(24.)).child(sees).child(sensitive);
        let modes: Vec<(VisibleMode, &str)> = [VisibleMode::Except, VisibleMode::Only].iter().map(|m| (*m, m.label())).collect();
        let typed = self.vis_input.read(cx).text().trim().to_string();
        let kind = Visible::kind_of(&typed, &self.schemas_of(&p, cx), names);
        let verb = if r.visible.mode == VisibleMode::Only && kind != ItemKind::Column { "Show" } else { "Hide" };
        let items = r.visible.items.iter().enumerate().map(|(k, it)| {
            div()
                .h(px(32.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .border_b_1()
                .border_color(t.line)
                .child(icon(if it.kind == ItemKind::Column { Icon::Column } else { Icon::GridTable }).size(px(14.)).text_color(t.tx3))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(it.name.clone()))
                .child(div().text_size(px(11.)).text_color(t.tx3).child(it.kind.label()))
                .child(ui::icon_button(t, SharedString::from(format!("agents-vis-rm-{k}")), Icon::Close, 20., 10.).on_click(cx.listener(move |v, _, _, cx| {
                    v.set_rule(|r| {
                        if k < r.visible.items.len() {
                            r.visible.items.remove(k);
                        }
                    }, cx)
                })))
        }).collect::<Vec<_>>();
        let right = div()
            .w(px(360.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(label_tip(
                        t,
                        "agents-vis-tip",
                        "VISIBLE TO AGENTS",
                        Tip::new("Visible to agents")
                            .lead("What exists for agents on this connection, in every mode.")
                            .item("Everything except", "The list holds what is hidden. Tables created later are visible.")
                            .item("Only these", "The list holds the tables and schemas agents can see. Everything else is hidden, tables created later too.")
                            .lead("Hidden tables and schemas aren't listed, described or queryable. Hidden columns are removed: Dataman narrows each query so, for agents, they don't exist.")
                            .example("public.payments (table) · audit.* (schema) · users.password_hash (column)")
                            .note("Functions, copies of the data in other tables and statements Dataman can't read are refused or not covered. Only database permissions block everything."),
                    ))
                    .child(seg(t, "agents-vis-mode", &modes, r.visible.mode, false, cx, |v, m, cx| v.set_rule(|r| r.visible.mode = m, cx)))
                    .child(div().text_size(px(11.)).text_color(t.tx3).child("Left-out tables and columns are not listed, described or queryable.")),
            )
            .child(
                div().flex().flex_col().children(items).child(
                    div()
                        .mt(px(12.))
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .child(self.suggest_field(t, Which::Visible, field(t, "agents-field-visible", &self.vis_input, window, cx), window, cx))
                        .child(ui::button(t, "agents-vis-add", ui::Btn::Secondary).h(px(28.)).px(px(10.)).child(verb).on_click(cx.listener(|v, _, _, cx| v.add_visible(cx)))),
                ),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "Privacy"))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .border_t_1()
                    .border_color(t.line)
                    .child(self.render_conn_list(t, cx))
                    .child(div().id("agents-priv-body").flex_1().min_w_0().overflow_y_scroll().flex().flex_row().gap(px(40.)).px(px(32.)).py(px(24.)).child(left).child(right)),
            )
            .into_any_element()
    }

    /// The schemas (or MongoDB databases) known for a connection: from the schema cache when it
    /// is connected, else its own database.
    fn schemas_of(&self, p: &ConnectionProfile, cx: &mut Context<Self>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(c) = self.store.read(cx).connection(&p.id) {
            let db = c.database().to_string();
            out.extend(c.schema.cached_schemas(&db).unwrap_or_default());
            out.extend(c.schema.cached_databases().unwrap_or_default());
        }
        if !p.database.is_empty() {
            out.push(p.database.clone());
        }
        // The default schemas count even before the explorer has read them.
        match p.engine {
            Engine::Postgres => out.push("public".into()),
            Engine::SqlServer => out.push("dbo".into()),
            _ => {}
        }
        out
    }

    pub(crate) fn add_sensitive(&mut self, cx: &mut Context<Self>) {
        let text = self.sens_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let how = privacy::sensitive_name(text.rsplit('.').next().unwrap_or(&text).trim_matches('*')).unwrap_or(Mask::Hidden);
        self.set_rule(|r| {
            if !r.sensitive.iter().any(|s| s.col.eq_ignore_ascii_case(&text)) {
                r.sensitive.push(SensitiveCol { col: text, how, on: true });
            }
        }, cx);
        self.sens_input.update(cx, |i, cx| i.set_text("", cx));
    }

    pub(crate) fn add_visible(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx) else { return };
        let text = self.vis_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let kind = Visible::kind_of(&text, &self.schemas_of(&p, cx), Names::of(p.engine));
        self.set_rule(|r| {
            if !r.visible.items.iter().any(|i| i.name.eq_ignore_ascii_case(&text)) {
                r.visible.items.push(VisibleItem { name: text, kind });
            }
        }, cx);
        self.vis_input.update(cx, |i, cx| i.set_text("", cx));
    }

    /// Suggests sensitive columns by name from the schema of the connected database (on your
    /// connection, never an agent's).
    pub(crate) fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx) else { return };
        if !p.engine.is_sql() {
            self.scan_note = Some("Scan reads SQL schemas; add fields by name here.".into());
            cx.notify();
            return;
        }
        let Some(conn) = self.store.read(cx).connection(&p.id) else {
            self.scan_note = Some(format!("Connect to {} to scan it.", p.name));
            cx.notify();
            return;
        };
        self.scan_note = Some("Scanning…".into());
        cx.notify();
        let id = p.id.clone();
        cx.spawn(async move |this, cx| {
            let db = conn.database().to_string();
            let _ = conn.preload(&db).await;
            let snap = conn.schema.snapshot(&db);
            let mut found: Vec<(String, Mask)> = Vec::new();
            for (rel, cols) in &snap.columns {
                let Some(r) = snap.relations.get(*rel) else { continue };
                if r.kind.is_view() {
                    continue;
                }
                for c in cols {
                    if let Some(m) = privacy::sensitive_name(&c.name) {
                        found.push((format!("{}.{}", r.name, c.name), m));
                    }
                }
            }
            found.sort();
            let _ = this.update(cx, |v, cx| {
                let mut added = 0;
                v.edit_conn(&id, |p| {
                    for (col, how) in found.iter() {
                        if !p.mcp_rules.sensitive.iter().any(|s| s.col.eq_ignore_ascii_case(col)) {
                            p.mcp_rules.sensitive.push(SensitiveCol { col: col.clone(), how: *how, on: true });
                            added += 1;
                        }
                    }
                }, cx);
                v.scan_note = Some(match (found.len(), added) {
                    (0, _) => "No column looks sensitive by its name.".into(),
                    (_, 0) => "Nothing new found.".into(),
                    (_, 1) => "1 column added.".into(),
                    (_, n) => format!("{n} columns added."),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
