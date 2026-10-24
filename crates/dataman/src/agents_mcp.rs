//! Agents › General: whether agents may connect at all, how to connect one, and the agents'
//! settings as a file: exported as it is, imported only after a summary of what would change.

use std::path::PathBuf;
use std::sync::Mutex;

use dataman_core::agents_config::{self, AgentsFile, Change, Plan};
use dataman_mcp::audit::{ApprovalNote, Audit, Decision, Entry, Outcome};
use gpui::{AnyElement, Context, FontWeight, KeyDownEvent, SharedString, Window, div, prelude::*};

use crate::agents::{AgentsPage, Tip, label_tip, seg};
use crate::theme::{Theme, px};
use crate::ui;

/// An agents settings file read and checked: its changes, or why nothing of it can be used.
pub(crate) struct Import {
    pub(crate) name: String,
    pub(crate) file: Option<AgentsFile>,
    pub(crate) plan: Plan,
    pub(crate) errors: Vec<String>,
}

static TEST_SAVE: Mutex<Option<PathBuf>> = Mutex::new(None);
static TEST_OPEN: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The paths the next Export and Import use, without a file panel.
#[cfg(feature = "checks")]
pub(crate) fn set_test_paths(save: Option<PathBuf>, open: Option<PathBuf>) {
    *TEST_SAVE.lock().unwrap() = save;
    *TEST_OPEN.lock().unwrap() = open;
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

impl AgentsPage {
    pub(crate) fn render_general(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let on = self.core.settings.get().mcp_enabled;
        let access = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-mcp-access-tip",
                "AGENT ACCESS",
                Tip::new("Agent access")
                    .lead("Whether AI agents (Claude Code, Claude Desktop, any MCP client) may use Dataman at all.")
                    .item("On", "Each connection then follows its own MCP tier, privacy and limits (Permissions, Privacy).")
                    .item("Off", "Every agent call is refused. The same switch as Settings › MCP."),
            ))
            .child(seg(t, "agents-mcp-access", &[(false, "Off"), (true, "On")], on, false, cx, |v, on, cx| v.edit_settings(|s| s.mcp_enabled = on, cx)));
        let cmd = crate::mcp::stdio_command();
        let copy = cmd.clone();
        let connect = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-mcp-connect-tip",
                "CONNECT AN AGENT",
                Tip::new("Connect an agent").lead("Run this once in a terminal to add Dataman to Claude Code. Other MCP clients start the same command over stdio."),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w_0().px(px(10.)).py(px(6.)).bg(t.field).border_1().border_color(t.line).text_size(px(11.5)).text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().child(cmd))
                    .child(ui::button(t, "agents-mcp-copy", ui::Btn::Secondary).h(px(28.)).child("Copy").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))))),
            );
        let file = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-file-tip",
                "SETTINGS FILE",
                Tip::new("Agents settings file")
                    .lead("General, Sensitive data and each connection's rules for agents, to keep or to carry to another Mac.")
                    .item("In it", "MCP tier, what the agent sees, Visible to agents, sensitive columns, Agents may, limits, allowed statements and YOLO.")
                    .item("Not in it", "Passwords, the mask key, the audit log and the connections themselves (host, user…).")
                    .item("Import", "Checked whole first, then a summary of what would change; nothing changes until you apply it. Connections are found by id, then by name and engine; the others stay as they are.")
                    .note("Imports are logged in Audit."),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(ui::button(t, "agents-export", ui::Btn::Secondary).h(px(28.)).px(px(10.)).child("Export…").on_click(cx.listener(|v, _, _, cx| v.export_settings(cx))))
                    .child(ui::button(t, "agents-import", ui::Btn::Secondary).h(px(28.)).px(px(10.)).child("Import…").on_click(cx.listener(|v, _, window, cx| v.import_settings(window, cx)))),
            )
            .children(self.settings_note.clone().map(|n| div().children(ui::here(&"agents-file-note".into())).text_size(px(11.5)).text_color(t.tx2).child(n)));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "General"))
            .child(
                div()
                    .id("agents-mcp-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(t.line)
                    .px(px(32.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .child(access)
                    .child(div().max_w(px(760.)).child(connect))
                    .child(file),
            )
            .into_any_element()
    }

    pub(crate) fn export_settings(&mut self, cx: &mut Context<Self>) {
        let text = agents_config::to_text(&agents_config::export(&self.core.settings.get(), &self.core.config.connections()));
        let test = TEST_SAVE.lock().unwrap().take();
        let rx = test.is_none().then(|| cx.prompt_for_new_path(&home(), Some("dataman-agents.toml")));
        cx.spawn(async move |this, cx| {
            let path = match (test, rx) {
                (Some(p), _) => Some(p),
                (None, Some(rx)) => crate::sql_picker::picked(rx.await, cx),
                _ => None,
            };
            let Some(path) = path else { return };
            let note = match std::fs::write(&path, text) {
                Ok(()) => format!("Exported to {}.", path.display()),
                Err(e) => format!("Could not write {}: {e}", path.display()),
            };
            let _ = this.update(cx, |v, cx| {
                v.settings_note = Some(note);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn import_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let test = TEST_OPEN.lock().unwrap().take();
        let rx = test.is_none().then(|| cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: None }));
        cx.spawn_in(window, async move |this, cx| {
            let path = match (test, rx) {
                (Some(p), _) => Some(p),
                (None, Some(rx)) => crate::sql_picker::picked(rx.await, cx).and_then(|p| p.into_iter().next()),
                _ => None,
            };
            let Some(path) = path else { return };
            let text = agents_config::read_file(&path);
            let _ = this.update_in(cx, |v, window, cx| {
                v.open_import(&path, text, cx);
                window.focus(&v.import_focus, cx);
            });
        })
        .detach();
    }

    pub(crate) fn open_import(&mut self, path: &std::path::Path, text: std::io::Result<String>, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let parsed = text.map_err(|e| vec![format!("Could not read the file: {e}")]).and_then(|t| agents_config::parse(&t));
        let (mut file, mut errors) = match parsed {
            Ok(f) => (Some(f), Vec::new()),
            Err(e) => (None, e),
        };
        let plan = file.as_ref().map(|f| agents_config::plan(&self.core.settings.get(), &self.core.config.connections(), f)).unwrap_or_default();
        // Two entries for one connection here: refused whole, like any other error.
        if !plan.problems.is_empty() {
            (file, errors) = (None, plan.problems.clone());
        }
        self.import = Some(Import { name, file, plan, errors });
        self.menu = None;
        cx.notify();
    }

    pub(crate) fn close_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.import = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Writes the file's settings (all or nothing) and logs it in Audit as the user's own act.
    pub(crate) fn apply_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Import { name, file: Some(f), plan, .. }) = self.import.take() else { return };
        let started = std::time::Instant::now();
        // Written only as the summary the user just reviewed shows it.
        let r = agents_config::import(&self.core.config, &self.core.settings, &f, &plan);
        let user = dataman_mcp::audit::local_user();
        let (outcome, error, query, reasons) = match &r {
            Ok(p) => {
                let loose: Vec<String> = p.conns.iter().flat_map(|c| c.changes.iter().filter(|x| x.loosens).map(move |x| format!("{} › {}", c.name, x.what))).chain(p.general.iter().chain(&p.types).filter(|x| x.loosens).map(|x| x.what.clone())).take(20).collect();
                (Outcome::Ok, None, format!("Imported agents settings from {name}: {} changes, {} loosening protection", p.changes(), loose.len()), loose)
            }
            Err(e) => (Outcome::Error, Some(format!("{e:#}")), format!("Import of agents settings from {name}"), Vec::new()),
        };
        Audit::new(self.core.dirs.mcp_audit_file()).write(&Entry {
            id: None,
            ts: chrono::Utc::now(),
            client: "Dataman".into(),
            transport: "app".into(),
            tool: "import_settings".into(),
            connection: None,
            database: None,
            sql: None,
            query: Some(query),
            rows: None,
            ms: started.elapsed().as_secs_f64() * 1000.,
            outcome,
            error,
            approval: Some(ApprovalNote { decision: Decision::Approved, by: Some(user), reasons, rows: None }),
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
        });
        self.settings_note = Some(match r {
            Ok(p) => format!("Imported {name}: {} {}.", p.changes(), if p.changes() == 1 { "change" } else { "changes" }),
            Err(e) => format!("Nothing was imported: {e:#}"),
        });
        self.store.update(cx, |s, cx| s.reload(cx));
        self.reload(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn change_row(t: &Theme, k: usize, c: &Change) -> AnyElement {
        div()
            .children(ui::here(&SharedString::from(format!("agents-import-change-{k}")).into()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .py(px(4.))
            .border_b_1()
            .border_color(t.line)
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(c.what.trim_start_matches("Sensitive data › ").to_string()))
            .child(div().flex_none().max_w(px(150.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx3).child(c.from.clone()))
            .child(div().flex_none().text_color(t.tx3).child("→"))
            .child(div().flex_none().max_w(px(150.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if c.loosens { t.warn } else { t.tx2 }).child(c.to.clone()))
            .child(div().flex_none().w(px(64.)).text_size(px(10.5)).text_color(t.warn).child(if c.loosens { "loosens" } else { "" }))
            .into_any_element()
    }

    /// The import's summary, as the Allow sheet asks: changes, what loosens protection flagged,
    /// connections not on this Mac; Cancel is the default when anything loosens.
    pub(crate) fn render_import(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let imp = self.import.as_ref()?;
        let loosens = imp.plan.loosens();
        let ok = imp.errors.is_empty();
        let caps = |s: &str| ui::caps(t, s.to_string());
        let mut body = div().flex().flex_col().gap(px(14.));
        let mut k = 0;
        if !ok {
            body = body.child(div().text_color(t.err).child("This file can't be imported, so nothing was changed:")).child(
                div().children(ui::here(&"agents-import-errors".into())).flex().flex_col().gap(px(4.)).children(imp.errors.iter().map(|e| div().text_color(t.tx2).child(format!("· {e}")))),
            );
        } else if imp.plan.changes() == 0 && imp.plan.conns.iter().all(|c| !c.by_name) {
            body = body.child(div().text_color(t.tx2).child("Nothing to change: these settings are already here."));
        } else {
            let n = imp.plan.changes();
            let loose = imp.plan.general.iter().chain(&imp.plan.types).chain(imp.plan.conns.iter().flat_map(|c| &c.changes)).filter(|c| c.loosens).count();
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(div().text_color(t.tx).child(format!("{n} {}", if n == 1 { "change" } else { "changes" })))
                    .when(loose > 0, |d| d.child(div().children(ui::here(&"agents-import-loosens".into())).text_color(t.warn).child(format!("{loose} loosen what agents get")))),
            );
            let mut section = |title: String, tag: Option<&str>, changes: &[Change]| {
                if changes.is_empty() && tag.is_none() {
                    return;
                }
                let rows: Vec<AnyElement> = changes
                    .iter()
                    .map(|c| {
                        k += 1;
                        Self::change_row(t, k - 1, c)
                    })
                    .collect();
                body = std::mem::replace(&mut body, div()).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().flex().flex_row().gap(px(8.)).child(caps(&title)).children(tag.map(|s| div().text_size(px(10.5)).text_color(t.tx3).child(s.to_string()))))
                        .children(rows),
                );
            };
            section("GENERAL".into(), None, &imp.plan.general);
            section("SENSITIVE DATA".into(), None, &imp.plan.types);
            for c in &imp.plan.conns {
                section(c.name.to_uppercase(), c.by_name.then_some("found by name"), &c.changes);
            }
            if !imp.plan.not_found.is_empty() {
                body = body.child(
                    div()
                        .children(ui::here(&"agents-import-missing".into()))
                        .text_size(px(11.5))
                        .text_color(t.tx3)
                        .child(format!("Not on this Mac, left out: {}.", imp.plan.not_found.join(", "))),
                );
            }
        }
        let cancel_first = loosens || !ok;
        let key = |s: &str| ui::keycap(t, s.to_string());
        let cancel = ui::button(t, "agents-import-cancel", if cancel_first { ui::Btn::Primary } else { ui::Btn::Secondary })
            .h(px(28.))
            .px(px(10.))
            .gap(px(8.))
            .child("Cancel")
            .child(key(if cancel_first { "↩" } else { "esc" }))
            .on_click(cx.listener(|v, _, window, cx| v.close_import(window, cx)));
        let apply = ok.then(|| {
            ui::button(t, "agents-import-apply", if loosens { ui::Btn::Danger } else { ui::Btn::Primary })
                .h(px(28.))
                .px(px(10.))
                .gap(px(8.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(if loosens { "Apply anyway" } else { "Apply" })
                .child(key(if loosens { "⌘↩" } else { "↩" }))
                .on_click(cx.listener(|v, _, window, cx| v.apply_import(window, cx)))
        });
        Some(
            div()
                .id("agents-import-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .track_focus(&self.import_focus)
                .on_key_down(cx.listener(move |v, ev: &KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    match k.key.as_str() {
                        "escape" => v.close_import(window, cx),
                        "enter" if k.modifiers.platform && ok => v.apply_import(window, cx),
                        "enter" if cancel_first => v.close_import(window, cx),
                        "enter" => v.apply_import(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(t, "agents-import-card")
                        .w(px(680.))
                        .child(ui::modal_header(t, None, format!("Import {}", imp.name), ui::modal_close(t, "agents-import-close").on_click(cx.listener(|v, _, window, cx| v.close_import(window, cx)))))
                        .child(div().id("agents-import-body").max_h(px(460.)).overflow_y_scroll().px(px(20.)).py(px(16.)).child(body))
                        .child(
                            ui::modal_footer(t)
                                .text_size(px(11.))
                                .gap(px(16.))
                                .child(div().flex_1().min_w_0().text_color(t.tx3).child(if loosens { "Some changes give agents more. Check them before applying." } else { "Applied all at once, and logged in Audit." }))
                                .child(div().flex_none().flex().flex_row().gap(px(8.)).child(cancel).children(apply)),
                        ),
                )
                .into_any_element(),
        )
    }
}
