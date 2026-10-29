//! Permissions › "Create db user based on these settings": a login of the agents' own on each
//! connection, under Limits. Off, its Create agent login opens the review (what can't be granted,
//! the exact SQL, the options); the login is created with the user's login, checked by logging in
//! as it, and only then kept: its password in the Keychain, the rest on the connection. When the
//! rules change it drifts: the changes are shown and applied on request. Verify, Rotate password
//! and Remove login (only what Dataman made, after confirmation) follow, in the same section.

use std::sync::Arc;

use chrono::Utc;
use dataman_core::agent_login::{self, AgentLogin, Options, Plan, live};
use dataman_core::{Connection, ConnectionProfile};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, StyledText, div, prelude::*};

use crate::agents::{AgentsPage, Page, Tip, checkbox, label_tip};
use crate::theme::{Theme, mono, px};
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
}

pub(crate) enum Sheet {
    Create { review: Option<Result<live::Review, String>>, opts: Options, confirm: bool, busy: bool, error: Option<String> },
    Remove { plan: Option<Result<Plan, String>>, confirm: bool, busy: bool, error: Option<String> },
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

pub(crate) fn login_tip() -> Tip {
    Tip::new("Create db user based on these settings")
        .lead("A database login of the agents' own, made from this connection's rules (Permissions and Privacy), so the database itself refuses what the rules refuse, even if one of Dataman's checks had a gap.")
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
                    // Always checked by logging in as it before it is kept: what fails is undone.
                    let checks = verify_as_agent(&core, &p, &rec.user, &rv.password, &rv).await;
                    if let Ok(c) = &checks
                        && c.iter().all(|x| x.ok)
                    {
                        rec.verified = Some(Utc::now());
                        return Ok((rec, checks.unwrap_or_default(), None));
                    }
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
                    Ok((rec, checks.unwrap_or_default(), Some(format!("logging in as it did not go as the rules say: {why}"))))
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
                    Ok(Verified::Checked(verify_as_agent(&core, &p, &user, &pw, &rv).await?))
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

    /// Permissions on a connection, at its database login's section (deep links to it).
    pub(crate) fn show_login(&mut self, id: &str, cx: &mut Context<Self>) {
        self.conn = Some(id.to_string());
        self.show(Page::Permissions, cx);
        // The section sits near the top of the page's right column.
        self.perm_scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
    }

    // ---- Render ----

    /// Permissions, under Limits: the login made from these settings, or what to do to make one.
    pub(crate) fn login_section(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div()
            .children(ui::here(&"agents-login-section".into()))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(label_tip(t, "agents-login-tip", "CREATE DB USER BASED ON THESE SETTINGS", login_tip()));
        if !agent_login::supported(p.engine) {
            return col.child(div().text_size(px(11.5)).text_color(t.tx3).line_height(px(18.)).child(agent_login::FILE_NOTE)).into_any_element();
        }
        let card = match &p.agent_login {
            Some(l) => {
                let drifted = l.rules != agent_login::rules_hash(p, &l.database) || self.login.drift.as_ref().is_some_and(|d| d.as_ref().is_ok_and(|d| !d.changes.is_empty()));
                if drifted { self.login_drift_card(t, p, l, cx) } else { self.login_on_card(t, p, l, cx) }
            }
            None => self.login_off_card(t, p, cx),
        };
        col = col.child(card);
        if let Some((ok, text)) = &self.login.note {
            col = col.child(div().children(ui::here(&"agents-login-note".into())).text_size(px(11.5)).text_color(if *ok { t.tx2 } else { t.err }).line_height(px(18.)).child(text.clone()));
        }
        if !self.login.checks.is_empty() {
            col = col.child(div().flex().flex_col().gap(px(4.)).text_size(px(11.5)).children(self.login.checks.iter().map(|c| {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .min_w_0()
                    .child(div().flex_none().w(px(12.)).text_color(if c.ok { t.ok } else { t.err }).child(if c.ok { "✓" } else { "✕" }))
                    .child(div().flex_none().text_color(t.tx2).child(c.what.clone()))
                    .when(!c.ok && !c.detail.is_empty(), |d| d.child(div().min_w_0().text_color(t.tx3).overflow_hidden().whitespace_nowrap().text_ellipsis().child(c.detail.clone())))
            })));
        }
        col.into_any_element()
    }

    /// Where a login was removed, said under the tier: agents are refused until the user chooses.
    pub(crate) fn login_refused_note(&self, t: &Theme, p: &ConnectionProfile) -> Option<AnyElement> {
        (p.agents_refused && p.agent_login.is_none()).then(|| div().children(ui::here(&"agents-login-refused".into())).text_size(px(11.)).text_color(t.err).child("Agents are refused here: its agent login was removed.").into_any_element())
    }

    fn login_off_card(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let refused = p.agents_refused;
        let text = if refused {
            "Agents are refused here: its agent login was removed. Create a new one, or let agents use your login again (Dataman's rules still apply).".to_string()
        } else {
            format!("Agents connect with your login, {}: Dataman's rules are all that stands between them and the data.", if p.user.is_empty() { "the connection's" } else { &p.user })
        };
        let buttons = if agent_login::mongo::atlas(p) {
            // Atlas makes users only through its own UI, CLI or Admin API.
            div().text_color(t.tx3).child("On MongoDB Atlas, database users are made in Atlas itself, so Dataman can't create one here.")
        } else {
            let pid = p.clone();
            let create = ui::button(t, "agents-login-create", ui::Btn::Primary).h(px(28.)).px(px(12.)).font_weight(FontWeight::SEMIBOLD).cursor_pointer().child("Create agent login").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_create(&pid, cx)));
            let pid = p.clone();
            let mine = refused.then(|| ui::button(t, "agents-login-mine", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child("Let agents use my login").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_let_mine(&pid, cx))));
            div().flex().flex_row().flex_wrap().gap(px(8.)).child(create).children(mine)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .text_size(px(11.5))
            .line_height(px(18.))
            .child(div().text_color(if refused { t.err } else { t.tx2 }).child(text))
            .child(buttons)
            .into_any_element()
    }

    fn login_on_card(&self, t: &Theme, p: &ConnectionProfile, l: &AgentLogin, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.login.busy.clone();
        let verified = l.verified.map(|v| format!("verified {} by logging in as it", ago(v))).unwrap_or_else(|| "not verified yet".into());
        let mine = if p.user.is_empty() { "your login".to_string() } else { p.user.clone() };
        let pv = p.clone();
        let verify = ui::button(t, "agents-login-verify", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child(if busy.as_deref() == Some("Verifying…") { "Verifying…" } else { "Verify" }).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_verify(&pv, cx)));
        let pr = p.clone();
        let rotate = ui::button(t, "agents-login-rotate", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child(if busy.as_deref() == Some("A new password…") { "A new password…" } else { "Rotate password" }).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_rotate(&pr, cx)));
        let pm = p.clone();
        let remove = ui::button(t, "agents-login-remove", ui::Btn::Ghost).h(px(28.)).text_color(t.err).cursor_pointer().child("Remove login…").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_open_remove(&pm, cx)));
        let rotate_text = match l.rotation_due(Utc::now()) {
            Some(d) if d <= 0 => format!("{} · rotation due", dataman_core::secret::STORE_NAME),
            Some(d) => format!("{} · rotates in {d} d", dataman_core::secret::STORE_NAME),
            None => dataman_core::secret::STORE_NAME.to_string(),
        };
        let row = |label: &str, value: String, warn: bool| {
            div().flex().flex_row().gap(px(12.)).child(div().w(px(110.)).flex_none().text_color(t.tx3).child(label.to_string())).child(div().flex_1().min_w_0().text_color(if warn { t.warn } else { t.tx }).child(value))
        };
        let sessions = match l.conn_limit {
            Some(n) if agent_login::caps_sessions(p.engine) => format!("at most {n} at once"),
            _ => "no cap".into(),
        };
        div()
            .p(px(14.))
            .border_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .gap(px(12.))
            .text_size(px(11.5))
            .line_height(px(18.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(10.))
                    .child(div().mt(px(5.)).flex_none().size(px(8.)).rounded_full().bg(t.ok))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().children(ui::here(&"agents-login-as".into())).font_weight(FontWeight::SEMIBOLD).text_size(px(12.5)).child(format!("Agents connect as {}", l.user)))
                            .child(div().text_color(t.tx3).child(format!("In sync with the rules · {verified}"))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(row("Password", rotate_text, l.rotation_due(Utc::now()).is_some_and(|d| d <= 0)))
                    .child(row("Granted", format!("{} · {}", agent_login_count(l.granted, "table"), agent_login_count(l.views.len(), "view")), false))
                    .child(row("Not grantable", if l.not_granted == 0 { "none".into() } else { agent_login_count(l.not_granted, "table") }, l.not_granted > 0))
                    .child(row("Sessions", sessions, false))
                    .child(row("Your tabs", format!("keep using {mine}"), false)),
            )
            .child(div().flex().flex_row().flex_wrap().gap(px(8.)).child(verify).child(rotate).child(remove))
            .into_any_element()
    }

    fn login_drift_card(&self, t: &Theme, p: &ConnectionProfile, l: &AgentLogin, cx: &mut Context<Self>) -> AnyElement {
        let (head, button, lines): (String, Option<AnyElement>, Vec<AnyElement>) = match &self.login.drift {
            None => (format!("The rules changed: {} needs changes", l.user), None, vec![div().text_color(t.tx3).child("Reading what the login has now…").into_any_element()]),
            Some(Err(e)) => {
                let pid = p.clone();
                (
                    format!("The rules changed: {} needs changes", l.user),
                    Some(ui::button(t, "agents-login-drift-retry", ui::Btn::Secondary).h(px(28.)).cursor_pointer().child("Try again").on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_drift(&pid, cx))).into_any_element()),
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
                            .child(div().flex_1().min_w_0().text_color(t.tx2).child(c.line.clone()))
                            .into_any_element()
                    })
                    .collect();
                (
                    format!("The rules changed: {} needs {}", l.user, if n == 1 { "1 change".to_string() } else { format!("{n} changes") }),
                    Some(ui::button(t, "agents-login-apply", ui::Btn::Primary).h(px(28.)).px(px(12.)).font_weight(FontWeight::SEMIBOLD).cursor_pointer().child(label).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_apply_drift(&pid, cx))).into_any_element()),
                    lines,
                )
            }
        };
        div()
            .p(px(14.))
            .border_1()
            .border_color(t.warn)
            .bg(t.warn_s)
            .flex()
            .flex_col()
            .gap(px(10.))
            .text_size(px(11.5))
            .line_height(px(18.))
            .child(div().children(ui::here(&"agents-login-drift".into())).font_weight(FontWeight::SEMIBOLD).text_size(px(12.5)).child(head))
            .child(div().text_color(t.tx2).child("Until they run, the login keeps its old grants. Dataman already applies the new rules on top, so nothing new is exposed meanwhile."))
            .child(div().pt(px(8.)).border_t_1().border_color(t.warn).flex().flex_col().gap(px(4.)).font_family(mono()).text_size(px(11.)).children(lines))
            .children(button.map(|b| div().flex().flex_row().child(b)))
            .into_any_element()
    }

    pub(crate) fn login_sheet(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.login.sheet.as_ref()?;
        let close = cx.listener(|v, _: &ClickEvent, _, cx| {
            if !matches!(v.login.sheet, Some(Sheet::Create { busy: true, .. } | Sheet::Remove { busy: true, .. })) {
                v.login.sheet = None;
                cx.notify();
            }
        });
        let (title, body, footer) = match sheet {
            Sheet::Create { review, opts, confirm, busy, error } => self.create_sheet(t, p, review.as_ref(), *opts, *confirm, *busy, error.as_deref(), cx),
            Sheet::Remove { plan, confirm, busy, error } => self.remove_sheet(t, p, plan.as_ref(), *confirm, *busy, error.as_deref(), cx),
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
        let warn_box = |text: String| div().border_1().border_color(t.warn).bg(t.warn_s).px(px(12.)).py(px(10.)).line_height(px(18.)).child(text);
        // The options first, then what stands in the way or needs a hand: the SQL beside them.
        let opt_row = |id: &'static str, on: bool, text: String, sub: Option<&str>| {
            div()
                .children(ui::here(&id.into()))
                .id(id)
                .flex()
                .flex_row()
                .gap(px(10.))
                .cursor_pointer()
                .child(div().pt(px(1.)).children(on.then(|| ui::here(&gpui::SharedString::from(format!("{id}-on")).into())).flatten()).child(checkbox(t, on, false)))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.)).child(div().text_color(t.tx).child(text)).children(sub.map(|s| div().text_size(px(11.)).text_color(t.tx3).child(s.to_string()))))
        };
        let (p1, p2) = (p.clone(), p.clone());
        let mut left = div()
            .id("agents-login-side")
            .w(px(340.))
            .flex_none()
            .border_r_1()
            .border_color(t.line)
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .line_height(px(18.))
            .child(ui::caps(t, "LOGIN"))
            .child(opt_row("agents-login-opt-rotate", opts.rotate_days.is_some(), format!("Rotate its password every {} days", agent_login::ROTATE_DAYS), None).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p1, |o| o.rotate_days = if o.rotate_days.is_some() { None } else { Some(agent_login::ROTATE_DAYS) }, cx))));
        // Only where the database itself turns the next session away.
        if agent_login::caps_sessions(p.engine) {
            left = left.child(
                opt_row("agents-login-opt-conns", opts.conn_limit.is_some(), format!("At most {} sessions open at once", agent_login::CONN_LIMIT), Some("The database turns away more, so a runaway agent can't pile up open sessions"))
                    .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.login_option(&p2, |o| o.conn_limit = if o.conn_limit.is_some() { None } else { Some(agent_login::CONN_LIMIT) }, cx))),
            );
        }
        if !plan.not_granted.is_empty() {
            let names: Vec<String> = plan.not_granted.iter().map(|n| n.object.clone()).collect();
            let reason = plan.not_granted[0].reason.clone();
            left = left.child(warn_box(format!(
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
            left = left.child(warn_box(format!(
                "Every login may read {} ({}), the agent login too: the database won't keep {} from agents, only Dataman does. To close it, their owner runs the REVOKE lines marked in the SQL.",
                agent_login::short_list(&names),
                plan.exposed[0].reason,
                if names.len() == 1 { "it" } else { "them" }
            )));
        }
        for l in &plan.lacks {
            left = left.child(div().children(ui::here(&"agents-login-lacks".into())).border_1().border_color(t.err).bg(t.err_s).px(px(12.)).py(px(10.)).text_color(t.err).child(l.clone()));
        }
        for n in &plan.notes {
            left = left.child(div().text_color(t.tx3).child(n.clone()));
        }
        let right = what_runs(t, "agents-login-sql", &plan.shown(), p.engine);
        let body = div().flex_1().min_w_0().flex().flex_row().child(left).child(right).into_any_element();
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

    #[allow(clippy::too_many_arguments)]
    fn remove_sheet(&self, t: &Theme, p: &ConnectionProfile, plan: Option<&Result<Plan, String>>, confirm: bool, busy: bool, error: Option<&str>, cx: &mut Context<Self>) -> (String, AnyElement, AnyElement) {
        let user = p.agent_login.as_ref().map(|l| l.user.clone()).unwrap_or_default();
        let title = format!("Remove {user} from {}", p.name);
        let text = match plan {
            None => "Reading what Dataman made…".to_string(),
            Some(Err(_)) => String::new(),
            Some(Ok(pl)) => pl.shown(),
        };
        let unreachable = matches!(plan, Some(Err(_))) || error.is_some();
        let lead = "Drops the login and only what Dataman made for it (views, their schema or database, its role), revokes what it holds, and forgets its password. Nothing Dataman didn't make is touched. Agents are then refused here until you create a new login or let them use yours.";
        let mut left = div().w(px(340.)).flex_none().border_r_1().border_color(t.line).px(px(20.)).py(px(16.)).flex().flex_col().gap(px(14.)).line_height(px(18.)).child(div().text_color(t.tx2).child(lead));
        if let Some(Err(e)) = plan {
            left = left.child(div().text_color(t.err).child(e.clone()));
        }
        if let Some(e) = error {
            left = left.child(div().text_color(t.err).child(e.to_string()));
        }
        if unreachable {
            left = left
                .child(div().text_color(t.tx2).child("Can't reach the server, or your login can't drop it? Dataman can forget it on its own side: the login stays on the server until an admin drops it."))
                .child(ui::button(t, "agents-login-forget", ui::Btn::Danger).h(px(28.)).cursor_pointer().child("Forget it in Dataman only").on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.login_forget(cx))));
        }
        let right = what_runs(t, "agents-login-remove-sql", &text, p.engine);
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
        let cancel = ui::button(t, "agents-login-remove-cancel", ui::Btn::Ghost).h(px(30.)).cursor_pointer().child("Cancel").on_click(cx.listener(|v, _: &ClickEvent, _, cx| {
            v.login.sheet = None;
            cx.notify();
        }));
        let say = if confirm { div().flex_1().text_color(t.err).child(format!("Production: this drops {user} on {} now.", p.host)) } else { div().flex_1() };
        let footer = ui::modal_footer(t).child(say).child(cancel).child(go).into_any_element();
        (title, body, footer)
    }
}

/// A sheet's "WHAT RUNS": the statements as the editor colours them in the engine's language.
fn what_runs(t: &Theme, id: &'static str, text: &str, engine: dataman_core::Engine) -> AnyElement {
    let lines = crate::editor::highlighted_lines_in(t, text, engine).into_iter().map(|(s, runs)| if s.is_empty() { div().h(px(19.)) } else { div().whitespace_nowrap().child(StyledText::new(s).with_runs(runs)) });
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(div().h(px(40.)).flex_none().px(px(16.)).flex().flex_row().items_center().border_b_1().border_color(t.line).child(div().children(ui::here(&"agents-login-what".into())).child(ui::caps(t, "WHAT RUNS"))))
        .child(div().id(id).flex_1().min_h_0().overflow_scroll().px(px(16.)).py(px(12.)).font_family(mono()).text_size(px(11.5)).line_height(px(19.)).flex().flex_col().children(lines))
        .into_any_element()
}

enum Verified {
    Gone,
    Checked(Vec<live::Check>),
}

/// Logs in as the agent with `pw` and runs Verify's checks; the connection is closed after.
async fn verify_as_agent(core: &Arc<dataman_core::Core>, p: &ConnectionProfile, user: &str, pw: &str, rv: &live::Review) -> Result<Vec<live::Check>, String> {
    let mine = ConnectionProfile { agent_login: None, ..p.clone() };
    let agent = core.connect_agent(&mine, live::agent_profile(p, user), pw.to_string()).await.map_err(|e| format!("cannot log in as {user}: {e:#}"))?;
    let r = live::verify(&agent, &rv.spec, &rv.inventory, &rv.probe).await.map_err(|e| format!("{e:#}"));
    agent.close();
    r
}

fn prod_tag(t: &Theme) -> gpui::Div {
    div().flex_none().h(px(20.)).px(px(7.)).flex().items_center().text_size(px(10.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.warn).bg(t.warn_s).child("PRODUCTION")
}

fn agent_login_count(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}
