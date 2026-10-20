use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, FontWeight, MouseButton, Pixels, ScrollHandle, SharedString,
    Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::format::KeywordCase;
use dataman_core::{Core, Settings, ThemePref, Translucency, UiFont};

use crate::assets::{Icon, icon};
use crate::input::{InputEvent, TextInput};
use crate::theme::{self, ActiveTheme, Theme, metrics};
use crate::ui;

actions!(settings_sheet, [Close, NextSection, PrevSection]);

const CONTEXT: &str = "SettingsSheet";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("escape", Close, c),
        crate::keymap::kb("down", NextSection, c),
        crate::keymap::kb("up", PrevSection, c),
    ]);
}

pub enum SettingsEvent {
    Close,
    ConnectionsChanged,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Appearance,
    Editor,
    Grid,
    Export,
    Mcp,
    Keyboard,
    About,
}

const SECTIONS: [Section; 7] =
    [Section::Appearance, Section::Editor, Section::Grid, Section::Export, Section::Mcp, Section::Keyboard, Section::About];

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Editor => "Editor",
            Section::Grid => "Results grid",
            Section::Export => "Export",
            Section::Mcp => "MCP",
            Section::Keyboard => "Keyboard",
            Section::About => "About",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Theme,
    Font,
    Translucency,
    FontSize,
    ReduceMotion,
    DbLogos,
    Panels,
    Prefetch,
    PageSize,
    ConfirmCommit,
    ExportDir,
    KeywordCase,
    Indent,
    McpAccess,
    McpTabs,
    McpWriteLimit,
    McpRows,
    McpClaude,
    McpLog,
    McpConnections,
    McpAdvanced,
    Version,
    UpdateCheck,
    CrashReports,
}

const ROWS: [(Row, Section, &str, &str); 24] = [
    (Row::Theme, Section::Appearance, "Theme", "dark light system"),
    (Row::Font, Section::Appearance, "Font", "typeface family monospace geist lilex zed jetbrains"),
    (Row::Translucency, Section::Appearance, "Window translucency", "transparency blur opaque"),
    (Row::FontSize, Section::Appearance, "Code font size", "editor text"),
    (Row::ReduceMotion, Section::Appearance, "Reduce motion", "animation accessibility"),
    (Row::DbLogos, Section::Appearance, "Database logos", "icon engine postgres mysql mongodb redis elasticsearch opensearch explorer picker"),
    (Row::Panels, Section::Appearance, "Panels", "panel layout dock explorer history favorites move reset"),
    (Row::KeywordCase, Section::Editor, "Keyword case", "format formatter sql upper lower"),
    (Row::Indent, Section::Editor, "Indent", "format formatter spaces tab"),
    (Row::Prefetch, Section::Grid, "Rows fetched ahead", "prefetch fetch"),
    (Row::PageSize, Section::Grid, "Rows per table page", "page paging table data view"),
    (Row::ConfirmCommit, Section::Grid, "Review edits before commit", "commit edit sql"),
    (Row::ExportDir, Section::Export, "Default export folder", "downloads directory"),
    (Row::McpAccess, Section::Mcp, "Agent access", "mcp ai agents claude"),
    (Row::McpTabs, Section::Mcp, "Agent queries in tabs", "mcp visible open query tab agent"),
    (Row::McpWriteLimit, Section::Mcp, "Rows per write", "mcp limit approval blast radius write rows"),
    (Row::McpRows, Section::Mcp, "Rows per call", "mcp limit max rows"),
    (Row::McpClaude, Section::Mcp, "Claude Code", "mcp add command stdio"),
    (Row::McpLog, Section::Mcp, "Log", "mcp audit calls"),
    (Row::McpConnections, Section::Mcp, "Connections", "mcp permission tier off read write full"),
    (Row::McpAdvanced, Section::Mcp, "Advanced", "mcp advanced allowed queries allowlist yolo"),
    (Row::Version, Section::About, "Version", "about update release"),
    (Row::UpdateCheck, Section::About, "Check for updates", "update automatic daily feed"),
    (Row::CrashReports, Section::About, "Crash reports", "crash logs panic diagnostics"),
];

impl Row {
    fn help(self) -> Option<&'static str> {
        Some(match self {
            Row::McpAccess => "Lets AI agents (Claude Code, Claude Desktop, any MCP client) use your connections. Off blocks every agent.",
            Row::McpTabs => "Shows the agent's queries and their results in a tab of their own, so you see what it runs.",
            Row::McpWriteLimit => "An agent's write changing more rows than this waits for your Allow before it commits.",
            Row::McpRows => "The most rows an agent receives per query. The full result stays in its tab.",
            Row::McpClaude => "Copies the command that adds Dataman to Claude Code.",
            _ => return None,
        })
    }
}

const MCP_TIERS: [(&str, &str); 4] = [
    ("Off", "no access"),
    ("Read", "queries only, in a read-only transaction"),
    ("Write", "inserts, and updates or deletes with a real WHERE; anything destructive is refused"),
    ("Full", "anything; destructive statements wait for your Allow"),
];

pub const MCP_ROW_CHOICES: [(usize, &str); 4] = [(200, "200"), (1_000, "1k"), (5_000, "5k"), (10_000, "10k")];
pub const MCP_WRITE_CHOICES: [(u64, &str); 4] = [(10, "10"), (100, "100"), (1_000, "1k"), (10_000, "10k")];

pub const PREFETCH_CHOICES: [(usize, &str); 4] = [(5_000, "5k"), (10_000, "10k"), (50_000, "50k"), (100_000, "100k")];
pub const PAGE_CHOICES: [(usize, &str); 4] = [(200, "200"), (1_000, "1k"), (5_000, "5k"), (10_000, "10k")];

const MAX_SHOWN_FONTS: usize = 8;

fn rank_fonts<'a>(query: &str, fonts: &'a [String], max: usize) -> Vec<&'a str> {
    let mut matches: Vec<(i32, &str)> =
        fonts.iter().filter_map(|n| crate::palette::fuzzy(query, n).map(|m| (m.score, n.as_str()))).collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    matches.truncate(max);
    matches.into_iter().map(|(_, n)| n).collect()
}

pub struct SettingsView {
    core: Arc<Core>,
    focus: FocusHandle,
    search: gpui::Entity<TextInput>,
    section: Section,
    settings: Settings,
    copied: Option<&'static str>,
    _copied_timer: Option<gpui::Task<()>>,
    system_font_open: bool,
    system_font_query: gpui::Entity<TextInput>,
    system_fonts: Option<Arc<[String]>>,
    recording: Option<&'static str>,
    key_bounds: Rc<RefCell<BTreeMap<&'static str, Vec<Bounds<Pixels>>>>>,
    reset_all_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    key_search: bool,
    key_query: Option<String>,
    key_button_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    check_now_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    page_scroll: ScrollHandle,
    mcp_advanced: bool,
    allow_inputs: BTreeMap<String, gpui::Entity<TextInput>>,
    allow_errors: BTreeMap<String, String>,
    yolo_confirm: Option<String>,
    _intercept: gpui::Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsView {
    pub fn new(core: Arc<Core>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search settings").bare());
        cx.subscribe(&search, |v: &mut Self, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Changed) {
                v.scroll_to_top();
                cx.notify();
            }
        })
        .detach();
        let system_font_query = cx.new(|cx| TextInput::new(cx, "Filter installed fonts").bare());
        cx.subscribe(&system_font_query, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Changed) {
                cx.notify();
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&search.focus_handle(cx), cx);
        theme::ensure_fonts(&UiFont::ALL, cx);
        let settings = core.settings.get();
        let weak = cx.weak_entity();
        let intercept = cx.intercept_keystrokes(move |ev, window, cx| {
            let Some(v) = weak.upgrade() else { return };
            let (recording, key_search) = (v.read(cx).recording.is_some(), v.read(cx).key_search);
            if recording {
                v.update(cx, |v, cx| v.record(&ev.keystroke, cx));
                cx.stop_propagation();
            } else if key_search {
                v.update(cx, |v, cx| v.search_keystroke(&ev.keystroke, window, cx));
                cx.stop_propagation();
            }
        });
        SettingsView {
            core,
            focus,
            search,
            section: Section::Appearance,
            settings,
            copied: None,
            _copied_timer: None,
            system_font_open: false,
            system_font_query,
            system_fonts: None,
            recording: None,
            key_bounds: Default::default(),
            reset_all_bounds: Default::default(),
            key_search: false,
            key_query: None,
            key_button_bounds: Default::default(),
            check_now_bounds: Default::default(),
            page_scroll: ScrollHandle::new(),
            mcp_advanced: false,
            allow_inputs: BTreeMap::new(),
            allow_errors: BTreeMap::new(),
            yolo_confirm: None,
            _intercept: intercept,
        }
    }

    fn scroll_to_top(&self) {
        self.page_scroll.set_offset(gpui::point(px(0.), px(0.)));
    }

    fn set_key_search(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.key_search = on;
        self.key_query = None;
        self.recording = None;
        self.search.update(cx, |i, cx| i.set_text("", cx));
        let target = if on { self.focus.clone() } else { self.search.focus_handle(cx) };
        window.focus(&target, cx);
        self.scroll_to_top();
        cx.notify();
    }

    fn search_keystroke(&mut self, k: &gpui::Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let m = k.modifiers;
        let bare = !(m.control || m.alt || m.platform || m.shift);
        match k.key.as_str() {
            "escape" if bare => return self.set_key_search(false, window, cx),
            "backspace" | "delete" if bare => self.key_query = None,
            _ => {
                let mods = gpui::Modifiers { function: false, ..m };
                match crate::keymap::parse(&gpui::Keystroke { modifiers: mods, key: k.key.clone(), key_char: None }.unparse()) {
                    Some(s) => self.key_query = Some(s),
                    None => return,
                }
            }
        }
        self.scroll_to_top();
        cx.notify();
    }

    fn start_recording(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = if self.recording == Some(id) { None } else { Some(id) };
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn record(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(id) = self.recording else { return };
        let m = k.modifiers;
        let bare = !(m.control || m.alt || m.platform || m.shift);
        let named = k.key.chars().count() > 1;
        let fkey = k.key.len() > 1 && k.key.starts_with('f') && k.key[1..].chars().all(|c| c.is_ascii_digit());
        match k.key.as_str() {
            "escape" if bare => {}
            "backspace" | "delete" if bare => self.set_key(id, Some(String::new()), cx),
            _ if m.control || m.alt || m.platform || fkey || (m.shift && named) => {
                let mods = gpui::Modifiers { function: false, ..m };
                match crate::keymap::parse(&gpui::Keystroke { modifiers: mods, key: k.key.clone(), key_char: None }.unparse()) {
                    Some(s) => self.set_key(id, Some(s), cx),
                    None => return,
                }
            }
            _ => return,
        }
        self.recording = None;
        cx.notify();
    }

    fn set_key(&mut self, id: &str, key: Option<String>, cx: &mut Context<Self>) {
        let mut keys = crate::keymap::overrides();
        match (id, key) {
            ("", _) => keys.clear(),
            (id, Some(k)) => {
                keys.insert(id.to_string(), k);
            }
            (id, None) => {
                keys.remove(id);
            }
        }
        let keys = crate::keymap::clean(&keys);
        let saved = keys.clone();
        self.save(move |s| s.keys = saved, cx);
        crate::keymap::apply(keys, cx);
    }

    fn set_mcp(&mut self, f: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        self.save(f, cx);
        crate::mcp::apply(&self.core, cx);
    }

    fn copy(&mut self, what: &'static str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.copied = Some(what);
        self._copied_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let _ = this.update(cx, |v, cx| {
                v.copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_conn_mcp(&mut self, id: &str, access: dataman_core::McpAccess, cx: &mut Context<Self>) {
        if let Some(mut p) = self.core.config.get(id) {
            p.mcp = Some(access);
            if let Err(e) = self.core.config.upsert(p) {
                log::warn!("could not save the connection: {e:#}");
            }
            cx.emit(SettingsEvent::ConnectionsChanged);
            cx.notify();
        }
    }

    fn edit_conn(&mut self, id: &str, f: impl FnOnce(&mut dataman_core::ConnectionProfile), cx: &mut Context<Self>) {
        if let Some(mut p) = self.core.config.get(id) {
            f(&mut p);
            if let Err(e) = self.core.config.upsert(p) {
                log::warn!("could not save the connection: {e:#}");
            }
            cx.emit(SettingsEvent::ConnectionsChanged);
            cx.notify();
        }
    }

    fn toggle_mcp_advanced(&mut self, cx: &mut Context<Self>) {
        self.mcp_advanced = !self.mcp_advanced;
        self.yolo_confirm = None;
        if self.mcp_advanced {
            for p in self.core.config.connections() {
                if self.allow_inputs.contains_key(&p.id) {
                    continue;
                }
                let input = cx.new(|cx| TextInput::new(cx, "DELETE FROM sessions WHERE expires_at < ?"));
                let id = p.id.clone();
                cx.subscribe(&input, move |v: &mut Self, _, ev: &InputEvent, cx| match ev {
                    InputEvent::Submit => v.add_allowed(&id, cx),
                    InputEvent::Changed => {
                        if v.allow_errors.remove(&id).is_some() {
                            if let Some(i) = v.allow_inputs.get(&id) {
                                i.update(cx, |i, _| i.error = false);
                            }
                            cx.notify();
                        }
                    }
                })
                .detach();
                self.allow_inputs.insert(p.id, input);
            }
        }
        cx.notify();
    }

    fn add_allowed(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some(p), Some(input)) = (self.core.config.get(id), self.allow_inputs.get(id).cloned()) else { return };
        let d = p.engine.dialect();
        let text = input.read(cx).text().to_string();
        let r = dataman_core::guard::allowlist_entry(&text, d).and_then(|e| {
            if p.mcp_allowed.iter().any(|a| dataman_core::guard::allowlist_match(a, &e, d)) { Err("already allowed".to_string()) } else { Ok(e) }
        });
        match r {
            Ok(e) => {
                self.allow_errors.remove(id);
                input.update(cx, |i, cx| {
                    i.set_text("", cx);
                    i.error = false;
                });
                self.edit_conn(id, |p| p.mcp_allowed.push(e), cx);
            }
            Err(e) => {
                self.allow_errors.insert(id.to_string(), e);
                input.update(cx, |i, _| i.error = true);
                cx.notify();
            }
        }
    }

    fn toggle_yolo(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(p) = self.core.config.get(id) else { return };
        if p.mcp_yolo || self.yolo_confirm.as_deref() == Some(id) {
            self.yolo_confirm = None;
            self.edit_conn(id, |p| p.mcp_yolo = false, cx);
        } else if p.env.is_production() {
            self.yolo_confirm = Some(id.to_string());
            cx.notify();
        } else {
            self.edit_conn(id, |p| p.mcp_yolo = true, cx);
        }
    }

    fn row_visible(&self, row: Row, cx: &App) -> bool {
        match row {
            Row::McpTabs | Row::McpWriteLimit | Row::McpRows | Row::McpClaude | Row::McpConnections => self.settings.mcp_enabled,
            Row::McpAdvanced => dataman_core::MCP_ADVANCED && self.settings.mcp_enabled,
            Row::UpdateCheck => crate::updates::enabled(cx),
            _ => true,
        }
    }

    fn copy_button(&self, t: &Theme, id: &'static str, label: &'static str, text: Option<String>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let done = self.copied == Some(id);
        let enabled = text.is_some();
        ui::button(t, id, ui::Btn::Secondary)
            .h(px(28.))
            .when(!enabled, |d| d.opacity(0.45))
            .child(if done { "Copied" } else { label })
            .when_some(text, |d, text| d.on_click(cx.listener(move |v, _, _, cx| v.copy(id, text.clone(), cx))))
            .into_any_element()
    }

    fn render_mcp_connections(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let profiles = self.core.config.connections();
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .pb(px(14.))
            .border_b_1()
            .border_color(t.line)
            .child(div().pb(px(6.)).child(ui::caps(t, "CONNECTIONS")))
            .child(
                div().pb(px(8.)).flex().flex_col().gap(px(2.)).text_size(px(10.5)).line_height(px(15.)).children(MCP_TIERS.map(|(name, what)| {
                    div().flex().flex_row().gap(px(8.)).child(div().w(px(44.)).flex_none().text_color(t.tx2).child(name)).child(div().min_w_0().text_color(t.tx3).child(what))
                })),
            )
            .child(div().pb(px(8.)).text_size(px(10.5)).line_height(px(15.)).text_color(t.tx3).child("On production, every write waits for your Allow. A read-only database refuses all writes."))
            .children(profiles.into_iter().enumerate().map(|(i, p)| {
                let cur = p.mcp_access();
                let env = p.env;
                let id = p.id.clone();
                let on = cur != dataman_core::McpAccess::Off;
                let allowed = match p.mcp_allowed_on().len() {
                    0 => None,
                    1 => Some("1 allowed query".to_string()),
                    n => Some(format!("{n} allowed queries")),
                };
                div()
                    .h(px(36.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(ui::chip(t.profile_color(&p), 8.))
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                    .when(!env.is_none(), |d| d.child(div().flex_none().text_size(px(11.)).text_color(if env.is_production() { t.err } else { t.tx3 }).child(env.label())))
                    .child(div().flex_1())
                    .children(allowed.filter(|_| on).map(|a| div().flex_none().text_size(px(11.)).text_color(t.tx3).whitespace_nowrap().child(a)))
                    .when(p.mcp_yolo_on(), |d| d.child(crate::conn_dialog::yolo_badge(t)))
                    .child(ui::segmented(t).w(px(232.)).children(dataman_core::McpAccess::ALL.into_iter().map(|a| {
                        let id = id.clone();
                        ui::segment(t, ("mcp-conn", i * 4 + a as usize), a.label(), cur == a)
                            .when(a >= dataman_core::McpAccess::Write && cur == a && env.is_production(), |d| d.text_color(t.err))
                            .on_click(cx.listener(move |v, _, _, cx| v.set_conn_mcp(&id, a, cx)))
                    })))
            }))
            .into_any_element()
    }

    fn render_mcp_advanced(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let profiles: Vec<_> = self.core.config.connections().into_iter().filter(|p| p.mcp_access() != dataman_core::McpAccess::Off).collect();
        let open = self.mcp_advanced;
        let allowed: usize = profiles.iter().map(|p| p.mcp_allowed.len()).sum();
        let yolos = profiles.iter().filter(|p| p.mcp_yolo_on()).count();
        let summary = match allowed {
            0 => None,
            1 => Some("1 allowed query".to_string()),
            n => Some(format!("{n} allowed queries")),
        };
        let head = div()
            .id("mcp-advanced")
            .h(px(20.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .child(ui::disclosure("mcp-advanced-chev", open, 11., t.tx3))
            .child(ui::caps(t, "ADVANCED"))
            .when(!open, |d| d.children(summary.map(|s| div().text_size(px(10.5)).text_color(t.tx3).child(s))))
            .when(!open && yolos > 0, |d| d.child(crate::conn_dialog::yolo_badge(t)))
            .on_click(cx.listener(|v, _, _, cx| v.toggle_mcp_advanced(cx)));
        let mut col = div().flex().flex_col().gap(px(10.)).pb(px(14.)).border_b_1().border_color(t.line).child(head);
        if !open {
            return col.into_any_element();
        }
        let note = |text: &'static str| div().text_size(px(10.5)).line_height(px(15.)).text_color(t.tx3).child(text);
        col = col.child(div().pl(px(17.)).child(note(
            "Allowed queries run without asking, whatever the tier (never while it is off or the database is read-only). Each is one statement, matched statement by statement: the same words in the same order, case, spacing and comments aside; a ? stands for exactly one value, never for more SQL.",
        )));
        if profiles.is_empty() {
            return col.child(div().pl(px(17.)).child(note("No connection gives agents access.")).into_any_element()).into_any_element();
        }
        for (i, p) in profiles.into_iter().enumerate() {
            let id = p.id.clone();
            let rows = p.mcp_allowed.iter().enumerate().map(|(k, s)| {
                let id = id.clone();
                div()
                    .min_h(px(26.))
                    .pl(px(10.))
                    .pr(px(2.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .bg(t.card)
                    .border_1()
                    .border_color(t.line)
                    .child(div().flex_1().min_w_0().py(px(4.)).text_size(px(11.5)).text_color(t.tx2).child(s.clone()))
                    .child(ui::icon_button(t, ("allowed-rm", i * 1000 + k), Icon::Close, 22., 11.).on_click(cx.listener(move |v, _, _, cx| {
                        v.edit_conn(&id, |p| {
                            if k < p.mcp_allowed.len() {
                                p.mcp_allowed.remove(k);
                            }
                        }, cx)
                    })))
            });
            let add = self.allow_inputs.get(&p.id).cloned().map(|input| {
                let id = id.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(8.))
                    .child(div().flex_1().child(input))
                    .child(ui::button(t, ("allowed-add", i), ui::Btn::Secondary).h(px(28.)).px(px(10.)).child("Add").on_click(cx.listener(move |v, _, _, cx| v.add_allowed(&id, cx))))
            });
            let yolo = p.mcp_yolo;
            let confirming = self.yolo_confirm.as_deref() == Some(p.id.as_str());
            let switch = div()
                .flex_none()
                .w(px(26.))
                .h(px(14.))
                .relative()
                .bg(if yolo { t.err } else { t.line2 })
                .child(div().absolute().top(px(2.)).when(yolo, |d| d.right(px(2.))).when(!yolo, |d| d.left(px(2.))).size(px(10.)).rounded(px(5.)).bg(if yolo { gpui::white() } else { t.tx3 }));
            let yolo_row = {
                let id = id.clone();
                div()
                    .id(("yolo", i))
                    .px(px(10.))
                    .py(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .border_1()
                    .border_color(if yolo || confirming { t.err } else { t.line2 })
                    .when(yolo, |d| d.bg(t.err_s))
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| v.toggle_yolo(&id, cx)))
                    .child(switch)
                    .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.err).child("YOLO"))
                    .child(div().flex_1().min_w_0().text_size(px(11.)).text_color(t.tx2).child("agents run anything here, nobody asked (read-only still refuses writes)"))
            };
            let confirm = confirming.then(|| {
                let (yes, no) = (id.clone(), id.clone());
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .bg(t.err_s)
                    .border_1()
                    .border_color(t.err)
                    .child(div().flex_1().min_w_0().text_size(px(11.5)).line_height(px(17.)).text_color(t.tx).child("This connection is production: with YOLO, an agent can change or destroy its data and nobody is asked first."))
                    .child(ui::button(t, ("yolo-cancel", i), ui::Btn::Ghost).h(px(26.)).child("Cancel").on_click(cx.listener(move |v, _, _, cx| {
                        if v.yolo_confirm.as_deref() == Some(no.as_str()) {
                            v.yolo_confirm = None;
                            cx.notify();
                        }
                    })))
                    .child(ui::button(t, ("yolo-confirm", i), ui::Btn::Danger).h(px(26.)).font_weight(FontWeight::SEMIBOLD).child("Turn On YOLO").on_click(cx.listener(move |v, _, _, cx| {
                        v.yolo_confirm = None;
                        v.edit_conn(&yes, |p| p.mcp_yolo = true, cx);
                    })))
            });
            let env = p.env;
            col = col.child(
                div()
                    .pl(px(17.))
                    .pt(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .h(px(20.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(ui::chip(t.profile_color(&p), 8.))
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                            .when(!env.is_none(), |d| d.child(div().flex_none().text_size(px(11.)).text_color(if env.is_production() { t.err } else { t.tx3 }).child(env.label()))),
                    )
                    .children(rows)
                    .children(add)
                    .children(self.allow_errors.get(&p.id).cloned().map(|e| div().text_size(px(11.)).text_color(t.err).child(e)))
                    .child(yolo_row)
                    .children(confirm),
            );
        }
        col.into_any_element()
    }

    pub fn show_update_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.key_search {
            self.set_key_search(false, window, cx);
        }
        self.search.update(cx, |i, cx| i.set_text("", cx));
        if self.section != Section::About {
            self.section = Section::About;
            self.scroll_to_top();
        }
        cx.notify();
    }

    fn update_status(cx: &App) -> Option<(SharedString, gpui::Hsla, bool)> {
        use crate::updates::State;
        let t = cx.theme();
        match crate::updates::state(cx) {
            State::Checking => Some(("◌ Checking…".into(), t.tx3, false)),
            s if s.feed().is_some() => s.feed().map(|f| (format!("{} available", f.version).into(), t.acc, false)),
            _ => crate::updates::checked(cx).map(|c| match c.error {
                None => (format!("Dataman is up to date · checked {}", crate::updates::when(c.at)).into(), t.tx3, false),
                Some(e) => (format!("Check failed: {e}").into(), t.err, true),
            }),
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_mcp_advanced(&mut self, cx: &mut Context<Self>) {
        if !self.mcp_advanced {
            self.toggle_mcp_advanced(cx);
        }
    }

    pub fn show_section(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(s) = SECTIONS.iter().find(|s| s.label().to_lowercase().starts_with(&name.to_lowercase())) {
            self.section = *s;
            cx.notify();
        }
    }

    fn save(&mut self, f: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        if let Err(e) = self.core.settings.update(f) {
            log::warn!("could not save the settings: {e:#}");
        }
        self.settings = self.core.settings.get();
        cx.notify();
    }

    fn set_theme(&mut self, pref: ThemePref, cx: &mut Context<Self>) {
        theme::set_pref(pref, cx);
        self.settings = self.core.settings.get();
        cx.notify();
    }

    fn set_translucency(&mut self, level: Translucency, window: &mut Window, cx: &mut Context<Self>) {
        theme::set_translucency(level, window, cx);
        self.settings = self.core.settings.get();
        cx.notify();
    }

    fn set_font(&mut self, font: UiFont, cx: &mut Context<Self>) {
        theme::set_font(font, cx);
        self.settings = self.core.settings.get();
        self.system_font_open = false;
        cx.notify();
    }

    fn toggle_system_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.system_font_open = !self.system_font_open;
        if self.system_font_open {
            if self.system_fonts.is_none() {
                let embedded: Vec<&str> = UiFont::ALL.iter().map(|f| f.family()).collect();
                let names: Vec<String> = cx
                    .text_system()
                    .all_font_names()
                    .into_iter()
                    .filter(|n| !embedded.contains(&n.as_str()) && n != theme::PIXEL)
                    .collect();
                self.system_fonts = Some(names.into());
            }
            self.system_font_query.update(cx, |i, cx| i.set_text("", cx));
            window.focus(&self.system_font_query.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn set_system_font(&mut self, name: String, cx: &mut Context<Self>) {
        theme::set_system_font(Some(name), cx);
        self.settings = self.core.settings.get();
        self.system_font_open = false;
        cx.notify();
    }

    fn set_font_size(&mut self, pt: u8, cx: &mut Context<Self>) {
        self.save(|s| s.editor_font_size = pt, cx);
        crate::editor::set_code_size(pt);
        cx.refresh_windows();
    }

    fn set_format(&mut self, f: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        self.save(f, cx);
        crate::editor::set_format_options(self.settings.format_options());
    }

    fn set_logos(&mut self, on: bool, cx: &mut Context<Self>) {
        self.save(|s| s.db_logos = on, cx);
        crate::assets::set_logos(on);
        cx.refresh_windows();
    }

    fn set_reduce_motion(&mut self, on: bool, cx: &mut Context<Self>) {
        self.save(|s| s.reduce_motion = on, cx);
        crate::motion::set_reduce_motion_pref(on, cx);
    }

    fn set_update_check(&mut self, on: bool, cx: &mut Context<Self>) {
        self.save(|s| s.update_check = on, cx);
        crate::updates::set_auto(on, cx);
    }

    fn set_prefetch(&mut self, rows: usize, cx: &mut Context<Self>) {
        self.save(|s| s.prefetch_rows = rows, cx);
        dataman_core::result::set_prefetch(rows);
    }

    fn choose_export_dir(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |v, cx| v.save(|s| s.export_dir = Some(p.display().to_string()), cx));
        })
        .detach();
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).text().trim().to_lowercase()
    }

    fn step_section(&mut self, d: isize, cx: &mut Context<Self>) {
        let i = SECTIONS.iter().position(|s| *s == self.section).unwrap_or(0) as isize;
        let n = SECTIONS.len() as isize;
        self.section = SECTIONS[((i + d).rem_euclid(n)) as usize];
        self.scroll_to_top();
        cx.notify();
    }

    fn render_system_font_picker(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let query = self.system_font_query.read(cx).text().to_string();
        let current = self.settings.system_font.clone();
        let matches = rank_fonts(&query, self.system_fonts.as_deref().unwrap_or(&[]), MAX_SHOWN_FONTS);
        ui::popover(t)
            .flex()
            .flex_col()
            .gap(px(4.))
            .w(px(280.))
            .child(
                div()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .bg(t.hov)
                    .border_1()
                    .border_color(t.field_b)
                    .child(icon(Icon::Search).size(px(12.)).text_color(t.tx3))
                    .child(div().flex_1().min_w_0().child(self.system_font_query.clone())),
            )
            .children(matches.into_iter().map(|name| {
                let selected = current.as_deref() == Some(name);
                let picked = name.to_string();
                div()
                    .id(SharedString::from(name.to_string()))
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .font_family(name.to_string())
                    .text_color(if selected { t.tx } else { t.tx2 })
                    .when(selected, |d| d.bg(t.hov))
                    .hover(|d| d.bg(t.hov))
                    .child(name.to_string())
                    .on_click(cx.listener(move |v, _, _, cx| v.set_system_font(picked.clone(), cx)))
            }))
            .into_any_element()
    }

    fn render_row(&self, row: Row, label: &'static str, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = &self.settings;
        let control: gpui::AnyElement = match row {
            Row::Theme => return self.render_themes(t, cx).into_any_element(),
            Row::Font => {
                let cur = theme::ui_font();
                let sys = s.system_font.clone();
                let sys_on = sys.is_some();
                let sys_label: SharedString = sys.clone().unwrap_or_else(|| "System…".into()).into();
                let open = self.system_font_open;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        ui::segmented(t)
                            .children(UiFont::ALL.into_iter().enumerate().map(|(i, f)| {
                                ui::segment(t, ("font", i), f.family(), !sys_on && cur == f)
                                    .font_family(f.family())
                                    .on_click(cx.listener(move |v, _, _, cx| v.set_font(f, cx)))
                            }))
                            .child(
                                ui::segment(t, "font-system", sys_label, sys_on || open)
                                    .when_some(sys.clone(), |d, f| d.font_family(f))
                                    .on_click(cx.listener(|v, _, window, cx| v.toggle_system_fonts(window, cx))),
                            ),
                    )
                    .when(open, |d| d.child(self.render_system_font_picker(t, cx)))
                    .into_any_element()
            }
            Row::Translucency => {
                let on = s.translucency.is_on();
                ui::segmented(t)
                    .child(ui::segment(t, "tr-off", "Off", !on).on_click(cx.listener(|v, _, w, cx| v.set_translucency(Translucency::Off, w, cx))))
                    .child(ui::segment(t, "tr-on", "On", on).on_click(cx.listener(|v, _, w, cx| v.set_translucency(Translucency::On, w, cx))))
                    .into_any_element()
            }
            Row::FontSize => {
                let cur = s.editor_font_size;
                ui::segmented(t)
                    .children([12u8, 13, 14].into_iter().map(|pt| {
                        ui::segment(t, ("fs", pt as usize), pt.to_string(), cur == pt)
                            .on_click(cx.listener(move |v, _, _, cx| v.set_font_size(pt, cx)))
                    }))
                    .into_any_element()
            }
            Row::ReduceMotion => {
                let on = s.reduce_motion;
                let system = crate::motion::system_reduce_motion();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .when(system && !on, |d| d.child(div().text_size(px(11.)).text_color(t.tx3).whitespace_nowrap().child("on in macOS")))
                    .child(
                        ui::segmented(t)
                            .child(ui::segment(t, "rm-off", "Animate", !on).on_click(cx.listener(|v, _, _, cx| v.set_reduce_motion(false, cx))))
                            .child(ui::segment(t, "rm-on", "Reduce", on).on_click(cx.listener(|v, _, _, cx| v.set_reduce_motion(true, cx)))),
                    )
                    .into_any_element()
            }
            Row::DbLogos => {
                let on = s.db_logos;
                ui::segmented(t)
                    .child(ui::segment(t, "logos-off", "Hide", !on).on_click(cx.listener(|v, _, _, cx| v.set_logos(false, cx))))
                    .child(ui::segment(t, "logos-on", "Show", on).on_click(cx.listener(|v, _, _, cx| v.set_logos(true, cx))))
                    .into_any_element()
            }
            Row::Panels => ui::button(t, "reset-panels", ui::Btn::Secondary)
                .h(px(28.))
                .child("Reset panels")
                .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::dock::ResetPanelLayout), cx))
                .into_any_element(),
            Row::KeywordCase => {
                let cur = s.keyword_case;
                ui::segmented(t)
                    .children(KeywordCase::ALL.into_iter().enumerate().map(|(i, k)| {
                        ui::segment(t, ("kc", i), k.label(), cur == k).on_click(cx.listener(move |v, _, _, cx| v.set_format(|s| s.keyword_case = k, cx)))
                    }))
                    .into_any_element()
            }
            Row::Indent => {
                let cur = if s.indent == 4 { 4 } else { 2 };
                ui::segmented(t)
                    .children([2u8, 4].into_iter().map(|n| {
                        ui::segment(t, ("in", n as usize), n.to_string(), cur == n).on_click(cx.listener(move |v, _, _, cx| v.set_format(|s| s.indent = n, cx)))
                    }))
                    .into_any_element()
            }
            Row::Prefetch => {
                let cur = s.prefetch_rows;
                ui::segmented(t)
                    .children(PREFETCH_CHOICES.into_iter().map(|(n, label)| {
                        ui::segment(t, ("pf", n), label, cur == n).on_click(cx.listener(move |v, _, _, cx| v.set_prefetch(n, cx)))
                    }))
                    .into_any_element()
            }
            Row::PageSize => {
                let cur = s.page_size;
                ui::segmented(t)
                    .children(PAGE_CHOICES.into_iter().map(|(n, label)| {
                        ui::segment(t, ("pg", n), label, cur == n).on_click(cx.listener(move |v, _, _, cx| v.save(|s| s.page_size = n, cx)))
                    }))
                    .into_any_element()
            }
            Row::ConfirmCommit => {
                let on = s.confirm_commit;
                ui::segmented(t)
                    .child(ui::segment(t, "cc-on", "Review first", on).on_click(cx.listener(|v, _, _, cx| v.save(|s| s.confirm_commit = true, cx))))
                    .child(ui::segment(t, "cc-off", "Commit at once", !on).on_click(cx.listener(|v, _, _, cx| v.save(|s| s.confirm_commit = false, cx))))
                    .into_any_element()
            }
            Row::ExportDir => {
                let dir: SharedString = s.export_dir.clone().unwrap_or_else(|| dataman_core::os::downloads_dir().map(|d| crate::export_ui::tilde(&d)).unwrap_or_else(|| "~/Downloads".into())).into();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .h(px(28.))
                            .max_w(px(280.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .bg(t.field)
                            .border_1()
                            .border_color(t.field_b)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(dir),
                    )
                    .child(ui::button(t, "ed-choose", ui::Btn::Secondary).h(px(28.)).child("Choose…").on_click(cx.listener(|v, _, _, cx| v.choose_export_dir(cx))))
                    .when(s.export_dir.is_some(), |d| {
                        d.child(ui::button(t, "ed-reset", ui::Btn::Ghost).h(px(28.)).child("Reset").on_click(cx.listener(|v, _, _, cx| v.save(|s| s.export_dir = None, cx))))
                    })
                    .into_any_element()
            }
            Row::McpConnections => return self.render_mcp_connections(t, cx),
            Row::McpAdvanced => return self.render_mcp_advanced(t, cx),
            Row::Version => {
                use crate::updates::State;
                let enabled = crate::updates::enabled(cx);
                let state = crate::updates::state(cx);
                let offered = state.feed().is_some();
                let checking = state == State::Checking;
                let status = Self::update_status(cx);
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(div().text_color(t.tx2).child(crate::VERSION))
                    .children(status.map(|(s, color, err)| {
                        div()
                            .id("update-status")
                            .max_w(px(320.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .text_color(color)
                            .when(err, |d| d.tooltip(ui::text_tooltip(s.clone())))
                            .child(s)
                    }))
                    .when(enabled && offered, |d| {
                        d.child(ui::button(t, "update-now", ui::Btn::Primary).h(px(28.)).child("Update…").on_click(cx.listener(|_, _, window, cx| {
                            cx.emit(SettingsEvent::Close);
                            window.dispatch_action(Box::new(crate::app::ShowUpdate), cx);
                        })))
                    })
                    .when(enabled && !offered, |d| {
                        let bounds = self.check_now_bounds.clone();
                        d.child(
                            div().flex_none().on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b).child(
                                ui::button(t, "check-now", ui::Btn::Secondary)
                                    .h(px(28.))
                                    .child("Check now")
                                    .when(checking, |d| d.opacity(0.45).cursor_default())
                                    .when(!checking, |d| d.on_click(cx.listener(|_, _, _, cx| crate::updates::check(true, cx)))),
                            ),
                        )
                    })
                    .into_any_element()
            }
            Row::UpdateCheck => {
                let on = s.update_check;
                ui::segmented(t)
                    .child(ui::segment(t, "upd-daily", "Daily", on).on_click(cx.listener(|v, _, _, cx| v.set_update_check(true, cx))))
                    .child(ui::segment(t, "upd-off", "Off", !on).on_click(cx.listener(|v, _, _, cx| v.set_update_check(false, cx))))
                    .into_any_element()
            }
            Row::CrashReports => ui::button(t, "crash-folder", ui::Btn::Secondary)
                .h(px(28.))
                .child("Show")
                .on_click(|_, _, cx| {
                    let dir = dataman_core::crash::dir();
                    let _ = std::fs::create_dir_all(&dir);
                    cx.open_with_system(&dir);
                })
                .into_any_element(),
            Row::McpAccess => {
                let on = s.mcp_enabled;
                ui::segmented(t)
                    .child(ui::segment(t, "mcp-off", "Off", !on).on_click(cx.listener(|v, _, _, cx| v.set_mcp(|s| s.mcp_enabled = false, cx))))
                    .child(ui::segment(t, "mcp-on", "On", on).on_click(cx.listener(|v, _, _, cx| v.set_mcp(|s| s.mcp_enabled = true, cx))))
                    .into_any_element()
            }
            Row::McpTabs => {
                let on = s.mcp_show_in_tabs;
                ui::segmented(t)
                    .child(ui::segment(t, "mcp-tabs-off", "Off", !on).on_click(cx.listener(|v, _, _, cx| v.save(|s| s.mcp_show_in_tabs = false, cx))))
                    .child(ui::segment(t, "mcp-tabs-on", "On", on).on_click(cx.listener(|v, _, _, cx| v.save(|s| s.mcp_show_in_tabs = true, cx))))
                    .into_any_element()
            }
            Row::McpWriteLimit => {
                let cur = s.mcp_write_row_limit;
                ui::segmented(t)
                    .children(MCP_WRITE_CHOICES.into_iter().map(|(n, label)| {
                        ui::segment(t, ("mwl", n as usize), label, cur == n).on_click(cx.listener(move |v, _, _, cx| v.save(|s| s.mcp_write_row_limit = n, cx)))
                    }))
                    .into_any_element()
            }
            Row::McpRows => {
                let cur = s.mcp_max_rows;
                ui::segmented(t)
                    .children(MCP_ROW_CHOICES.into_iter().map(|(n, label)| {
                        ui::segment(t, ("mr", n), label, cur == n).on_click(cx.listener(move |v, _, _, cx| v.save(|s| s.mcp_max_rows = n, cx)))
                    }))
                    .into_any_element()
            }
            Row::McpClaude => self.copy_button(t, "copy-stdio", "Copy command", Some(crate::mcp::stdio_command()), cx),
            Row::McpLog => ui::button(t, "open-mcp-log", ui::Btn::Secondary)
                .h(px(28.))
                .child("Open")
                .on_click(cx.listener(|_, _, window, cx| {
                    cx.emit(SettingsEvent::Close);
                    window.dispatch_action(Box::new(crate::app::ToggleMcpLog), cx);
                }))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.))
            .pb(px(14.))
            .border_b_1()
            .border_color(t.line)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_color(t.tx).child(label))
                    .children(row.help().map(|h| div().text_size(px(10.5)).line_height(px(15.)).text_color(t.tx3).child(h))),
            )
            .child(control)
            .into_any_element()
    }

    fn render_themes(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let cur = theme::pref(cx);
        let card = |id: &'static str, label: &'static str, pref: ThemePref, dark: bool, split: bool| {
            let on = cur == pref;
            let c = |hex: u32| -> gpui::Hsla { gpui::rgb(hex).into() };
            let (side, main, l1, l2, l3) = if dark {
                (c(0x11131d), c(0x0b0c13), c(0x7aa2ff), c(0x4b5066), c(0x9be37b))
            } else {
                (c(0xe9ecf3), c(0xffffff), c(0x2f5bff), c(0xc9cdd9), c(0x2c7a2e))
            };
            let bar = |w: f32, color: gpui::Hsla| div().h(px(4.)).w(gpui::relative(w)).bg(color);
            let preview = div()
                .w(px(150.))
                .h(px(92.))
                .overflow_hidden()
                .flex()
                .flex_row()
                .border_1()
                .border_color(if on { t.acc } else { t.line2 })
                .when(on, |d| d.shadow(t.halo()))
                .child(div().w(px(40.)).h_full().bg(side).when(split, |d| d.child(div().mt(px(46.)).h(px(46.)).bg(c(0xe9ecf3)))))
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .relative()
                        .bg(main)
                        .when(split, |d| d.child(div().absolute().left_0().right_0().bottom_0().h(px(46.)).bg(c(0xffffff))))
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .px(px(8.))
                                .py(px(10.))
                                .flex()
                                .flex_col()
                                .gap(px(5.))
                                .child(bar(0.7, l1))
                                .child(bar(0.5, l2))
                                .child(bar(0.8, l2))
                                .child(bar(0.4, l3)),
                        ),
                );
            div()
                .children(ui::here(&id.into()))
                .id(id)
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .child(preview)
                .child(div().text_color(if on { t.tx } else { t.tx3 }).child(label))
                .on_click(cx.listener(move |v, _, _, cx| v.set_theme(pref, cx)))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pb(px(14.))
            .border_b_1()
            .border_color(t.line)
            .child(ui::caps(t, "THEME"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(12.))
                    .child(card("th-dark", "Dark", ThemePref::Dark, true, false))
                    .child(card("th-light", "Light", ThemePref::Light, false, false))
                    .child(card("th-system", "Match system", ThemePref::System, true, true)),
            )
    }

    fn matching_commands(q: &str) -> Vec<&'static crate::keymap::Command> {
        let typed = crate::keymap::parse_typed(q);
        crate::keymap::COMMANDS
            .iter()
            .filter(|c| {
                q.is_empty()
                    || c.label.to_lowercase().contains(q)
                    || c.group.to_lowercase().contains(q)
                    || crate::keymap::key_label(c.id).to_lowercase().contains(q)
                    || typed.as_ref().is_some_and(|k| crate::keymap::effective(c.id).contains(k))
            })
            .collect()
    }

    fn commands_on(key: Option<&str>) -> Vec<&'static crate::keymap::Command> {
        crate::keymap::COMMANDS.iter().filter(|c| key.is_none_or(|k| crate::keymap::effective(c.id).iter().any(|e| e == k))).collect()
    }

    fn render_key_query(&self, t: &Theme) -> gpui::AnyElement {
        match &self.key_query {
            Some(k) => div().flex().flex_row().child(ui::keycap(t, crate::keymap::label(k)).text_color(t.acc_t).border_color(t.acc)).into_any_element(),
            None => div().whitespace_nowrap().overflow_hidden().text_color(t.tx3).child("Press a shortcut").into_any_element(),
        }
    }

    fn render_commands(&self, cmds: Vec<&'static crate::keymap::Command>, searching: bool, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let conflicts = crate::keymap::conflicts();
        let overrides = crate::keymap::overrides();
        let mut groups: Vec<(&str, Vec<&crate::keymap::Command>)> = Vec::new();
        for c in cmds {
            match groups.iter_mut().find(|(g, _)| *g == c.group) {
                Some((_, v)) => v.push(c),
                None => groups.push((c.group, vec![c])),
            }
        }
        let reset_all = (!searching && !overrides.is_empty()).then(|| {
            let bounds = self.reset_all_bounds.clone();
            div()
                .flex()
                .flex_row()
                .justify_end()
                .on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b)
                .child(
                    ui::button(t, "keys-reset-all", ui::Btn::Secondary)
                        .h(px(26.))
                        .child("Reset all")
                        .on_click(cx.listener(|v, _, _, cx| v.set_key("", None, cx))),
                )
        });
        let mut ix = 0usize;
        let mut sections: Vec<gpui::AnyElement> = Vec::new();
        for (title, cmds) in groups {
            let lar = title.starts_with('◆');
            let mut rows: Vec<gpui::AnyElement> = Vec::new();
            let also = self.key_query.as_deref().filter(|_| self.key_search).map(crate::keymap::label);
            for c in cmds {
                ix += 1;
                let id = c.id;
                let rec = self.recording == Some(id);
                let changed = overrides.contains_key(id);
                let shown = crate::keymap::key_label(id);
                let shown_label = shown.clone();
                let text: SharedString = if rec { "…".into() } else if shown.is_empty() { "—".into() } else { shown.into() };
                let bounds = self.key_bounds.clone();
                let cap = ui::keycap(t, text)
                    .id(("key", ix))
                    .min_w(px(30.))
                    .justify_center()
                    .cursor_pointer()
                    .when(lar, |d| d.text_color(t.lar_t).border_color(t.lar_r))
                    .when(conflicts.contains(id), |d| d.text_color(t.err))
                    .when(rec, |d| d.bg(t.acc_s).border_color(t.acc).text_color(t.acc_t))
                    .on_click(cx.listener(move |v, _, window, cx| v.start_recording(id, window, cx)));
                let reset = changed.then(|| {
                    let hov = t.hov;
                    div()
                        .id(("key-reset", ix))
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(move |s| s.bg(hov))
                        .child(icon(Icon::Reload).size(px(12.)).text_color(t.tx3))
                        .on_click(cx.listener(move |v, _, _, cx| v.set_key(id, None, cx)))
                });
                rows.push(
                    div()
                        .min_h(px(28.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .border_b_1()
                        .border_color(t.line)
                        .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(t.tx2).child(c.label))
                        .children(also.as_ref().filter(|k| **k != shown_label).map(|k| div().flex_none().text_size(px(11.)).text_color(t.tx3).child(format!("also {k}"))))
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.))
                                .on_children_prepainted(move |b, _, _| {
                                    bounds.borrow_mut().insert(id, b);
                                })
                                .children(reset)
                                .child(cap),
                        )
                        .into_any_element(),
                );
            }
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().pb(px(6.)).text_size(px(10.5)).text_color(if lar { t.lar } else { t.tx3 }).child(title))
                    .children(rows)
                    .into_any_element(),
            );
        }
        div().flex().flex_col().gap(px(18.)).children(reset_all).children(sections).into_any_element()
    }

    #[cfg(feature = "checks")]
    pub fn demo_key_points(&self, id: &str) -> (Option<gpui::Point<Pixels>>, Option<gpui::Point<Pixels>>) {
        let b = self.key_bounds.borrow();
        let v = b.get(id).cloned().unwrap_or_default();
        (v.last().map(|b| b.center()), (v.len() == 2).then(|| v[0].center()))
    }

    #[cfg(feature = "checks")]
    pub fn demo_reset_all_point(&self) -> Option<gpui::Point<Pixels>> {
        self.reset_all_bounds.borrow().first().map(|b| b.center())
    }

    #[cfg(feature = "checks")]
    pub fn demo_recording(&self) -> Option<&'static str> {
        self.recording
    }

    #[cfg(feature = "checks")]
    pub fn demo_section(&self) -> &'static str {
        self.section.label()
    }

    #[cfg(feature = "checks")]
    pub fn demo_search(&mut self, q: &str, cx: &mut Context<Self>) {
        self.search.update(cx, |i, cx| i.set_text(q, cx));
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_key_search_point(&self) -> Option<gpui::Point<Pixels>> {
        self.key_button_bounds.borrow().first().map(|b| b.center())
    }

    #[cfg(feature = "checks")]
    pub fn demo_check_now_point(&self) -> Option<gpui::Point<Pixels>> {
        self.check_now_bounds.borrow().first().map(|b| b.center())
    }

    #[cfg(feature = "checks")]
    pub fn demo_update_status(&self, cx: &App) -> Option<String> {
        Self::update_status(cx).map(|(s, _, _)| s.to_string())
    }

    #[cfg(feature = "checks")]
    pub fn demo_key_search(&self) -> (bool, Option<String>, Vec<&'static str>) {
        let ids = if self.key_search { Self::commands_on(self.key_query.as_deref()).iter().map(|c| c.id).collect() } else { Vec::new() };
        (self.key_search, self.key_query.clone(), ids)
    }

    #[cfg(feature = "checks")]
    pub fn demo_key_search_with(&mut self, key: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        self.set_key_search(true, window, cx);
        self.key_query = key.and_then(crate::keymap::parse);
    }

    #[cfg(feature = "checks")]
    pub fn demo_scroll(&mut self, y: f32) {
        self.page_scroll.set_offset(gpui::point(px(0.), px(-y)));
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let q = self.query(cx);
        let keys = self.key_search;
        let searching = !q.is_empty() && !keys;
        let nav = SECTIONS.iter().map(|&s| {
            let on = if keys { s == Section::Keyboard } else { !searching && s == self.section };
            let hov = t.hov;
            div()
                .children(ui::here(&s.label().into()))
                .id(s.label())
                .h(px(30.))
                .mx(px(8.))
                .px(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .when(on, |d| d.bg(t.acc_s).text_color(t.tx).border_1().border_color(t.acc))
                .when(!on, |d| d.text_color(t.tx2).hover(move |st| st.bg(hov)))
                .child(div().flex_1().child(s.label()))
                .on_click(cx.listener(move |v, _, window, cx| {
                    if v.key_search {
                        v.set_key_search(false, window, cx);
                    }
                    v.section = s;
                    v.search.update(cx, |i, cx| i.set_text("", cx));
                    v.scroll_to_top();
                    cx.notify();
                }))
        }).collect::<Vec<_>>();
        let mut page: Vec<gpui::AnyElement> = Vec::new();
        let key_label = self.key_query.as_deref().map(crate::keymap::label);
        let title: SharedString = match &key_label {
            _ if !keys && searching => format!("Results for \"{q}\"").into(),
            _ if !keys => self.section.label().into(),
            Some(k) => format!("Commands on {k}").into(),
            None => "Keyboard".into(),
        };
        if keys {
            let cmds = Self::commands_on(self.key_query.as_deref());
            if !cmds.is_empty() {
                page.push(self.render_commands(cmds, true, &t, cx));
            }
        } else {
            for (row, section, label, words) in ROWS {
                let hit = if searching { label.to_lowercase().contains(&q) || words.contains(q.as_str()) } else { section == self.section };
                if hit && self.row_visible(row, cx) {
                    page.push(self.render_row(row, label, &t, cx));
                }
            }
            let cmds = Self::matching_commands(&q);
            let show_keys = (!searching && self.section == Section::Keyboard) || (searching && !cmds.is_empty());
            if show_keys {
                if searching {
                    page.push(ui::caps(&t, "KEYBOARD").into_any_element());
                }
                page.push(self.render_commands(cmds, searching, &t, cx));
            }
        }
        if page.is_empty() {
            let free = key_label.map(|k| SharedString::from(format!("No command uses {k}.")));
            page.push(div().pt(px(80.)).flex().justify_center().child(ui::empty_state(&t, "[ NO MATCH ]", 20., free)).into_any_element());
        }
        let key_button = {
            let hov = t.hov;
            let bounds = self.key_button_bounds.clone();
            div().flex_none().on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b).child(
                div()
                    .id("settings-key-search")
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(keys, |d| d.bg(t.acc_s))
                    .when(!keys, move |d| d.hover(move |s| s.bg(hov)))
                    .child(icon(Icon::Keyboard).size(px(14.)).text_color(if keys { t.acc } else { t.tx3 }))
                    .tooltip(ui::text_tooltip(if keys { "Search by text" } else { "Search by shortcut" }))
                    .on_click(cx.listener(|v, _, window, cx| v.set_key_search(!v.key_search, window, cx))),
            )
        };
        let scrolled = self.page_scroll.offset().y < px(0.);
        let glass = t.glass;
        let sheet = ui::modal(&t, "settings-sheet")
            .w(px(900.))
            .h(px(660.))
            .flex_row()
            .overflow_hidden()
            .child(
                div()
                    .w(px(210.))
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(glass)
                    .border_r_1()
                    .border_color(t.line)
                    .child(div().h(px(44.)).pl(px(16.)).flex().items_center().child(ui::caps(&t, "SETTINGS")))
                    .child(
                        div()
                            .mx(px(10.))
                            .mb(px(10.))
                            .mt(px(4.))
                            .h(px(28.))
                            .pl(px(8.))
                            .pr(px(3.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .bg(t.hov)
                            .border_1()
                            .border_color(if keys { t.acc } else { t.field_b })
                            .text_color(t.tx)
                            .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                            .child(div().flex_1().min_w_0().map(|d| if keys { d.child(self.render_key_query(&t)) } else { d.child(self.search.clone()) }))
                            .child(key_button),
                    )
                    .children(nav),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_none()
                            .h(px(60.))
                            .pl(px(30.))
                            .pr(px(16.))
                            .pt(px(23.))
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap(px(12.))
                            .border_b_1()
                            .border_color(if scrolled { t.line } else { gpui::transparent_black() })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(26.))
                                    .flex()
                                    .items_center()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(ui::modal_close(&t, "settings-close").on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close)))),
                    )
                    .child(
                        div()
                            .id("settings-page")
                            .flex_1()
                            .min_h_0()
                            .px(px(30.))
                            .pt(px(6.))
                            .pb(px(26.))
                            .flex()
                            .flex_col()
                            .gap(px(20.))
                            .overflow_y_scroll()
                            .track_scroll(&self.page_scroll)
                            .children(page),
                    ),
            );
        div()
            .id("settings-scrim")
            .absolute()
            .top(px(metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.scrim)
            .key_context("SettingsSheet Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|v, _: &Close, window, cx| {
                if !v.search.read(cx).text().is_empty() {
                    v.search.update(cx, |i, cx| i.set_text("", cx));
                    window.focus(&v.search.focus_handle(cx), cx);
                    return;
                }
                cx.emit(SettingsEvent::Close)
            }))
            .on_action(cx.listener(|v, _: &NextSection, _, cx| v.step_section(1, cx)))
            .on_action(cx.listener(|v, _: &PrevSection, _, cx| v.step_section(-1, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close)))
            .child(sheet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rank_fonts_filters_sorts_and_caps() {
        let all = fonts(&["Menlo", "Monaco", "Consolas", "Courier New", "Arial"]);
        assert_eq!(rank_fonts("cour", &all, 8), vec!["Courier New"]);
        assert_eq!(rank_fonts("", &all, 8), vec!["Arial", "Consolas", "Courier New", "Menlo", "Monaco"]);
        assert_eq!(rank_fonts("", &all, 2).len(), 2);
        assert!(rank_fonts("xyz123", &all, 8).is_empty());
    }
}
