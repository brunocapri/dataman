//! Agents › Database login: a login of the agents' own on each connection. Off, it shows what
//! the user's login could do (the probe); Create… opens the review (who enforces each rule, what
//! can't be granted, the exact SQL, the options), then the login is created with the user's
//! login, checked by logging in as it, and kept: its password in the Keychain, the rest on the
//! connection. When the rules change it drifts: the changes are shown and applied on request.
//! Verify, Rotate password and Remove login (only what Dataman made, after confirmation) follow.
//! "I'll run it myself" saves nothing until a later Verify finds the login on the server.

use std::sync::Arc;

use chrono::Utc;
use dataman_core::agent_login::{self, AgentLogin, Options, Plan, Who, live};
use dataman_core::privacy::AgentView;
use dataman_core::{Connection, ConnectionProfile, McpAccess};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, Window, div, prelude::*};

use crate::agents::{AgentsPage, Page, checkbox};
use crate::theme::{Theme, mono, px};
use crate::ui;

/// The page's state for the selected connection.
#[derive(Default)]
pub(crate) struct LoginUi {
    /// The connection the rest is about.
    pub(crate) conn: Option<String>,
    pub(crate) probe: Option<Result<agent_login::Probe, String>>,
    pub(crate) busy: Option<String>,
    /// The last action's outcome: good or not, and what to say.
    pub(crate) note: Option<(bool, String)>,
    pub(crate) checks: Vec<live::Check>,
    pub(crate) drift: Option<Result<live::Drift, String>>,
    /// "I'll run it myself": waiting for Verify to find the login.
    pub(crate) pending: Option<Plan>,
    pub(crate) sheet: Option<Sheet>,
}

pub(crate) enum Sheet {
    Create { review: Option<Result<live::Review, String>>, opts: Options, confirm: bool, busy: bool, error: Option<String> },
    Remove { plan: Option<Result<Plan, String>>, confirm: bool, busy: bool, error: Option<String>, waiting: bool },
}

fn who_colors(t: &Theme, w: Who) -> (gpui::Hsla, gpui::Hsla) {
    match w {
        Who::Database => (t.ok, t.ok_s),
        Who::Partly => (t.warn, t.warn_s),
        Who::Dataman => (t.acc, t.acc_s),
    }
}

fn badge(t: &Theme, w: Who) -> gpui::Div {
    let (fg, bg) = who_colors(t, w);
    div().flex_none().h(px(20.)).px(px(7.)).flex().items_center().text_size(px(10.5)).font_weight(FontWeight::SEMIBOLD).text_color(fg).bg(bg).child(w.label())
}

fn ago(at: chrono::DateTime<Utc>) -> String {
    let s = (Utc::now() - at).num_seconds().max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

/// The user's own connection: the open one, else one opened with what Dataman keeps.
async fn admin_of(core: Arc<dataman_core::Core>, open: Option<Arc<Connection>>, p: ConnectionProfile) -> Result<Arc<Connection>, String> {
    if let Some(c) = open {
        return Ok(c);
    }
    core.connect(ConnectionProfile { agent_login: None, ..p }, None).await.map_err(|e| format!("cannot connect as you: {e:#}"))
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

    /// The state for `p`: the probe when it has no login, the drift when its rules changed.
    fn login_refresh(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        self.login = LoginUi { conn: Some(p.id.clone()), ..Default::default() };
        if !agent_login::supported(p.engine) {
            return;
        }
        match &p.agent_login {
            None => self.login_probe(p, cx),
            // Its rules, and what the server has now (a table made since, a grant added by hand).
            Some(l) if l.rotation_due(Utc::now()).is_some_and(|d| d <= 0) => {
                self.login_drift(p, cx);
                self.login_rotate(p, cx);
            }
            Some(_) => self.login_drift(p, cx),
        }
    }

    fn login_probe(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        self.login.probe = None;
        self.login_task(p, cx, |_, admin, p| Box::pin(async move { live::probe(&admin, &p).await.map(|x| x.0).map_err(|e| format!("{e:#}")) }), |v, r, _| v.login.probe = Some(r));
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
    /// background when Dataman can reach the server with what it keeps; the others wait for the
    /// Database login page, which says they are due.
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
                        core.secrets.set(&agent_login::secret_account(&p.id), &new).map_err(|e| format!("{e:#}"))
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
        self.login_task(
            p,
            cx,
            move |_, admin, p| Box::pin(async move { live::review(&admin, &p, &opts).await.map_err(|e| format!("{e:#}")) }),
            |v, r, _| {
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
                Box::pin(async move {
                    live::apply(&admin, &rv.plan, &p.database).await.map_err(|e| format!("{e:#}"))?;
                    let account = agent_login::secret_account(&p.id);
                    core.secrets.set(&account, &rv.password).map_err(|e| format!("the login was created but its password could not be kept in the {}: {e:#}", dataman_core::secret::STORE_NAME))?;
                    let mut rec = live::record(&p, &rv.plan, &opts);
                    let checks = if opts.verify { verify_as_agent(&core, &p, &rec.user, &rv.password, &rv).await } else { Ok(Vec::new()) };
                    match &checks {
                        Ok(c) if c.iter().all(|x| x.ok) => {
                            if opts.verify {
                                rec.verified = Some(Utc::now());
                            }
                        }
                        // Verify before saving: what failed is undone, nothing is kept.
                        _ => {
                            let why = match &checks {
                                Ok(c) => c.iter().filter(|x| !x.ok).map(|x| format!("{}: {}", x.what, x.detail)).collect::<Vec<_>>().join("; "),
                                Err(e) => e.clone(),
                            };
                            let undone = match live::removal(&admin, &p, &rec).await {
                                Ok(plan) => live::apply(&admin, &plan, &p.database).await.is_ok(),
                                Err(_) => false,
                            };
                            if undone {
                                let _ = core.secrets.delete(&account);
                                return Err(format!("logging in as {} did not go as the rules say ({why}); it was removed again", rec.user));
                            }
                            return Ok((rec, checks.unwrap_or_default(), Some(format!("logging in as it did not go as the rules say: {why}"))));
                        }
                    }
                    Ok((rec, checks.unwrap_or_default(), None))
                })
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
                        None => (true, format!("{user} created; agents connect as it from their next call")),
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

    /// "I'll run it myself": the script (its password's hash in it) on the clipboard, the
    /// password kept aside; nothing saved until Verify finds the login.
    fn login_manual(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(Sheet::Create { review: Some(Ok(rv)), .. }) = self.login.sheet.as_ref() else { return };
        let text = rv.atlas.clone().unwrap_or_else(|| rv.plan.copy());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        if let Err(e) = self.core.secrets.set(&agent_login::pending_account(&p.id), &rv.password) {
            self.login.note = Some((false, format!("the password could not be kept in the {}: {e:#}", dataman_core::secret::STORE_NAME)));
            return;
        }
        self.login.pending = Some(rv.plan.clone());
        self.login.sheet = None;
        self.login.note = Some((true, "The script is on the clipboard. Run it as an admin, then Verify.".into()));
        cx.notify();
    }

    fn login_verify(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        self.login.busy = Some("Verifying…".into());
        self.login.note = None;
        let pending = self.login.pending.clone();
        self.login_task(
            p,
            cx,
            move |core, admin, p| {
                Box::pin(async move {
                    let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_else(|| agent_login::login_name(&p.id));
                    let there = live::exists(&admin, &p, &user).await.map_err(|e| format!("{e:#}"))?;
                    match (&p.agent_login, there) {
                        // Removed by hand (Remove › I'll run it myself): the record goes.
                        (Some(_), None) => return Ok(Verified::Gone),
                        (_, Some(false)) => return Err(format!("{user} on the server isn't marked as Dataman's: Dataman leaves it alone")),
                        (None, None) => return Err(format!("{user} is not on the server yet: run the script, then Verify")),
                        _ => {}
                    }
                    let fresh = p.agent_login.is_none();
                    let pw = if fresh {
                        core.secrets.get(&agent_login::pending_account(&p.id)).ok().flatten().ok_or_else(|| "the password made for the script is gone: create the login again".to_string())?
                    } else {
                        live::password_of(&*core.secrets, &p.id, &user).map_err(|e| format!("{e:#}"))?
                    };
                    let rv = live::review(&admin, &p, &Options::default()).await.map_err(|e| format!("{e:#}"))?;
                    let checks = verify_as_agent(&core, &p, &user, &pw, &rv).await?;
                    if fresh {
                        core.secrets.set(&agent_login::secret_account(&p.id), &pw).map_err(|e| format!("{e:#}"))?;
                        let _ = core.secrets.delete(&agent_login::pending_account(&p.id));
                        let plan = pending.unwrap_or(rv.plan);
                        return Ok(Verified::Adopted(live::record(&p, &plan, &Options::default()), checks));
                    }
                    Ok(Verified::Checked(checks))
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
                    Ok(Verified::Adopted(mut rec, checks)) => {
                        let ok = checks.iter().all(|c| c.ok);
                        if ok {
                            rec.verified = Some(Utc::now());
                        }
                        let user = rec.user.clone();
                        v.edit_conn(&id, |p| {
                            p.agent_login = Some(rec);
                            p.agents_refused = false;
                        }, cx);
                        v.login.pending = None;
                        v.login.checks = checks;
                        v.login.note = Some((ok, if ok { format!("{user} found and checked; agents connect as it") } else { format!("{user} found, but logging in as it did not go as the rules say") }));
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
                    Ok(())
                })
            },
            |v, r, cx| {
                v.login.busy = None;
                match r {
                    Ok(()) => {
                        let id = v.login.conn.clone().unwrap_or_default();
                        v.edit_conn(&id, |p| {
                            if let Some(l) = p.agent_login.as_mut() {
                                l.rotated = Some(Utc::now());
                            }
                        }, cx);
                        v.login.note = Some((true, "New password set; agents reconnect with it".into()));
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
        self.login.sheet = Some(Sheet::Remove { plan: None, confirm: false, busy: false, error: None, waiting: false });
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
        let Some(Sheet::Remove { plan: Some(Ok(plan)), confirm, busy, error, .. }) = self.login.sheet.as_mut() else { return };
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
        self.login_task(p, cx, move |_, admin, _| Box::pin(async move { live::apply(&admin, &plan, &db).await.map_err(|e| format!("{e:#}")) }), |v, r, cx| match r {
            Ok(()) => v.login_forget(cx),
            Err(e) => {
                if let Some(Sheet::Remove { busy, error, .. }) = v.login.sheet.as_mut() {
                    *busy = false;
                    *error = Some(e);
                }
            }
        });
    }

    /// Dataman forgets the login: its record and its Keychain password. Agents are refused here
    /// until a new login exists or the user lets them use theirs.
    pub(crate) fn login_forget(&mut self, cx: &mut Context<Self>) {
        let id = self.login.conn.clone().unwrap_or_default();
        let _ = self.core.secrets.delete(&agent_login::secret_account(&id));
        let _ = self.core.secrets.delete(&agent_login::pending_account(&id));
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

    fn login_remove_manual(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Remove { plan, waiting, .. }) = self.login.sheet.as_mut() else { return };
        let text = match plan {
            Some(Ok(p)) => p.copy(),
            _ => return,
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        *waiting = true;
        cx.notify();
    }

    /// The server can't be reached (or the user's login can't drop it): the SQL Dataman would
    /// run, from what it recorded, for later.
    fn login_remove_offline(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        let Some(l) = p.agent_login.as_ref() else { return };
        let plan = live::removal_offline(p, l);
        if let Some(Sheet::Remove { plan: slot, .. }) = self.login.sheet.as_mut() {
            *slot = Some(Ok(plan));
        }
        cx.notify();
    }

    fn login_let_mine(&mut self, p: &ConnectionProfile, cx: &mut Context<Self>) {
        self.edit_conn(&p.id, |p| p.agents_refused = false, cx);
        self.login.note = Some((true, "Agents use your login here again, within Dataman's rules".into()));
        self.login_probe(p, cx);
    }

    /// Opens the page on a connection (the Permissions nudge, the connection list).
    pub(crate) fn show_login(&mut self, id: &str, cx: &mut Context<Self>) {
        self.conn = Some(id.to_string());
        self.login.conn = None;
        self.show(Page::Login, cx);
    }

    // ---- Render ----

    pub(crate) fn render_login(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = self.selected_profile(cx) else { return self.no_conns(t, "Database login") };
        if self.login.conn.as_deref() != Some(p.id.as_str()) {
            self.login_refresh(&p, cx);
        }
        let _ = window;
        let body = div()
            .id("agents-login-body")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .px(px(28.))
            .py(px(22.))
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(self.login_head(t, &p))
            .child(self.login_summary(t, &p))
            .child(self.login_card(t, &p, cx))
            .child(
                div().max_w(px(860.)).text_color(t.tx3).line_height(px(19.)).child(
                    "The login is a ceiling, not a copy: row limits, the real-WHERE rule, production approvals and value masking (cards, e-mails, keys inside any text) stay in Dataman. Statements an agent may only run after your Allow (DDL, TRUNCATE) run on your login after you click it, so the agent's own credential can never change structure.",
                ),
            );
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "Database login"))
            .child(div().flex_1().min_h_0().flex().flex_row().border_t_1().border_color(t.line).child(self.render_conn_list(t, cx)).child(body))
            .children(self.login_sheet(t, &p, cx))
            .into_any_element()
    }

    fn login_head(&self, t: &Theme, p: &ConnectionProfile) -> AnyElement {
        let version = self.login.probe.as_ref().and_then(|r| r.as_ref().ok()).map(|x| x.version.clone()).unwrap_or_else(|| p.engine.label().to_string());
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(p.name.clone()))
            .child(div().text_color(t.tx3).child(format!("{version} · {}", p.host)))
            .when(p.env.is_production(), |d| d.child(prod_tag(t)))
            .into_any_element()
    }

    fn login_summary(&self, t: &Theme, p: &ConnectionProfile) -> AnyElement {
        let r = &p.mcp_rules;
        let tier = p.mcp_access();
        let writes = p.mcp_row_limit.unwrap_or(dataman_core::settings::MCP_WRITE_ROW_LIMIT);
        let (hid_t, hid_c) = (r.visible.items.iter().filter(|i| i.kind != dataman_core::privacy::ItemKind::Column).count(), r.visible.items.iter().filter(|i| i.kind == dataman_core::privacy::ItemKind::Column).count());
        let masked: Vec<String> = r.sensitive.iter().filter(|s| s.on).map(|s| s.col.clone()).collect();
        let secs = r.time_limit().as_secs();
        let cell = |label: &str, value: String, sub: String| {
            div()
                .flex_1()
                .min_w_0()
                .p(px(12.))
                .border_1()
                .border_color(t.line)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_color(t.tx3).child(label.to_string()))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(value))
                .child(div().text_color(t.tx3).overflow_hidden().whitespace_nowrap().text_ellipsis().child(sub))
        };
        let on_off = |b: bool| if b { "On" } else { "Off" };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(ui::caps(t, "WHAT AGENTS MAY DO"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(cell("Access", capitalize(tier.label()), if tier >= McpAccess::Write { format!("{writes} rows per write") } else { "no writes".into() }))
                    .child(cell("Data", r.view.label().to_string(), if r.view == AgentView::Masked { if masked.is_empty() { "sensitive types only".into() } else { masked.join(", ") } } else { String::new() }))
                    .child(cell("Hidden", format!("{} · {}", agent_login_count(hid_t, "table"), agent_login_count(hid_c, "column")), r.visible.items.iter().take(3).map(|i| i.name.clone()).collect::<Vec<_>>().join(", ")))
                    .child(cell("Structure · code", format!("{} · {}", on_off(r.structure), on_off(r.code)), if secs < 60 { format!("{secs} s per call") } else { format!("{} min per call", secs / 60) })),
            )
            .into_any_element()
    }

    fn login_card(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().gap(px(10.)).child(ui::caps(t, "DATABASE LOGIN FOR AGENTS"));
        if !agent_login::supported(p.engine) {
            return col.child(div().p(px(18.)).border_1().border_color(t.line).text_color(t.tx2).line_height(px(19.)).child(agent_login::FILE_NOTE)).into_any_element();
        }
        let card = match (&p.agent_login, &self.login.pending) {
            (Some(l), _) => {
                let drifted = l.rules != agent_login::rules_hash(p, &l.database) || self.login.drift.as_ref().is_some_and(|d| d.as_ref().is_ok_and(|d| !d.changes.is_empty()));
                if drifted { self.login_drift_card(t, p, l, cx) } else { self.login_on_card(t, p, l, cx) }
            }
            (None, Some(_)) => self.login_pending_card(t, p, cx),
            (None, None) => self.login_off_card(t, p, cx),
        };
        col = col.child(card);
        if let Some((ok, text)) = &self.login.note {
            col = col.child(div().children(ui::here(&"agents-login-note".into())).text_color(if *ok { t.tx2 } else { t.err }).line_height(px(18.)).child(text.clone()));
        }
        if !self.login.checks.is_empty() {
            col = col.child(div().flex().flex_col().gap(px(4.)).children(self.login.checks.iter().map(|c| {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(div().flex_none().w(px(12.)).text_color(if c.ok { t.ok } else { t.err }).child(if c.ok { "✓" } else { "✕" }))
                    .child(div().text_color(t.tx2).child(c.what.clone()))
                    .when(!c.ok && !c.detail.is_empty(), |d| d.child(div().text_color(t.tx3).overflow_hidden().whitespace_nowrap().text_ellipsis().child(c.detail.clone())))
            })));
        }
        col.into_any_element()
    }

    fn login_off_card(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let refused = p.agents_refused;
        let (title, text) = if refused {
            ("Agents are refused on this connection".to_string(), "Its agent login was removed. Create a new one, or let agents use your login again (Dataman's rules still apply).".to_string())
        } else {
            (
                format!("Agents connect with your login, {}", if p.user.is_empty() { "the connection's" } else { &p.user }),
                "Dataman's rules above are all that stands between the agent and the data. Give agents a login of their own and the database refuses what the rules refuse too, even if a rule had a gap.".to_string(),
            )
        };
        let pid = p.clone();
        let create = ui::button(t, "agents-login-create", ui::Btn::Primary).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).cursor_pointer().child("Create agent login…").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_create(&pid, cx)));
        let pid = p.clone();
        let mine = refused.then(|| ui::button(t, "agents-login-mine", ui::Btn::Secondary).h(px(30.)).cursor_pointer().child("Let agents use my login").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_let_mine(&pid, cx))));
        let checks: AnyElement = match &self.login.probe {
            None => div().text_color(t.tx3).child("Checking what your login can do…").into_any_element(),
            Some(Err(e)) => div().text_color(t.warn).child(format!("Couldn't check your login: {e}")).into_any_element(),
            Some(Ok(pr)) => div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(18.))
                .text_color(t.tx3)
                .children(pr.checks.iter().map(|(ok, s)| div().flex().flex_row().gap(px(6.)).child(div().text_color(if *ok { t.ok } else { t.warn }).child(if *ok { "✓" } else { "!" })).child(s.clone())))
                .children(pr.lacks.first().map(|l| div().w_full().text_color(t.warn).child(l.clone())))
                .into_any_element(),
        };
        div()
            .p(px(18.))
            .border_1()
            .border_color(if refused { t.err } else { t.line })
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(14.))
                    .child(div().flex_1().flex().flex_col().gap(px(8.)).line_height(px(19.)).child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.)).child(title)).child(div().text_color(t.tx2).child(text)))
                    .children(mine)
                    .child(create),
            )
            .child(checks)
            .into_any_element()
    }

    fn login_pending_card(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let user = agent_login::login_name(&p.id);
        let pid = p.clone();
        let verify = ui::button(t, "agents-login-verify", ui::Btn::Primary).h(px(30.)).cursor_pointer().child(self.login.busy.clone().unwrap_or_else(|| "Verify".into())).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_verify(&pid, cx)));
        let copy = self.login.pending.as_ref().map(|pl| pl.copy()).unwrap_or_default();
        let again = ui::button(t, "agents-login-copy-again", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child("Copy SQL").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))));
        let pid = p.id.clone();
        let forget = ui::button(t, "agents-login-forget-pending", ui::Btn::Ghost).h(px(28.)).cursor_pointer().child("Forget it").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
            let _ = v.core.secrets.delete(&agent_login::pending_account(&pid));
            v.login.pending = None;
            v.login.note = None;
            cx.notify();
        }));
        div()
            .p(px(18.))
            .border_1()
            .border_color(t.line)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .child(div().flex_1().flex().flex_col().gap(px(4.)).child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.)).child(format!("Waiting for {user} on the server"))).child(div().text_color(t.tx3).child("Run the script as an admin; Verify finds the login, logs in as it, and only then saves it.")))
            .child(forget)
            .child(again)
            .child(verify)
            .into_any_element()
    }

    fn login_on_card(&self, t: &Theme, p: &ConnectionProfile, l: &AgentLogin, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.login.busy.clone();
        let verified = l.verified.map(|v| format!("verified {} by logging in as it", ago(v))).unwrap_or_else(|| "not verified yet".into());
        let mine = if p.user.is_empty() { "your login".to_string() } else { p.user.clone() };
        let pv = p.clone();
        let verify = ui::button(t, "agents-login-verify", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child(if busy.as_deref() == Some("Verifying…") { "Verifying…" } else { "Verify" }).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_verify(&pv, cx)));
        let pr = p.clone();
        let rotate = ui::button(t, "agents-login-rotate", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child("Rotate password").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_rotate(&pr, cx)));
        let pm = p.clone();
        let remove = ui::button(t, "agents-login-remove", ui::Btn::Ghost).h(px(28.)).text_color(t.err).cursor_pointer().child("Remove login…").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_remove(&pm, cx)));
        let rotate_text = match l.rotation_due(Utc::now()) {
            Some(d) if d <= 0 => "Keychain · rotation due".to_string(),
            Some(d) => format!("Keychain · rotates in {d} d"),
            None => "Keychain".to_string(),
        };
        let cell = |label: &str, value: String, warn: bool, first: bool| {
            div()
                .flex_1()
                .px(px(18.))
                .py(px(12.))
                .when(!first, |d| d.border_l_1().border_color(t.line))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_color(t.tx3).child(label.to_string()))
                .child(div().text_color(if warn { t.warn } else { t.tx }).child(value))
        };
        let views = l.views.len();
        div()
            .border_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(18.))
                    .py(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .child(div().size(px(8.)).rounded_full().bg(t.ok))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().children(ui::here(&"agents-login-as".into())).font_weight(FontWeight::SEMIBOLD).text_size(px(13.)).child(format!("Agents connect as {}", l.user)))
                            .child(div().text_color(t.tx3).child(format!("In sync with the rules · {verified} · your tabs keep using {mine}"))),
                    )
                    .child(verify)
                    .child(rotate)
                    .child(remove),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(t.line)
                    .flex()
                    .flex_row()
                    .child(cell("Password", rotate_text, l.rotation_due(Utc::now()).is_some_and(|d| d <= 0), true))
                    .child(cell("Granted", format!("{} · {}", agent_login_count(l.granted, "table"), agent_login_count(views, "view")), false, false))
                    .child(cell("Not grantable", if l.not_granted == 0 { "none".into() } else { agent_login_count(l.not_granted, "table") }, l.not_granted > 0, false))
                    .child(cell("Connections", l.conn_limit.map(|n| format!("at most {n}")).unwrap_or_else(|| "no cap".into()), false, false)),
            )
            .into_any_element()
    }

    fn login_drift_card(&self, t: &Theme, p: &ConnectionProfile, l: &AgentLogin, cx: &mut Context<Self>) -> AnyElement {
        let (head, button, lines): (String, Option<AnyElement>, Vec<AnyElement>) = match &self.login.drift {
            None => (format!("The rules changed: {} needs changes", l.user), None, vec![div().text_color(t.tx3).child("Reading what the login has now…").into_any_element()]),
            Some(Err(e)) => {
                let pid = p.clone();
                (
                    format!("The rules changed: {} needs changes", l.user),
                    Some(ui::button(t, "agents-login-drift-retry", ui::Btn::Secondary).h(px(30.)).cursor_pointer().child("Try again").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_drift(&pid, cx))).into_any_element()),
                    vec![div().text_color(t.err).child(e.clone()).into_any_element()],
                )
            }
            Some(Ok(d)) => {
                let n = d.changes.len();
                let pid = p.clone();
                let label = self.login.busy.clone().unwrap_or_else(|| if n == 1 { "Apply 1 change".into() } else { format!("Apply {n} changes") });
                let lines = d
                    .changes
                    .iter()
                    .map(|c| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_none().text_color(if c.add { t.ok } else { t.err }).child(if c.add { "+" } else { "−" }))
                            .child(div().text_color(t.tx2).child(c.line.clone()))
                            .into_any_element()
                    })
                    .collect();
                (
                    format!("The rules changed: {} needs {}", l.user, if n == 1 { "1 change".to_string() } else { format!("{n} changes") }),
                    Some(ui::button(t, "agents-login-apply", ui::Btn::Primary).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).cursor_pointer().child(label).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_apply_drift(&pid, cx))).into_any_element()),
                    lines,
                )
            }
        };
        div()
            .border_1()
            .border_color(t.warn)
            .bg(t.warn_s)
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(18.))
                    .py(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .child(div().size(px(8.)).rounded_full().bg(t.warn))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().children(ui::here(&"agents-login-drift".into())).font_weight(FontWeight::SEMIBOLD).text_size(px(13.)).child(head))
                            .child(div().text_color(t.tx2).child("Until they run, the login keeps its old grants. Dataman already applies the new rules on top, so nothing new is exposed meanwhile.")),
                    )
                    .children(button),
            )
            .child(div().border_t_1().border_color(t.warn).px(px(18.)).py(px(12.)).flex().flex_col().gap(px(6.)).font_family(mono()).line_height(px(19.)).children(lines))
            .into_any_element()
    }

    fn login_sheet(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.login.sheet.as_ref()?;
        let close = cx.listener(|v, _: &ClickEvent, _, cx| {
            if !matches!(v.login.sheet, Some(Sheet::Create { busy: true, .. } | Sheet::Remove { busy: true, .. })) {
                v.login.sheet = None;
                cx.notify();
            }
        });
        let (title, body, footer) = match sheet {
            Sheet::Create { review, opts, confirm, busy, error } => self.create_sheet(t, p, review.as_ref(), *opts, *confirm, *busy, error.as_deref(), cx),
            Sheet::Remove { plan, confirm, busy, error, waiting } => self.remove_sheet(t, p, plan.as_ref(), *confirm, *busy, error.as_deref(), *waiting, cx),
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
        let rules = plan.rules.iter().map(|r| {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(10.))
                .py(px(8.))
                .border_b_1()
                .border_color(t.line)
                .line_height(px(17.))
                .child(div().flex_1().flex().flex_col().child(div().child(r.rule.clone())).child(div().text_color(t.tx3).child(r.how.clone())))
                .child(badge(t, r.who))
        });
        let warn_box = |text: String| div().border_1().border_color(t.warn).bg(t.warn_s).px(px(12.)).py(px(10.)).line_height(px(18.)).child(text);
        let mut left = div()
            .id("agents-login-rules")
            .w(px(430.))
            .flex_none()
            .border_r_1()
            .border_color(t.line)
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(ui::caps(t, "WHO ENFORCES EACH RULE"))
            .child(div().flex().flex_col().children(rules));
        if !plan.not_granted.is_empty() {
            let names: Vec<String> = plan.not_granted.iter().map(|n| n.object.clone()).collect();
            let reason = plan.not_granted[0].reason.clone();
            left = left.child(warn_box(format!(
                "{} {} can't be granted by {} ({reason}), so the agent won't see {}. To include {}, run the lines marked below as their owner.",
                names.len(),
                if names.len() == 1 { "table" } else { "tables" },
                rv.probe.login,
                agent_login::short_list(&names),
                if names.len() == 1 { "it" } else { "them" }
            )));
        }
        for l in &plan.lacks {
            left = left.child(div().border_1().border_color(t.err).bg(t.err_s).px(px(12.)).py(px(10.)).line_height(px(18.)).text_color(t.err).child(l.clone()));
        }
        for n in &plan.notes {
            left = left.child(div().text_color(t.tx3).line_height(px(18.)).child(n.clone()));
        }
        let opt_row = |id: &'static str, on: bool, text: String, fixed: bool| {
            div().children(ui::here(&id.into())).id(id).flex().flex_row().items_center().gap(px(10.)).when(!fixed, |d| d.cursor_pointer()).child(checkbox(t, on, fixed)).child(div().text_color(if fixed { t.tx2 } else { t.tx }).child(text))
        };
        let (p1, p2, p3) = (p.clone(), p.clone(), p.clone());
        left = left
            .child(ui::caps(t, "LOGIN"))
            .child(opt_row("agents-login-opt-pw", true, format!("Generate a {}-character password, kept in the {}", agent_login::hash::PASSWORD_LEN, dataman_core::secret::STORE_NAME), true))
            .child(opt_row("agents-login-opt-rotate", opts.rotate_days.is_some(), format!("Rotate it every {} days", agent_login::ROTATE_DAYS), false).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p1, |o| o.rotate_days = if o.rotate_days.is_some() { None } else { Some(agent_login::ROTATE_DAYS) }, cx))))
            .child(opt_row("agents-login-opt-conns", opts.conn_limit.is_some(), format!("At most {} connections", agent_login::CONN_LIMIT), false).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p2, |o| o.conn_limit = if o.conn_limit.is_some() { None } else { Some(agent_login::CONN_LIMIT) }, cx))))
            .child(opt_row("agents-login-opt-verify", opts.verify, "Verify by logging in as it before saving".into(), false).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p3, |o| o.verify = !o.verify, cx))));
        let script = rv.atlas.clone().unwrap_or_else(|| plan.shown());
        let copy_text = rv.atlas.clone().unwrap_or_else(|| plan.copy());
        let comment = |l: &str| l.starts_with("--") || l.starts_with('#') || l.starts_with("//");
        let lines = script.lines().map(|l| div().whitespace_nowrap().text_color(if comment(l.trim_start()) { t.tx3 } else { t.tx2 }).child(if l.is_empty() { " ".to_string() } else { l.to_string() }));
        let what = if plan.one_tx { "WHAT RUNS · ONE TRANSACTION" } else if rv.atlas.is_some() { "WHAT TO CREATE IN ATLAS" } else { "WHAT RUNS · IN THIS ORDER" };
        let right = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .px(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(ui::caps(t, what))
                    .child(div().flex_1())
                    .child(ui::button(t, "agents-login-copy", ui::Btn::Ghost).h(px(24.)).cursor_pointer().child("Copy SQL").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()))))),
            )
            .child(div().id("agents-login-sql").flex_1().min_h_0().overflow_scroll().px(px(16.)).py(px(12.)).font_family(mono()).text_size(px(11.5)).line_height(px(19.)).flex().flex_col().children(lines));
        let body = div().flex_1().min_w_0().flex().flex_row().child(left).child(right).into_any_element();
        let blocked = !plan.lacks.is_empty() || rv.atlas.is_some();
        let label = match (busy, p.env.is_production(), confirm) {
            (true, _, _) => "Creating…".to_string(),
            (_, true, true) => "Confirm: create on production".to_string(),
            (_, true, false) => "Create login on production".to_string(),
            _ => "Create login".to_string(),
        };
        let pc = p.clone();
        let create = ui::button(t, "agents-login-go", if confirm { ui::Btn::Danger } else { ui::Btn::Primary }).h(px(30.)).px(px(14.)).font_weight(FontWeight::SEMIBOLD).child(label);
        let create = if blocked || busy { create.opacity(0.5).into_any_element() } else { create.cursor_pointer().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_create(&pc, cx))).into_any_element() };
        let pm = p.clone();
        let manual = ui::button(t, "agents-login-manual", ui::Btn::Secondary).h(px(30.)).cursor_pointer().child("I'll run it myself").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_manual(&pm, cx)));
        let cancel = ui::button(t, "agents-login-cancel", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Cancel").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = match (error, confirm) {
            (Some(e), _) => div().flex_1().min_w_0().text_color(t.err).overflow_hidden().whitespace_nowrap().text_ellipsis().child(e.to_string()),
            (None, true) => div().flex_1().text_color(t.err).child(format!("Production: this creates a login on {} now.", p.host)),
            (None, false) => div().flex_1().text_color(t.tx3).child(format!("Dataman runs this as {}, then logs in as the new login to check it.", if rv.probe.login.is_empty() { "your login" } else { &rv.probe.login })),
        };
        let footer = ui::modal_footer(t).child(say).child(cancel).child(manual).child(create).into_any_element();
        (title, body, footer)
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_sheet(&self, t: &Theme, p: &ConnectionProfile, plan: Option<&Result<Plan, String>>, confirm: bool, busy: bool, error: Option<&str>, waiting: bool, cx: &mut Context<Self>) -> (String, AnyElement, AnyElement) {
        let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_default();
        let title = format!("Remove {user} from {}", p.name);
        let text = match plan {
            None => "Reading what Dataman made…".to_string(),
            Some(Err(_)) => String::new(),
            Some(Ok(pl)) => pl.shown(),
        };
        let lines = text.lines().map(|l| div().whitespace_nowrap().text_color(if l.trim_start().starts_with("--") || l.starts_with('#') || l.starts_with("//") { t.tx3 } else { t.tx2 }).child(if l.is_empty() { " ".to_string() } else { l.to_string() }));
        let unreachable = matches!(plan, Some(Err(_))) || error.is_some();
        let lead = "Drops the login and only what Dataman made for it (views, their schema or database, its role), revokes what it holds, and forgets its password. Nothing Dataman didn't make is touched. Agents are then refused here until you create a new login or let them use yours.";
        let mut left = div().w(px(430.)).flex_none().border_r_1().border_color(t.line).px(px(20.)).py(px(16.)).flex().flex_col().gap(px(14.)).line_height(px(18.)).child(div().text_color(t.tx2).child(lead));
        if let Some(Err(e)) = plan {
            left = left.child(div().text_color(t.err).child(e.clone()));
        }
        if let Some(e) = error {
            left = left.child(div().text_color(t.err).child(e.to_string()));
        }
        if unreachable {
            let pid = p.clone();
            left = left
                .child(div().text_color(t.tx2).child("Can't reach the server, or your login can't drop it? Dataman can forget it on its own side: the login stays on the server until someone runs the SQL."))
                .child(ui::button(t, "agents-login-sql-later", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child("Show the SQL to run later").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_remove_offline(&pid, cx))))
                .child(ui::button(t, "agents-login-forget", ui::Btn::Danger).h(px(28.)).cursor_pointer().child("Forget it in Dataman only (the user stays on the server)").on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.login_forget(cx))));
        }
        if waiting {
            let pv = p.clone();
            left = left.child(div().text_color(t.tx2).child("The SQL is on the clipboard. Run it as an admin, then Verify: once the login is gone, Dataman forgets it.")).child(
                ui::button(t, "agents-login-remove-verify", ui::Btn::Primary).h(px(28.)).cursor_pointer().child("Verify").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
                    v.login.sheet = None;
                    v.login_verify(&pv, cx);
                })),
            );
        }
        let copy = match plan {
            Some(Ok(pl)) => pl.copy(),
            _ => String::new(),
        };
        let right = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .px(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .border_b_1()
                    .border_color(t.line)
                    .child(ui::caps(t, "WHAT RUNS"))
                    .child(div().flex_1())
                    .child(ui::button(t, "agents-login-remove-copy", ui::Btn::Ghost).h(px(24.)).cursor_pointer().child("Copy SQL").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))))),
            )
            .child(div().id("agents-login-remove-sql").flex_1().min_h_0().overflow_scroll().px(px(16.)).py(px(12.)).font_family(mono()).text_size(px(11.5)).line_height(px(19.)).flex().flex_col().children(lines));
        let body = div().flex_1().min_w_0().flex().flex_row().child(left).child(right).into_any_element();
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
        let manual = ui::button(t, "agents-login-remove-manual", ui::Btn::Secondary).h(px(30.)).cursor_pointer().child("I'll run it myself").on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.login_remove_manual(cx)));
        let cancel = ui::button(t, "agents-login-remove-cancel", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Cancel").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = if confirm { div().flex_1().text_color(t.err).child(format!("Production: this drops {user} on {} now.", p.host)) } else { div().flex_1() };
        let footer = ui::modal_footer(t).child(say).child(cancel).child(manual).child(go).into_any_element();
        (title, body, footer)
    }

    /// Permissions' nudge: always recommended where agents may use the connection.
    pub(crate) fn login_nudge(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        if p.mcp_access() == McpAccess::Off || !agent_login::supported(p.engine) || p.agent_login.is_some() {
            return None;
        }
        let id = p.id.clone();
        let text = if p.agents_refused { "Agents are refused here: its agent login was removed." } else { "Recommended: give agents a database login of their own, so the database refuses what these rules refuse too." };
        Some(
            div()
                .children(ui::here(&"agents-login-nudge".into()))
                .id("agents-login-nudge")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .text_size(px(11.))
                .text_color(t.tx2)
                .cursor_pointer()
                .child(div().flex_1().child(text))
                .child(div().text_color(t.acc).child("Database login →"))
                .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.show_login(&id, cx)))
                .into_any_element(),
        )
    }
}

enum Verified {
    Gone,
    Adopted(AgentLogin, Vec<live::Check>),
    Checked(Vec<live::Check>),
}

/// Logs in as the agent with `pw` and runs Verify's checks; the connection is closed after.
async fn verify_as_agent(core: &Arc<dataman_core::Core>, p: &ConnectionProfile, user: &str, pw: &str, rv: &live::Review) -> Result<Vec<live::Check>, String> {
    let agent = core.connect(live::agent_profile(p, user), Some(pw.to_string())).await.map_err(|e| format!("cannot log in as {user}: {e:#}"))?;
    let r = live::verify(&agent, &rv.spec, &rv.inventory, &rv.probe).await.map_err(|e| format!("{e:#}"));
    agent.close();
    r
}

fn prod_tag(t: &Theme) -> gpui::Div {
    div().flex_none().h(px(20.)).px(px(7.)).flex().items_center().text_size(px(10.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.warn).bg(t.warn_s).child("PRODUCTION")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn agent_login_count(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}
