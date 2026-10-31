//! Protection's agent login: a database login of the agents' own on each connection. Without one,
//! a card above Access shows how agents reach the database and its Create agent login opens the
//! review (what can't be granted, the exact SQL, the options); the login is created with the
//! user's login, checked by logging in as it, and only then kept: its password in the Keychain,
//! the rest on the connection. Then the page ends on an Agent login section instead: when the
//! rules change it drifts (the changes are shown and applied on request); Verify, Rotate password
//! and Remove login (only what Dataman made, after confirmation) are there too.

use std::cell::RefCell;
use std::sync::Arc;

use chrono::Utc;
use dataman_core::agent_login::{self, Manual, Options, Plan, live};
use dataman_core::{Connection, ConnectionProfile, Engine, McpAccess};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, Hsla, SharedString, StyledText, TextRun, div, prelude::*};

use crate::agents::{AgentsPage, Page, Tip, checkbox, qmark};
use crate::agents_rules::{BODY_W, Jump, sub, sub_title};
use crate::theme::{Theme, mix, mono, px};
use crate::ui;

/// The section's state for the selected connection.
#[derive(Default)]
pub(crate) struct LoginUi {
    /// The connection the rest is about.
    pub(crate) conn: Option<String>,
    pub(crate) busy: Option<String>,
    /// The last action's outcome: good or not, and what to say.
    pub(crate) note: Option<(bool, String)>,
    pub(crate) checks: Vec<live::Check>,
    pub(crate) drift: Option<Result<live::Drift, String>>,
    /// The rules (their hash) the drift was read for: read again when they change.
    drift_rules: Option<String>,
    pub(crate) sheet: Option<Sheet>,
    /// The sheet's WHAT RUNS, scrolled both ways.
    pub(crate) sql_scroll: gpui::ScrollHandle,
    /// WHAT RUNS as last laid out and coloured.
    what: RefCell<Option<WhatRuns>>,
}

/// WHAT RUNS's lines, made again only when the plan, its engine or the theme changes: a script
/// can run to hundreds of lines, too many to lay out and colour on every frame.
struct WhatRuns {
    key: (String, Engine, [Hsla; 3]),
    lines: Vec<(SharedString, Vec<TextRun>)>,
    /// The longest line, always laid out so the code is as wide as it, wherever it's scrolled.
    widest: usize,
}

pub(crate) enum Sheet {
    Create { review: Option<Result<live::Review, String>>, opts: Options, confirm: bool, busy: bool, error: Option<String> },
    Remove { plan: Option<Result<Plan, String>>, confirm: bool, busy: bool, error: Option<String> },
    /// The drift: what changes on the login, and what runs for it.
    Changes,
}

/// The user's own connection: the open one, else one opened with what Dataman keeps.
async fn admin_of(core: Arc<dataman_core::Core>, open: Option<Arc<Connection>>, p: ConnectionProfile) -> Result<Arc<Connection>, String> {
    if let Some(c) = open {
        return Ok(c);
    }
    core.connect(ConnectionProfile { agent_login: None, ..p }, None).await.map_err(|e| format!("cannot connect as you: {e:#}"))
}

pub(crate) fn login_tip() -> Tip {
    Tip::new("An agent login")
        .lead("A database login of the agents' own, made from this connection's rules (Access and Privacy), so the database itself refuses what the rules refuse, even if one of Dataman's checks had a gap.")
        .item("Password", format!("{} random characters, kept in the {}. Dataman and its MCP connect as the login on their own: nothing to paste anywhere.", agent_login::hash::PASSWORD_LEN, dataman_core::secret::STORE_NAME))
        .item("Your tabs", "keep using your own login.")
        .note("It is a ceiling, not a copy: row limits, the real-WHERE rule, approvals and value masking stay in Dataman. Statements that wait for your Allow (DDL, TRUNCATE) run on your login once you click it.")
}

impl AgentsPage {
    fn login_open_conn(&self, id: &str, cx: &Context<Self>) -> Option<Arc<Connection>> {
        self.store.read(cx).connection(id)
    }

    /// Runs `f` on the core's runtime with the user's own connection, then `done` on the page.
    fn login_task<T: Send + 'static>(
        &mut self,
        p: &ConnectionProfile,
        cx: &mut Context<Self>,
        f: impl FnOnce(Arc<dataman_core::Core>, Arc<Connection>, ConnectionProfile) -> futures_util::future::BoxFuture<'static, Result<T, String>> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) {
        let (core, open, p) = (self.core.clone(), self.login_open_conn(&p.id, cx), p.clone());
        let h = core.handle();
        let fut = async move {
            let admin = admin_of(core.clone(), open, p.clone()).await?;
            f(core, admin, p).await
        };
        cx.spawn(async move |this, cx| {
            let r = h.spawn(fut).await.map_err(|e| e.to_string()).and_then(|r| r);
            let _ = this.update(cx, |v, cx| {
                done(v, r, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Keeps the section about `p`: a fresh state for another connection, and the drift read
    /// again when its rules change. Browsing connections opens none: what the server has now (a
    /// table made since, a grant added by hand) is read where Dataman is connected already, or
    /// where the rules changed or the password is due.
    pub(crate) fn login_sync(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let fresh = self.login.conn.as_deref() != Some(p.id.as_str());
        if fresh {
            self.login = LoginUi { conn: Some(p.id.clone()), ..Default::default() };
        }
        // A PostgreSQL-protocol server Dataman is connected to already: asked once whether it has
        // logins at all (MotherDuck is known by its host; another, by its review).
        if fresh && p.engine == Engine::Postgres && p.agent_login.is_none() && !self.no_logins.contains(&p.id) && !agent_login::pg::motherduck(p) && self.login_open_conn(&p.id, cx).is_some() {
            let id = p.id.clone();
            self.login_task(p, cx, |_, admin, p| Box::pin(async move { live::has_logins(&admin, &p).await.map_err(|e| format!("{e:#}")) }), move |v, r, _| match r {
                Ok(false) => {
                    v.no_logins.insert(id);
                }
                Ok(true) => {}
                Err(e) => log::debug!("couldn't ask the server whether it has logins: {e}"),
            });
        }
        let Some(l) = p.agent_login.as_ref().filter(|_| agent_login::supported(p.engine)) else { return };
        let now = agent_login::rules_hash(p, &l.database);
        let changed = now != l.rules;
        let due = l.rotation_due(Utc::now()).is_some_and(|d| d <= 0);
        let read = if fresh { changed || due || self.login_open_conn(&p.id, cx).is_some() } else { changed && self.login.drift_rules.as_deref() != Some(now.as_str()) };
        if read {
            self.login.drift_rules = Some(now);
            self.login_drift(p, cx);
        }
        if fresh && due {
            self.login_rotate(p, cx);
        }
    }

    fn login_drift(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(l) = p.agent_login.clone() else { return };
        self.login.drift = None;
        self.login_task(
            p,
            cx,
            move |core, admin, p| {
                Box::pin(async move {
                    let pw = live::password_of(&*core.secrets, &p.id, &l.user).map_err(|e| format!("{e:#}"))?;
                    live::drift(&admin, &p, &l, &pw).await.map_err(|e| format!("{e:#}"))
                })
            },
            |v, r, _| {
                // The rules unchanged and the server not answering: said, not shown as drift.
                if let Err(e) = &r {
                    v.login.note = Some((false, format!("Couldn't compare the login with the server: {e}")));
                }
                v.login.drift = Some(r);
            },
        );
    }

    /// Logins whose password is due for rotation (the option in the review), rotated in the
    /// background when Dataman can reach the server with what it keeps; the others wait for
    /// Permissions, which says they are due.
    pub(crate) fn rotate_due(&mut self, cx: &mut Context<Self>) {
        let due: Vec<ConnectionProfile> = self.profiles(cx).into_iter().filter(|p| p.agent_login.as_ref().and_then(|l| l.rotation_due(Utc::now())).is_some_and(|d| d <= 0)).collect();
        for p in due {
            let Some(l) = p.agent_login.clone() else { continue };
            let id = p.id.clone();
            self.login_task(
                &p,
                cx,
                move |core, admin, p| {
                    Box::pin(async move {
                        let old = live::password_of(&*core.secrets, &p.id, &l.user).unwrap_or_default();
                        let new = agent_login::hash::password();
                        let plan = live::rotation(&admin, &p, &l, &old, &new).await.map_err(|e| format!("{e:#}"))?;
                        live::apply(&admin, &plan, &l.database).await.map_err(|e| format!("{e:#}"))?;
                        core.secrets.set(&agent_login::secret_account(&p.id), &new).map_err(|e| format!("{e:#}"))?;
                        // What logged in with the old password goes too.
                        if let Err(e) = live::end_sessions(&admin, &p, &l.user).await {
                            log::info!("the agent login's sessions from before its new password could not be ended: {e:#}");
                        }
                        Ok(())
                    })
                },
                move |v, r, cx| match r {
                    Ok(()) => v.edit_conn(&id, |p| {
                        if let Some(l) = p.agent_login.as_mut() {
                            l.rotated = Some(Utc::now());
                        }
                    }, cx),
                    Err(e) => log::info!("the agent login's password is due for rotation and could not be rotated now: {e}"),
                },
            );
        }
    }

    fn login_open_create(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let opts = Options::default();
        self.login.sheet = Some(Sheet::Create { review: None, opts, confirm: false, busy: false, error: None });
        self.login.sql_scroll.set_offset(gpui::Point::default());
        self.login_task(
            p,
            cx,
            move |_, admin, p| Box::pin(async move { live::review(&admin, &p, &opts).await.map_err(|e| format!("{e:#}")) }),
            |v, r, _| {
                // No logins on this server after all: the section says so, the sheet goes.
                if let Ok(rv) = &r
                    && rv.probe.manual == Some(Manual::NoLogins)
                {
                    v.no_logins.extend(v.login.conn.clone());
                    v.login.sheet = None;
                    return;
                }
                if let Some(Sheet::Create { review, .. }) = v.login.sheet.as_mut() {
                    *review = Some(r);
                }
            },
        );
    }

    /// An option flipped: the plan made again for it (same password, same server).
    fn login_option(&mut self, p: &ConnectionProfile, f: impl FnOnce(&mut Options), cx: &mut Context<Self>) {
        let Some(Sheet::Create { review: Some(Ok(rv)), opts, confirm, .. }) = self.login.sheet.as_mut() else { return };
        f(opts);
        *confirm = false;
        rv.spec = agent_login::compile(p, "", opts);
        // What the probe found (a login already there, what the user's login lacks) stays.
        let (lacks, notes) = (std::mem::take(&mut rv.plan.lacks), std::mem::take(&mut rv.plan.notes));
        rv.plan = live::create_plan(&rv.spec, &rv.inventory, &rv.probe, rv.persist, &rv.password, p.env.is_production());
        (rv.plan.lacks, rv.plan.notes) = (lacks, notes);
        cx.notify();
    }

    fn login_create(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(Sheet::Create { review: Some(Ok(rv)), opts, confirm, busy, error }) = self.login.sheet.as_mut() else { return };
        if *busy || !rv.plan.lacks.is_empty() {
            return;
        }
        // Production: the usual second, explicit yes.
        if p.env.is_production() && !*confirm {
            *confirm = true;
            cx.notify();
            return;
        }
        *busy = true;
        *error = None;
        let (rv, opts) = (rv.clone(), *opts);
        self.login_task(
            p,
            cx,
            move |core, admin, p| {
                Box::pin(async move { live::create(&core, &admin, &p, &rv, &opts).await })
            },
            |v, r, cx| match r {
                Ok((rec, checks, warn)) => {
                    let id = v.login.conn.clone().unwrap_or_default();
                    let user = rec.user.clone();
                    v.edit_conn(&id, |p| {
                        p.agent_login = Some(rec);
                        p.agents_refused = false;
                    }, cx);
                    v.login.sheet = None;
                    v.login.checks = checks;
                    v.login.note = Some(match warn {
                        Some(w) => (false, w),
                        None => (true, format!("{user} created and checked by logging in as it; agents connect as it from their next call")),
                    });
                }
                Err(e) => {
                    if let Some(Sheet::Create { busy, error, .. }) = v.login.sheet.as_mut() {
                        *busy = false;
                        *error = Some(e);
                    }
                }
            },
        );
    }

    fn login_verify(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(user) = p.agent_login.as_ref().map(|l| l.user.clone()) else { return };
        self.login.busy = Some("Verifying…".into());
        self.login.note = None;
        self.login_task(
            p,
            cx,
            move |core, admin, p| {
                Box::pin(async move {
                    match live::exists(&admin, &p, &user).await.map_err(|e| format!("{e:#}"))? {
                        // Dropped on the server since: the record goes.
                        None => return Ok(Verified::Gone),
                        Some(false) => return Err(format!("{user} on the server isn't marked as Dataman's: Dataman leaves it alone")),
                        Some(true) => {}
                    }
                    let pw = live::password_of(&*core.secrets, &p.id, &user).map_err(|e| format!("{e:#}"))?;
                    let rv = live::review(&admin, &p, &Options::default()).await.map_err(|e| format!("{e:#}"))?;
                    Ok(Verified::Checked(live::verify_as(&core, &p, &user, &pw, &rv).await?))
                })
            },
            |v, r, cx| {
                v.login.busy = None;
                let id = v.login.conn.clone().unwrap_or_default();
                match r {
                    Ok(Verified::Gone) => {
                        let _ = v.core.secrets.delete(&agent_login::secret_account(&id));
                        v.edit_conn(&id, |p| {
                            p.agent_login = None;
                            p.agents_refused = true;
                        }, cx);
                        v.login.note = Some((true, "The login is gone from the server: Dataman forgot it. Agents are refused here until you choose.".into()));
                    }
                    Ok(Verified::Checked(checks)) => {
                        let ok = checks.iter().all(|c| c.ok);
                        if ok {
                            v.edit_conn(&id, |p| {
                                if let Some(l) = p.agent_login.as_mut() {
                                    l.verified = Some(Utc::now());
                                }
                            }, cx);
                        }
                        v.login.checks = checks;
                        v.login.note = Some((ok, if ok { "Verified by logging in as it".into() } else { "Logging in as it did not go as the rules say".into() }));
                    }
                    Err(e) => v.login.note = Some((false, e)),
                }
            },
        );
    }

    fn login_rotate(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(l) = p.agent_login.clone() else { return };
        self.login.busy = Some("A new password…".into());
        self.login.note = None;
        self.login_task(
            p,
            cx,
            move |core, admin, p| {
                Box::pin(async move {
                    let old = live::password_of(&*core.secrets, &p.id, &l.user).unwrap_or_default();
                    let new = agent_login::hash::password();
                    let plan = live::rotation(&admin, &p, &l, &old, &new).await.map_err(|e| format!("{e:#}"))?;
                    live::apply(&admin, &plan, &l.database).await.map_err(|e| format!("{e:#}"))?;
                    // The server first, then the Keychain: the old one stays until this works.
                    core.secrets.set(&agent_login::secret_account(&p.id), &new).map_err(|e| format!("the new password is set on the server but could not be kept in the {}: {e:#}; Rotate again", dataman_core::secret::STORE_NAME))?;
                    // Sessions opened with the old password would go on working: ended.
                    Ok(live::end_sessions(&admin, &p, &l.user).await.err().map(|e| format!("{e:#}")))
                })
            },
            |v, r, cx| {
                v.login.busy = None;
                match r {
                    Ok(left) => {
                        let id = v.login.conn.clone().unwrap_or_default();
                        v.edit_conn(&id, |p| {
                            if let Some(l) = p.agent_login.as_mut() {
                                l.rotated = Some(Utc::now());
                            }
                        }, cx);
                        v.login.note = Some(match left {
                            None => (true, "New password set; agents reconnect with it".into()),
                            Some(e) => (false, format!("New password set, but the sessions opened with the old one could not be ended ({e}): they last until they disconnect")),
                        });
                    }
                    Err(e) => v.login.note = Some((false, e)),
                }
            },
        );
    }

    fn login_apply_drift(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(Ok(d)) = self.login.drift.clone() else { return };
        let Some(l) = p.agent_login.clone() else { return };
        self.login.busy = Some("Applying…".into());
        self.login_task(
            p,
            cx,
            move |_, admin, _| Box::pin(async move { live::apply(&admin, &d.plan, &l.database).await.map(|_| d.plan).map_err(|e| format!("{e:#}")) }),
            |v, r, cx| {
                v.login.busy = None;
                match r {
                    Ok(plan) => {
                        let id = v.login.conn.clone().unwrap_or_default();
                        v.edit_conn(&id, |p| {
                            let rules = agent_login::rules_hash(p, &p.agent_login.as_ref().map(|l| l.database.clone()).unwrap_or_default());
                            if let Some(l) = p.agent_login.as_mut() {
                                (l.rules, l.plan, l.synced, l.views, l.granted, l.not_granted) = (rules, plan.hash.clone(), Utc::now(), plan.views.clone(), plan.granted, plan.not_granted.len());
                                for m in &plan.made {
                                    if !l.made.contains(m) {
                                        l.made.push(m.clone());
                                    }
                                }
                            }
                        }, cx);
                        v.login.drift = None;
                        v.login.note = Some((true, "The login follows the rules again".into()));
                    }
                    Err(e) => v.login.note = Some((false, e)),
                }
            },
        );
    }

    fn login_open_remove(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(l) = p.agent_login.clone() else { return };
        self.login.sheet = Some(Sheet::Remove { plan: None, confirm: false, busy: false, error: None });
        self.login.sql_scroll.set_offset(gpui::Point::default());
        self.login_task(
            p,
            cx,
            move |_, admin, p| Box::pin(async move { live::removal(&admin, &p, &l).await.map_err(|e| format!("{e:#}")) }),
            |v, r, _| {
                if let Some(Sheet::Remove { plan, .. }) = v.login.sheet.as_mut() {
                    *plan = Some(r);
                }
            },
        );
    }

    fn login_remove(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(Sheet::Remove { plan: Some(Ok(plan)), confirm, busy, error }) = self.login.sheet.as_mut() else { return };
        if *busy {
            return;
        }
        if p.env.is_production() && !*confirm {
            *confirm = true;
            cx.notify();
            return;
        }
        *busy = true;
        *error = None;
        let plan = plan.clone();
        let db = live::database_for(p, p.agent_login.as_ref());
        let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_default();
        self.login_task(
            p,
            cx,
            move |_, admin, p| {
                Box::pin(async move {
                    // Its open sessions first: one would outlive the login otherwise.
                    let _ = live::end_sessions(&admin, &p, &user).await;
                    live::apply(&admin, &plan, &db).await.map_err(|e| format!("{e:#}"))
                })
            },
            |v, r, cx| match r {
                Ok(()) => v.login_forget(cx),
                Err(e) => {
                    if let Some(Sheet::Remove { busy, error, .. }) = v.login.sheet.as_mut() {
                        *busy = false;
                        *error = Some(e);
                    }
                }
            },
        );
    }

    /// Dataman forgets the login: its record and its Keychain password. Agents are refused here
    /// until a new login exists or the user lets them use theirs.
    pub(crate) fn login_forget(&mut self, cx: &mut Context<Self>) {
        let id = self.login.conn.clone().unwrap_or_default();
        let _ = self.core.secrets.delete(&agent_login::secret_account(&id));
        self.edit_conn(&id, |p| {
            p.agent_login = None;
            p.agents_refused = true;
        }, cx);
        self.login.sheet = None;
        self.login.checks.clear();
        self.login.drift = None;
        self.login.note = Some((true, "The agent login is gone from Dataman. Agents are refused on this connection until you create a new one or let them use your login.".into()));
        cx.notify();
    }

    fn login_let_mine(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        self.edit_conn(&p.id, |p| p.agents_refused = false, cx);
        self.login.note = Some((true, "Agents use your login here again, within Dataman's rules".into()));
    }

    /// Protection on a connection, at its agent login (deep links to it).
    pub(crate) fn show_login(&mut self, id: &str, cx: &mut Context<Self>) {
        self.conn = Some(id.to_string());
        self.show(Page::Protection, cx);
        self.jump = Some(Jump::Login);
    }

    // ---- Render ----

    /// Above Access while agents use the user's own login: how they reach the database, and the
    /// way to give them one of their own (or why there is none to give).
    pub(crate) fn login_card(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let supported = agent_login::supported(p.engine);
        if p.agent_login.is_some() && supported {
            return None;
        }
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .child(ui::caps(t, format!("HOW AGENTS REACH {}", p.name.to_uppercase())).text_size(px(11.)).text_color(t.tx2))
            .child(qmark(t, "agents-login-tip", login_tip()));
        let mut card = div()
            .children(ui::here(&"agents-login-section".into()))
            .w_full()
            .max_w(px(BODY_W))
            .flex()
            .flex_col()
            .gap(px(16.))
            .px(px(22.))
            .py(px(20.))
            .bg(t.alt)
            .border_1()
            .border_color(t.line)
            .rounded(px(2.))
            .line_height(px(18.))
            .child(head);
        let mine = if p.user.is_empty() { "your login".to_string() } else { p.user.clone() };
        let create = |cx: &mut Context<Self>| {
            let pid = p.clone();
            ui::button(t, "agents-login-create", ui::Btn::Primary).h(px(32.)).px(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Create agent login").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_create(&pid, cx)))
        };
        if !supported {
            card = card.child(div().text_color(t.tx2).child(agent_login::FILE_NOTE));
        } else if agent_login::pg::motherduck(p) || self.no_logins.contains(&p.id) {
            card = card.child(
                div()
                    .children(ui::here(&"agents-login-pgwire".into()))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_color(t.tx2).child(agent_login::NO_LOGINS_NOTE))
                    .when(agent_login::pg::motherduck(p), |d| d.child(div().children(ui::here(&"agents-login-motherduck".into())).text_color(t.tx3).child(agent_login::MOTHERDUCK_NOTE))),
            );
        } else if p.agents_refused {
            let pid = p.clone();
            let let_mine = ui::button(t, "agents-login-mine", ui::Btn::Secondary).h(px(32.)).px(px(14.)).child("Let agents use my login").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_let_mine(&pid, cx)));
            card = card
                .child(div().text_color(t.err).child("Agents are refused here: its agent login was removed. Create a new one, or let agents use your login again (Dataman's rules still apply)."))
                .child(div().flex().flex_row().flex_wrap().gap(px(8.)).child(create(cx)).child(let_mine));
        } else {
            let (_, many) = crate::agents_rules::nouns(p.engine);
            let tier = match p.mcp_access() {
                McpAccess::Off => "no access",
                McpAccess::Read => "read-only",
                _ => "reads and writes, no structure changes",
            };
            // Atlas makes users only through its own UI, CLI or Admin API.
            let (text, button) = if agent_login::mongo::atlas(p) {
                ("On MongoDB Atlas, database users are made in Atlas itself, so Dataman can't create one here.".to_string(), None)
            } else {
                (format!("Give agents a login of their own and {} enforces these rules too: {tier}, hidden {many} not granted. Your tabs keep using {mine}.", p.engine.label()), Some(create(cx)))
            };
            card = card.child(self.login_chain(t, p, &mine)).child(div().pt(px(4.)).flex().flex_row().items_center().gap(px(20.)).child(div().flex_1().min_w_0().text_color(t.tx2).child(text)).children(button));
        }
        Some(card.children(self.login_notes(t)).into_any_element())
    }

    /// Agent → Dataman's rules → the database as the user's login: the last one filled, since
    /// the database itself refuses nothing to it.
    fn login_chain(&self, t: &Theme, p: &ConnectionProfile, mine: &str) -> gpui::Div {
        let node = |title: String, hint: String, fill: Option<Hsla>| {
            let (bg, line, tx, tx2) = match fill {
                Some(f) => (f, f, t.on_acc, mix(t.on_acc, f, 0.12)),
                None => (t.field, t.line2, t.tx, t.tx3),
            };
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(14.))
                .py(px(10.))
                .bg(bg)
                .border_1()
                .border_color(line)
                .rounded(px(2.))
                .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().font_weight(FontWeight::SEMIBOLD).text_color(tx).child(title))
                .child(div().text_size(px(11.)).line_height(px(17.)).text_color(tx2).child(hint))
        };
        // Nodes as tall as the tallest (stretched), each arrow centred on them.
        let arrow = || div().flex_none().flex().items_center().child(div().w(px(28.)).h(px(1.)).bg(t.tx4));
        // A mid grey: filled, apart from the two nodes Dataman holds.
        let fill = if t.is_dark() { mix(t.tx4, t.tx3, 0.15) } else { mix(t.tx3, t.tx4, 0.2) };
        let rules = format!("{} · {}", p.mcp_access().label(), p.mcp_rules.view.label().to_lowercase());
        div()
            .children(ui::here(&"agents-login-chain".into()))
            .flex()
            .flex_row()
            .child(node("Agent".into(), "over MCP".into(), None).flex_none())
            .child(arrow())
            .child(node("Dataman's rules".into(), rules, None).flex_1())
            .child(arrow())
            .child(node(format!("{} as {mine}", p.engine.label()), "your login: the database refuses nothing".into(), Some(fill)).flex_1())
    }

    /// The last action's outcome and, after a check, each of its lines.
    fn login_notes(&self, t: &Theme) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some((ok, text)) = &self.login.note {
            out.push(div().children(ui::here(&"agents-login-note".into())).text_size(px(11.5)).text_color(if *ok { t.tx2 } else { t.err }).line_height(px(18.)).child(text.clone()).into_any_element());
        }
        if !self.login.checks.is_empty() {
            out.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(px(11.5))
                    .children(self.login.checks.iter().map(|c| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .min_w_0()
                            .child(div().flex_none().w(px(12.)).text_color(if c.ok { t.ok } else { t.err }).child(if c.ok { "✓" } else { "✕" }))
                            .child(div().flex_none().text_color(t.tx2).child(c.what.clone()))
                            .when(!c.ok && !c.detail.is_empty(), |d| d.child(div().min_w_0().text_color(t.tx3).overflow_hidden().whitespace_nowrap().text_ellipsis().child(c.detail.clone())))
                    }))
                    .into_any_element(),
            );
        }
        out
    }

    /// Where a login was removed, said under the tier: agents are refused until the user chooses.
    pub(crate) fn login_refused_note(&self, t: &Theme, p: &ConnectionProfile) -> Option<AnyElement> {
        (p.agents_refused && p.agent_login.is_none()).then(|| div().children(ui::here(&"agents-login-refused".into())).text_size(px(11.)).text_color(t.err).child("Agents are refused here: its agent login was removed.").into_any_element())
    }

    /// Last on the page while agents connect as a login of their own: it, its password, and the
    /// way back to the user's login.
    pub(crate) fn login_section(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let l = p.agent_login.as_ref().filter(|_| agent_login::supported(p.engine))?;
        let drifted = l.rules != agent_login::rules_hash(p, &l.database) || self.login.drift.as_ref().is_some_and(|d| d.as_ref().is_ok_and(|d| !d.changes.is_empty()));
        let busy = self.login.busy.clone();
        let button = |id: &'static str, label: String| ui::button(t, id, ui::Btn::Secondary).h(px(30.)).px(px(14.)).child(label);
        let row = || div().min_h(px(30.)).flex().flex_row().items_center().gap(px(12.));
        let pv = p.clone();
        let verify = button("agents-login-verify", if busy.as_deref() == Some("Verifying…") { "Verifying…".into() } else { "Verify".into() }).ml_auto().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_verify(&pv, cx)));
        let (color, status) = match (&self.login.drift, drifted) {
            (_, false) => (t.ok, "In sync".to_string()),
            (Some(Ok(d)), true) if d.changes.len() == 1 => (t.warn, "1 change to apply".into()),
            (Some(Ok(d)), true) => (t.warn, format!("{} changes to apply", d.changes.len())),
            (_, true) => (t.warn, "The rules changed".into()),
        };
        let mut login = sub(sub_title(t, "agents-login-tip", "Login", login_tip())).child(
            row()
                .child(div().children(ui::here(&"agents-login-as".into())).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(l.user.clone()))
                .child(div().flex_none().flex().flex_row().items_center().gap(px(7.)).text_color(color).child(div().size(px(7.)).rounded_full().bg(color)).child(status))
                .child(verify),
        );
        if l.not_granted > 0 {
            login = login.child(div().text_size(px(11.5)).text_color(t.warn).child(format!("{} can't be granted to it: agents don't see {}.", agent_login_count(l.not_granted, "table"), if l.not_granted == 1 { "it" } else { "them" })));
        }
        if drifted {
            login = login.child(self.login_drift_lines(t, p, cx));
        }
        login = login.children(self.login_notes(t));
        let pr = p.clone();
        let rotate = button("agents-login-rotate", if busy.as_deref() == Some("A new password…") { "A new password…".into() } else { "Rotate password".into() }).ml_auto().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_rotate(&pr, cx)));
        let store = dataman_core::secret::STORE_NAME;
        let (due, kept) = match l.rotation_due(Utc::now()) {
            Some(d) if d <= 0 => (true, format!("In the {store} · rotation due")),
            Some(1) => (false, format!("In the {store} · rotates in 1 day")),
            Some(d) => (false, format!("In the {store} · rotates in {d} days")),
            None => (false, format!("In the {store}")),
        };
        let password = sub(sub_title(t, "agents-login-pw-tip", "Password", password_tip())).child(row().child(div().min_w_0().text_color(if due { t.warn } else { t.tx }).child(kept)).child(rotate));
        let pm = p.clone();
        let err = t.err;
        let remove = button("agents-login-remove", "Remove login…".into())
            .ml_auto()
            .bg(t.field)
            .border_color(err.opacity(0.35))
            .text_color(err)
            .hover(move |s| s.bg(err.opacity(0.08)))
            .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_remove(&pm, cx)));
        let back = if p.user.is_empty() { "Agents go back to your login.".to_string() } else { format!("Agents go back to your login, {}.", p.user) };
        let leave = div().child(row().child(div().min_w_0().text_color(t.tx3).child(back)).child(remove));
        Some(crate::agents_rules::section(t, "agents-login-section", "Agent login", vec![login, password, leave]).into_any_element())
    }

    /// The rules changed since the login was made: what it needs, and the button that runs it.
    fn login_drift_lines(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> gpui::Div {
        let col = div().children(ui::here(&"agents-login-drift".into())).flex().flex_col().gap(px(10.)).text_size(px(11.5)).line_height(px(18.));
        match &self.login.drift {
            None => col.child(div().text_color(t.tx3).child("Reading what the login has now…")),
            Some(Err(e)) => {
                let pid = p.clone();
                col.child(div().text_color(t.err).child(e.clone())).child(div().flex().flex_row().child(ui::button(t, "agents-login-drift-retry", ui::Btn::Secondary).h(px(28.)).child("Try again").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_drift(&pid, cx)))))
            }
            Some(Ok(d)) => {
                let n = d.changes.len();
                let pid = p.clone();
                let label = self.login.busy.clone().unwrap_or_else(|| if n == 1 { "Apply 1 change".into() } else { format!("Apply {n} changes") });
                let view = ui::button(t, "agents-login-changes", ui::Btn::Secondary).h(px(30.)).px(px(14.)).cursor_pointer().child("View changes").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
                    v.login.sheet = Some(Sheet::Changes);
                    v.login.sql_scroll.set_offset(gpui::Point::default());
                    cx.notify();
                }));
                col.child(div().text_color(t.tx2).child("Until they run, the login keeps its old grants. Dataman already applies the new rules on top, so nothing new is exposed meanwhile."))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(10.))
                            .child(view)
                            .child(ui::button(t, "agents-login-apply", ui::Btn::Primary).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).child(label).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_apply_drift(&pid, cx)))),
                    )
            }
        }
    }

    /// The sheet's ✕ and Esc: nothing while it creates or removes (that can't be stopped).
    fn login_close(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.login.sheet, Some(Sheet::Create { busy: true, .. } | Sheet::Remove { busy: true, .. })) {
            self.login.sheet = None;
            cx.notify();
        }
    }

    pub(crate) fn login_sheet(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.login.sheet.as_ref()?;
        let close = cx.listener(|v, _: &ClickEvent, _, cx| v.login_close(cx));
        let (title, body, footer) = match sheet {
            Sheet::Create { review, opts, confirm, busy, error } => self.create_sheet(t, p, review.as_ref(), *opts, *confirm, *busy, error.as_deref(), cx),
            Sheet::Remove { plan, confirm, busy, error } => self.remove_sheet(t, p, plan.as_ref(), *confirm, *busy, error.as_deref(), cx),
            Sheet::Changes => self.changes_sheet(t, p, cx)?,
        };
        Some(
            div()
                .id("agents-login-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(ui::esc_closes(None, cx.listener(|v, _: &(), _, cx| v.login_close(cx))))
                .child(
                    ui::modal(t, "agents-login-card")
                        .w(px(1040.))
                        .h(px(720.))
                        .child(ui::modal_header_with(t, None, title, p.env.is_production().then(|| prod_tag(t).into_any_element()), ui::modal_close(t, "agents-login-close").cursor_pointer().on_click(close)))
                        .child(div().flex_1().min_h_0().flex().flex_row().child(body))
                        .child(footer),
                )
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_sheet(&self, t: &Theme, p: &ConnectionProfile, review: Option<&Result<live::Review, String>>, opts: Options, confirm: bool, busy: bool, error: Option<&str>, cx: &mut Context<Self>) -> (String, AnyElement, AnyElement) {
        let user = agent_login::login_name(&p.id);
        let title = format!("Create {user} on {}", p.name);
        let rv = match review {
            None => return (title, div().flex_1().flex().items_center().justify_center().text_color(t.tx3).child("Checking your login and what the rules allow…").into_any_element(), ui::modal_footer(t).into_any_element()),
            Some(Err(e)) => return (title, div().flex_1().p(px(20.)).text_color(t.err).child(e.clone()).into_any_element(), ui::modal_footer(t).into_any_element()),
            Some(Ok(rv)) => rv,
        };
        let plan = &rv.plan;
        let warn_box = |text: String| div().border_1().border_color(t.warn).bg(t.warn_s).px(px(12.)).py(px(10.)).child(text);
        // One column: what stands in the way or needs a hand, the SQL, then the options.
        let mut warn = div().flex().flex_col().gap(px(10.));
        let mut warned = false;
        if !plan.not_granted.is_empty() {
            let names: Vec<String> = plan.not_granted.iter().map(|n| n.object.clone()).collect();
            let reason = plan.not_granted[0].reason.clone();
            warned = true;
            warn = warn.child(warn_box(format!(
                "{} {} can't be granted by {} ({reason}), so the agent won't see {}. To include {}, run the lines marked in the SQL as their owner.",
                names.len(),
                if names.len() == 1 { "table" } else { "tables" },
                rv.probe.login,
                agent_login::short_list(&names),
                if names.len() == 1 { "it" } else { "them" }
            )));
        }
        if !plan.exposed.is_empty() {
            let names: Vec<String> = plan.exposed.iter().map(|n| n.object.clone()).collect();
            warned = true;
            warn = warn.child(warn_box(format!(
                "Every login may read {} ({}), the agent login too: the database won't keep {} from agents, only Dataman does. To close it, their owner runs the REVOKE lines marked in the SQL.",
                agent_login::short_list(&names),
                plan.exposed[0].reason,
                if names.len() == 1 { "it" } else { "them" }
            )));
        }
        for l in &plan.lacks {
            warned = true;
            warn = warn.child(div().children(ui::here(&"agents-login-lacks".into())).border_1().border_color(t.err).bg(t.err_s).px(px(12.)).py(px(10.)).text_color(t.err).child(l.clone()));
        }
        let opt_row = |id: &'static str, on: bool, text: String, sub: Option<&str>| {
            div()
                .children(ui::here(&id.into()))
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .gap(px(10.))
                .cursor_pointer()
                .child(div().pt(px(1.)).children(on.then(|| ui::here(&gpui::SharedString::from(format!("{id}-on")).into())).flatten()).child(checkbox(t, on, false)))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.)).child(div().text_color(t.tx).child(text)).children(sub.map(|s| div().text_size(px(11.)).text_color(t.tx3).child(s.to_string()))))
        };
        let (p1, p2) = (p.clone(), p.clone());
        let mut opts_row = div().flex().flex_row().items_start().gap(px(32.)).child(
            opt_row("agents-login-opt-rotate", opts.rotate_days.is_some(), format!("Rotate its password every {} days", agent_login::ROTATE_DAYS), Some("A new one on the server and in the Keychain, the old one's sessions ended"))
                .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p1, |o| o.rotate_days = if o.rotate_days.is_some() { None } else { Some(agent_login::ROTATE_DAYS) }, cx))),
        );
        // Only where the database itself turns the next session away.
        if agent_login::caps_sessions(p.engine) {
            opts_row = opts_row.child(
                opt_row("agents-login-opt-conns", opts.conn_limit.is_some(), format!("At most {} sessions open at once", agent_login::CONN_LIMIT), Some("The database turns away more, so a runaway agent can't pile up open sessions"))
                    .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p2, |o| o.conn_limit = if o.conn_limit.is_some() { None } else { Some(agent_login::CONN_LIMIT) }, cx))),
            );
        }
        let options = div()
            .children(ui::here(&"agents-login-options".into()))
            .flex_none()
            .px(px(20.))
            .py(px(14.))
            .border_t_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .gap(px(12.))
            .line_height(px(18.))
            .child(ui::caps(t, "LOGIN"))
            .child(opts_row)
            .children(plan.notes.iter().map(|n| div().text_size(px(11.)).text_color(t.tx3).child(n.clone())));
        let body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .when(warned, |d| d.child(div().flex_none().px(px(20.)).py(px(14.)).border_b_1().border_color(t.line).line_height(px(18.)).child(warn)))
            .child(self.what_runs(t, "agents-login-sql", Ok(plan), p.engine))
            .child(options)
            .into_any_element();
        let blocked = !plan.lacks.is_empty();
        let label = match (busy, p.env.is_production(), confirm) {
            (true, _, _) => "Creating…".to_string(),
            (_, true, true) => "Confirm: create on production".to_string(),
            (_, true, false) => "Create login on production".to_string(),
            _ => "Create login".to_string(),
        };
        let pc = p.clone();
        let create = ui::button(t, "agents-login-go", if confirm { ui::Btn::Danger } else { ui::Btn::Primary }).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).child(label);
        let create = if blocked || busy { create.opacity(0.5).into_any_element() } else { create.cursor_pointer().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_create(&pc, cx))).into_any_element() };
        let cancel = ui::button(t, "agents-login-cancel", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Cancel").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = match (error, confirm) {
            (Some(e), _) => div().flex_1().min_w_0().text_color(t.err).overflow_hidden().whitespace_nowrap().text_ellipsis().child(e.to_string()),
            (None, true) => div().flex_1().text_color(t.err).child(format!("Production: this creates a login on {} now.", p.host)),
            (None, false) => div().flex_1().text_color(t.tx3).child(format!("Dataman runs this as {}, then logs in as the new login to check it before keeping it.", if rv.probe.login.is_empty() { "your login" } else { &rv.probe.login })),
        };
        let footer = ui::modal_footer(t).child(say).child(cancel).child(create).into_any_element();
        (title, body, footer)
    }

    /// View changes: the login's grants as a diff (− what goes, + what comes), beside what runs.
    fn changes_sheet(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<(String, AnyElement, AnyElement)> {
        let Some(Ok(d)) = &self.login.drift else { return None };
        let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_default();
        let n = d.changes.len();
        let (gone, new) = (d.changes.iter().filter(|c| !c.add).count(), d.changes.iter().filter(|c| c.add).count());
        let line = |c: &agent_login::Change| {
            let (sign, fg, bg) = if c.add { ("+", t.ok, t.ok_s) } else { ("−", t.err, t.err_s) };
            div().flex_none().min_h(px(22.)).px(px(20.)).py(px(2.)).flex().flex_row().gap(px(10.)).bg(bg).child(div().flex_none().w(px(8.)).text_color(fg).child(sign)).child(div().flex_1().min_w_0().text_color(t.tx).child(c.line.clone()))
        };
        // What goes first, then what comes, as a diff reads.
        let lines = d.changes.iter().filter(|c| !c.add).chain(d.changes.iter().filter(|c| c.add)).map(line);
        let count = |k: usize, sign: &str, color| div().text_color(color).child(format!("{sign}{k}"));
        let diff = div()
            .w(px(460.))
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(t.line)
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .px(px(20.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(ui::caps(t, "CHANGES"))
                    .child(div().ml_auto().flex().flex_row().gap(px(8.)).font_family(mono()).text_size(px(11.)).child(count(gone, "−", t.err)).child(count(new, "+", t.ok))),
            )
            .child(div().children(ui::here(&"agents-login-diff".into())).id("agents-login-diff").flex_1().min_h_0().overflow_y_scroll().py(px(10.)).flex().flex_col().gap(px(2.)).font_family(mono()).text_size(px(11.5)).line_height(px(18.)).children(lines));
        let body = div().flex_1().min_w_0().flex().flex_row().child(diff).child(self.what_runs(t, "agents-login-drift-sql", Ok(&d.plan), p.engine)).into_any_element();
        let pid = p.clone();
        let label = if n == 1 { "Apply 1 change".to_string() } else { format!("Apply {n} changes") };
        let apply = ui::button(t, "agents-login-changes-apply", ui::Btn::Primary).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).cursor_pointer().child(label).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            v.login_apply_drift(&pid, cx);
        }));
        let close = ui::button(t, "agents-login-changes-close", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Close").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = div().flex_1().min_w_0().text_color(t.tx3).child(format!("Runs as your login, then {user} has what the rules allow now."));
        Some((format!("Changes to {user}"), body, ui::modal_footer(t).child(say).child(close).child(apply).into_any_element()))
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_sheet(&self, t: &Theme, p: &ConnectionProfile, plan: Option<&Result<Plan, String>>, confirm: bool, busy: bool, error: Option<&str>, cx: &mut Context<Self>) -> (String, AnyElement, AnyElement) {
        let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_default();
        let title = format!("Remove {user} from {}", p.name);
        let text = match plan {
            None => Err("Reading what Dataman made…"),
            Some(Err(_)) => Err(""),
            Some(Ok(pl)) => Ok(pl),
        };
        let unreachable = matches!(plan, Some(Err(_))) || error.is_some();
        let lead = "Drops the login and only what Dataman made for it (views, their schema or database, its role), revokes what it holds, and forgets its password. Nothing Dataman didn't make is touched. Agents are then refused here until you create a new login or let them use yours.";
        let mut head = div().flex_none().px(px(20.)).py(px(14.)).border_b_1().border_color(t.line).flex().flex_col().gap(px(10.)).line_height(px(18.)).child(div().text_color(t.tx2).child(lead));
        if let Some(Err(e)) = plan {
            head = head.child(div().text_color(t.err).child(e.clone()));
        }
        if let Some(e) = error {
            head = head.child(div().text_color(t.err).child(e.to_string()));
        }
        if unreachable {
            head = head
                .child(div().text_color(t.tx2).child("Can't reach the server, or your login can't drop it? Dataman can forget it on its own side: the login stays on the server until an admin drops it."))
                .child(div().flex().flex_row().child(ui::button(t, "agents-login-forget", ui::Btn::Danger).h(px(28.)).cursor_pointer().child("Forget it in Dataman only").on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.login_forget(cx)))));
        }
        let body = div().flex_1().min_w_0().flex().flex_col().child(head).child(self.what_runs(t, "agents-login-remove-sql", text, p.engine)).into_any_element();
        let ready = matches!(plan, Some(Ok(_))) && !busy;
        let label = match (busy, p.env.is_production(), confirm) {
            (true, _, _) => "Removing…".to_string(),
            (_, true, true) => "Confirm: remove on production".to_string(),
            (_, true, false) => "Remove login on production".to_string(),
            _ => "Remove login".to_string(),
        };
        let pr = p.clone();
        let go = ui::button(t, "agents-login-remove-go", ui::Btn::Danger).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).child(label);
        let go = if ready { go.cursor_pointer().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_remove(&pr, cx))).into_any_element() } else { go.opacity(0.5).into_any_element() };
        let cancel = ui::button(t, "agents-login-remove-cancel", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Cancel").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = if confirm { div().flex_1().text_color(t.err).child(format!("Production: this drops {user} on {} now.", p.host)) } else { div().flex_1() };
        let footer = ui::modal_footer(t).child(say).child(cancel).child(go).into_any_element();
        (title, body, footer)
    }
}

impl AgentsPage {
    /// A sheet's "WHAT RUNS": the plan (or a line in its place) as the editor colours it in the
    /// engine's language, scrolled both ways (a long line runs on to the right instead of being
    /// cut). Only the lines in view are laid out.
    fn what_runs(&self, t: &Theme, id: &'static str, plan: Result<&Plan, &str>, engine: Engine) -> AnyElement {
        let key = (plan.map_or_else(str::to_string, Plan::shown), engine, [t.tx, t.kw, t.str]);
        let mut what = self.login.what.borrow_mut();
        if what.as_ref().is_none_or(|w| w.key != key) {
            let text = plan.map_or_else(str::to_string, Plan::display);
            let lines = crate::editor::highlighted_lines_in(t, &text, engine);
            let widest = (0..lines.len()).max_by_key(|&i| lines[i].0.chars().count()).unwrap_or(0);
            *what = Some(WhatRuns { key, lines, widest });
        }
        let what = what.as_ref().expect("made above");
        // The lines in view and a screenful either side; before the first layout, a sheet's worth.
        let (row, pad, scroll) = (px(19.), px(12.), &self.login.sql_scroll);
        let view = scroll.bounds().size.height;
        let first = (((-scroll.offset().y - pad).as_f32() / row.as_f32()).max(0.) as usize).saturating_sub(WHAT_OVERSCAN);
        let count = if view > gpui::px(0.) { (view.as_f32() / row.as_f32()).ceil() as usize + 2 * WHAT_OVERSCAN } else { 80 };
        let n = what.lines.len();
        let (first, last) = (first.min(n), (first + count).min(n));
        let line = |(s, runs): &(SharedString, Vec<TextRun>)| if s.is_empty() { div().h(row) } else { div().whitespace_nowrap().child(StyledText::new(s.clone()).with_runs(runs.clone())) };
        let widest = (!(first..last).contains(&what.widest)).then(|| what.lines.get(what.widest).map(|l| line(l).h_0().overflow_hidden())).flatten();
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(div().h(px(40.)).flex_none().px(px(20.)).flex().flex_row().items_center().border_b_1().border_color(t.line).child(div().children(ui::here(&"agents-login-what".into())).child(ui::caps(t, "WHAT RUNS"))))
            .child(
                div()
                    .children(ui::here(&id.into()))
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(scroll)
                    .flex()
                    .flex_col()
                    // Each line as wide as it is: the scroll's content is the widest.
                    .items_start()
                    .font_family(mono())
                    .text_size(px(11.5))
                    .line_height(row)
                    .child(
                        div()
                            .min_w_full()
                            .px(px(20.))
                            .py(pad)
                            .flex()
                            .flex_col()
                            .children(widest)
                            .child(div().flex_none().h(row * first as f32))
                            .children(what.lines[first..last].iter().map(line))
                            .child(div().flex_none().h(row * (n - last) as f32)),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(feature = "checks")]
impl AgentsPage {
    /// The demo stages: `p`'s login drifted from its rules, `changes` read from the server.
    pub(crate) fn demo_drift(&mut self, p: &ConnectionProfile, changes: Vec<agent_login::Change>) {
        let now = agent_login::rules_hash(p, &p.agent_login.as_ref().map(|l| l.database.clone()).unwrap_or_default());
        // What runs: the lines themselves, as the plan of a real drift holds them.
        let steps = changes.iter().map(|c| agent_login::Step { action: agent_login::Action::Sql(c.line.clone()), shown: format!("{};", c.line), copy: format!("{};", c.line), group: "Grants".into() }).collect();
        let plan = Plan { engine: Some(p.engine), steps, one_tx: true, ..Default::default() };
        let drift = live::Drift { plan, changes, probe: Default::default(), spec: agent_login::compile(p, "", &Options::default()), inventory: Default::default() };
        self.login = LoginUi { conn: Some(p.id.clone()), drift: Some(Ok(drift)), drift_rules: Some(now), ..Default::default() };
    }

    /// The demo's View changes, open.
    pub(crate) fn demo_changes(&mut self) {
        self.login.sheet = Some(Sheet::Changes);
    }
}

/// Lines laid out above and below WHAT RUNS's view, so a fast scroll doesn't show a blank edge.
const WHAT_OVERSCAN: usize = 20;

enum Verified {
    Gone,
    Checked(Vec<live::Check>),
}

fn prod_tag(t: &Theme) -> gpui::Div {
    div().flex_none().h(px(20.)).px(px(7.)).flex().items_center().text_size(px(10.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.warn).bg(t.warn_s).child("PRODUCTION")
}

fn password_tip() -> Tip {
    Tip::new("Its password")
        .lead(format!("{} random characters, kept in the {}. Dataman and its MCP connect as the login on their own: nothing to paste anywhere.", agent_login::hash::PASSWORD_LEN, dataman_core::secret::STORE_NAME))
        .item("Rotate password", "A new one on the server and in the Keychain; the sessions opened with the old one are ended.")
}

fn agent_login_count(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}
