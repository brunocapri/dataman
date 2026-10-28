use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton,
    ScrollHandle, Window, actions, div, prelude::*,
};
use crate::theme::px;

use crate::assets::{Icon, icon};
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, PIXEL, Theme};
use crate::ui;

actions!(palette, [Dismiss, Up, Down, NextScope, PrevScope, OpenNewTab, OpenInPlace, PageUp, PageDown]);

const CONTEXT: &str = "Palette";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("escape", Dismiss, c),
        crate::keymap::kb("up", Up, c),
        crate::keymap::kb("down", Down, c),
        crate::keymap::kb("ctrl-p", Up, c),
        crate::keymap::kb("ctrl-n", Down, c),
        crate::keymap::kb("pageup", PageUp, c),
        crate::keymap::kb("pagedown", PageDown, c),
        crate::keymap::kb("tab", NextScope, c),
        crate::keymap::kb("shift-tab", PrevScope, c),
        crate::keymap::kb("cmd-enter", OpenNewTab, c),
        crate::keymap::kb("alt-enter", OpenInPlace, c),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

fn is_boundary(prev: Option<char>, c: char) -> bool {
    match prev {
        None => true,
        Some(p) => matches!(p, '_' | ' ' | '.' | '-' | '/' | '(' | '·') || (p.is_lowercase() && c.is_uppercase()),
    }
}

pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(Match { score: 0, positions: vec![] });
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let lower: Vec<char> = chars.iter().map(|(_, c)| c.to_lowercase().next().unwrap_or(*c)).collect();
    let n = lower.len();
    let len_penalty = (n as i32).min(60);
    let mut best: Option<(i32, usize)> = None;
    if q.len() <= n {
        for start in 0..=n - q.len() {
            if lower[start..start + q.len()] == q[..] {
                let prev = if start == 0 { None } else { Some(chars[start - 1].1) };
                let score = if start == 0 {
                    1000
                } else if is_boundary(prev, chars[start].1) {
                    800 - start as i32
                } else {
                    600 - start as i32
                };
                if best.is_none_or(|(s, _)| score > s) {
                    best = Some((score, start));
                }
            }
        }
    }
    if let Some((score, start)) = best {
        return Some(Match { score: score - len_penalty, positions: (start..start + q.len()).map(|i| chars[i].0).collect() });
    }
    // Word starts first; when that skips letters the rest needs ("report 497 rev" in
    // "Report 497 revenue"), the next occurrence every time.
    subsequence(&q, &chars, &lower, true).or_else(|| subsequence(&q, &chars, &lower, false)).map(|(score, positions)| Match { score: score - len_penalty, positions })
}

fn subsequence(q: &[char], chars: &[(usize, char)], lower: &[char], words: bool) -> Option<(i32, Vec<usize>)> {
    let n = lower.len();
    let mut positions = Vec::with_capacity(q.len());
    let mut at = 0usize;
    let mut score = 300i32;
    let mut last: Option<usize> = None;
    let mut gaps = 0usize;
    for &qc in q {
        let next_any = (at..n).find(|&i| lower[i] == qc)?;
        let next_word = (at..n).find(|&i| lower[i] == qc && is_boundary(if i == 0 { None } else { Some(chars[i - 1].1) }, chars[i].1));
        let i = match next_word {
            Some(w) if words && w - next_any <= 8 => w,
            _ => next_any,
        };
        if is_boundary(if i == 0 { None } else { Some(chars[i - 1].1) }, chars[i].1) {
            score += 12;
        }
        if let Some(l) = last {
            gaps += i - l - 1;
            score -= ((i - l - 1) as i32).min(20) * 3;
        } else {
            score -= (i as i32).min(30) * 2;
        }
        positions.push(chars[i].0);
        last = Some(i);
        at = i + 1;
    }
    if gaps > 3 * q.len() {
        return None;
    }
    Some((score, positions))
}

fn sql_match(query: &str, sql: &str) -> Option<Match> {
    let q = query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    (!q.is_empty() && sql.to_lowercase().contains(&q)).then(|| Match { score: 100, positions: vec![] })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Commands,
    Tables,
    Tabs,
    Favorites,
    Connections,
    Goto,
}

const SCOPES: [(Scope, &str, &str); 6] = [
    (Scope::All, "All", ""),
    (Scope::Commands, "Commands", ">"),
    (Scope::Tables, "Tables", "@"),
    (Scope::Tabs, "Tabs", "#"),
    (Scope::Favorites, "Favorites", "*"),
    (Scope::Connections, "Connections", ":"),
];

impl Scope {
    fn from_prefix(c: char) -> Option<Scope> {
        SCOPES.iter().find(|(_, _, p)| p.starts_with(c) && !p.is_empty()).map(|(s, _, _)| *s)
    }
    fn shows(self, s: Section) -> bool {
        match self {
            Scope::All => true,
            Scope::Commands => s == Section::Commands,
            Scope::Tables => matches!(s, Section::Tables | Section::Offline),
            Scope::Tabs => s == Section::Tabs,
            Scope::Favorites => s == Section::Favorites,
            Scope::Connections => s == Section::Connections,
            Scope::Goto => matches!(s, Section::Tables | Section::Tabs | Section::Offline),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Tables,
    Tabs,
    Favorites,
    Commands,
    Connections,
    Offline,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Tables => "TABLES",
            Section::Tabs => "TABS",
            Section::Favorites => "FAVORITES",
            Section::Commands => "COMMANDS",
            Section::Connections => "CONNECTIONS",
            Section::Offline => "NOT CONNECTED",
        }
    }
    fn all_cap(self) -> usize {
        match self {
            Section::Tables => 6,
            Section::Tabs => 4,
            Section::Favorites => 6,
            Section::Commands => 6,
            Section::Connections => 4,
            Section::Offline => 3,
        }
    }
}

const SCOPE_MAX: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Workspace,
    Editor,
    Grid,
}

pub enum ItemKind {
    Table { conn: String, db: String, schema: String, name: String },
    Tab(usize),
    Favorite { id: String },
    Command { action: Box<dyn Action>, target: Target },
    Connection { id: String },
}

impl Clone for ItemKind {
    fn clone(&self) -> Self {
        match self {
            ItemKind::Table { conn, db, schema, name } => ItemKind::Table { conn: conn.clone(), db: db.clone(), schema: schema.clone(), name: name.clone() },
            ItemKind::Tab(i) => ItemKind::Tab(*i),
            ItemKind::Favorite { id } => ItemKind::Favorite { id: id.clone() },
            ItemKind::Command { action, target } => ItemKind::Command { action: action.boxed_clone(), target: *target },
            ItemKind::Connection { id } => ItemKind::Connection { id: id.clone() },
        }
    }
}

#[derive(Clone)]
pub struct Item {
    pub kind: ItemKind,
    pub section: Section,
    pub label: String,
    pub sub: String,
    pub key: String,
    pub icon: PaletteIcon,
    pub lar: bool,
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteIcon {
    Table,
    View,
    Tab,
    Favorite,
    Command,
    Connection,
    Lar(Icon),
}

impl Item {
    pub fn command(label: &str, sub: &str, key: &str, action: Box<dyn Action>, target: Target) -> Item {
        let key = crate::keymap::label_for_action(&*action).unwrap_or_else(|| crate::keymap::glyphs(key));
        Item {
            kind: ItemKind::Command { action, target },
            section: Section::Commands,
            label: label.into(),
            sub: sub.into(),
            key,
            icon: PaletteIcon::Command,
            lar: false,
            disabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    Open,
    NewTab,
    InPlace,
}

pub enum PaletteEvent {
    Pick(ItemKind, OpenMode),
    SearchHistory(String),
    Dismiss,
}

#[derive(Clone, Debug)]
struct Hit {
    item: usize,
    m: Match,
}

fn rank(items: &[Item], query: &str, scope: Scope) -> (Vec<(Section, Vec<Hit>)>, usize) {
    let mut groups: Vec<(Section, Vec<Hit>)> = Vec::new();
    let mut hidden = 0;
    for section in [Section::Tables, Section::Tabs, Section::Favorites, Section::Commands, Section::Connections, Section::Offline] {
        if !scope.shows(section) {
            continue;
        }
        if query.is_empty() && scope == Scope::All && matches!(section, Section::Tables | Section::Favorites | Section::Offline) {
            continue;
        }
        let mut hits: Vec<Hit> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.section == section)
            .filter_map(|(i, it)| {
                if section == Section::Offline {
                    return Some(Hit { item: i, m: Match { score: 0, positions: vec![] } });
                }
                let m = fuzzy(query, &it.label).or_else(|| {
                    if section == Section::Favorites {
                        return sql_match(query, &it.sub);
                    }
                    fuzzy(query, &it.sub).filter(|m| m.score >= 500 || query.is_empty()).map(|m| Match { score: m.score - 400, positions: vec![] })
                })?;
                Some(Hit { item: i, m })
            })
            .collect();
        if !query.is_empty() {
            hits.sort_by(|a, b| {
                b.m.score
                    .cmp(&a.m.score)
                    .then_with(|| items[a.item].label.len().cmp(&items[b.item].label.len()))
                    .then_with(|| items[a.item].label.cmp(&items[b.item].label))
            });
        }
        let cap = if scope == Scope::All { section.all_cap() } else { SCOPE_MAX };
        if hits.len() > cap {
            hidden += hits.len() - cap;
            hits.truncate(cap);
        }
        if !hits.is_empty() {
            groups.push((section, hits));
        }
    }
    (groups, hidden)
}

pub struct Palette {
    focus: FocusHandle,
    input: gpui::Entity<TextInput>,
    scope: Scope,
    items: Vec<Item>,
    groups: Vec<(Section, Vec<Hit>)>,
    hidden: usize,
    sel: usize,
    scroll: ScrollHandle,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Palette {
    pub fn new(items: Vec<Item>, scope: Scope, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(cx, "Type a command, a table, a tab…").bare());
        cx.subscribe(&input, |p: &mut Palette, _, ev: &InputEvent, cx| match ev {
            InputEvent::Changed => p.refilter(cx),
            InputEvent::Submit => p.pick(OpenMode::Open, cx),
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        let mut p = Palette { focus: cx.focus_handle(), input, scope, items, groups: vec![], hidden: 0, sel: 0, scroll: ScrollHandle::new() };
        p.refilter(cx);
        p
    }

    #[cfg(feature = "checks")]
    pub fn set_query(&mut self, q: &str, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_text(q, cx));
        self.refilter(cx);
    }

    fn parsed(&self, cx: &App) -> (Scope, String) {
        let text = self.input.read(cx).text();
        let mut chars = text.chars();
        if let Some(c) = chars.next()
            && let Some(s) = Scope::from_prefix(c)
        {
            return (s, chars.as_str().trim().to_string());
        }
        (self.scope, text.trim().to_string())
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let (scope, q) = self.parsed(cx);
        let (groups, hidden) = rank(&self.items, &q, scope);
        self.groups = groups;
        self.hidden = hidden;
        self.sel = 0;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn count(&self) -> usize {
        self.groups.iter().map(|(_, h)| h.len()).sum()
    }

    fn selected(&self) -> Option<&Hit> {
        self.groups.iter().flat_map(|(_, h)| h.iter()).nth(self.sel)
    }

    fn move_sel(&mut self, d: isize, cx: &mut Context<Self>) {
        let n = self.count();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, n as isize - 1) as usize;
        self.scroll.scroll_to_item(self.row_of(self.sel));
        cx.notify();
    }

    fn row_of(&self, flat: usize) -> usize {
        let mut row = 0;
        let mut seen = 0;
        for (_, hits) in &self.groups {
            row += 1;
            if flat < seen + hits.len() {
                return row + flat - seen;
            }
            seen += hits.len();
            row += hits.len();
        }
        row
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_state(&self) -> (String, usize, usize, f32, bool, gpui::Bounds<gpui::Pixels>) {
        let label = self.selected().map(|h| self.items[h.item].label.clone()).unwrap_or_default();
        let view = self.scroll.bounds();
        // Child bounds are laid out unscrolled: the offset (negative) moves them up.
        let dy = self.scroll.offset().y;
        let inside = self.scroll.bounds_for_item(self.row_of(self.sel)).is_some_and(|b| b.top() + dy >= view.top() - px(0.5) && b.bottom() + dy <= view.bottom() + px(0.5));
        (label, self.sel, self.count(), -self.scroll.offset().y.as_f32(), inside, view)
    }

    fn cycle_scope(&mut self, d: isize, cx: &mut Context<Self>) {
        let (cur, q) = self.parsed(cx);
        let i = SCOPES.iter().position(|(s, _, _)| *s == cur).unwrap_or(0) as isize;
        let n = SCOPES.len() as isize;
        self.scope = SCOPES[(i + d).rem_euclid(n) as usize].0;
        self.input.update(cx, |i, cx| i.set_text(q, cx));
        self.refilter(cx);
    }

    fn pick(&mut self, mode: OpenMode, cx: &mut Context<Self>) {
        let Some(hit) = self.selected() else { return };
        let item = &self.items[hit.item];
        if item.disabled {
            return;
        }
        cx.emit(PaletteEvent::Pick(item.kind.clone(), mode));
    }

    fn pick_index(&mut self, flat: usize, mode: OpenMode, cx: &mut Context<Self>) {
        self.sel = flat;
        self.pick(mode, cx);
    }

    fn render_item(&self, flat: usize, hit: &Hit, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let it = &self.items[hit.item];
        let sel = flat == self.sel;
        let icon_el = {
            let (ic, color) = match it.icon {
                PaletteIcon::Table => (Icon::Table, None),
                PaletteIcon::View => (Icon::View, None),
                PaletteIcon::Tab => (Icon::Tab, None),
                PaletteIcon::Favorite => (Icon::Star, None),
                PaletteIcon::Command => (Icon::Command, None),
                PaletteIcon::Connection => (Icon::Database, None),
                PaletteIcon::Lar(i) => (i, Some(t.lar)),
            };
            let c = color.unwrap_or(if it.lar { t.lar } else if sel { t.tx } else { t.tx3 });
            icon(ic).size(px(15.)).text_color(c)
        };
        let (mark, line) = if it.lar { (t.lar_t, t.lar) } else { (t.tx, t.acc) };
        let label = highlighted(&it.label, &hit.m.positions, mark, line);
        let hov = t.hov;
        div()
            .id(("pal", flat))
            .h(px(32.))
            .px(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .when(sel, |d| d.bg(t.acc_s).border_1().border_color(t.acc).text_color(t.tx))
            .when(!sel, |d| d.text_color(if it.disabled { t.tx4 } else { t.tx2 }).hover(move |s| s.bg(hov)))
            .child(icon_el)
            .child(div().flex_none().whitespace_nowrap().child(label))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(11.))
                    .text_color(t.tx3)
                    .when(it.section != Section::Commands, |d| d.child(it.sub.clone())),
            )
            .child(div().flex_none().text_size(px(11.)).text_color(t.tx3).child(it.key.clone()))
            .on_click(cx.listener(move |p, ev: &gpui::ClickEvent, _, cx| {
                let mode = if ev.modifiers().secondary() {
                    OpenMode::NewTab
                } else if ev.modifiers().alt {
                    OpenMode::InPlace
                } else {
                    OpenMode::Open
                };
                p.pick_index(flat, mode, cx)
            }))
            .into_any_element()
    }
}

fn highlighted(label: &str, positions: &[usize], mark: Hsla, line: Hsla) -> gpui::Div {
    let mut row = div().flex().flex_row();
    if positions.is_empty() {
        return row.child(label.to_string());
    }
    let mut runs: Vec<(bool, String)> = Vec::new();
    for (i, c) in label.char_indices() {
        let m = positions.binary_search(&i).is_ok();
        match runs.last_mut() {
            Some((lm, s)) if *lm == m => s.push(c),
            _ => runs.push((m, c.to_string())),
        }
    }
    for (m, s) in runs {
        row = row.child(if m {
            div().text_color(mark).font_weight(FontWeight::BOLD).text_decoration_1().text_decoration_color(line).child(s)
        } else {
            div().child(s)
        });
    }
    row
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let (scope, q) = self.parsed(cx);
        let scopes = SCOPES.iter().map(|&(s, name, prefix)| {
            let on = s == scope || (scope == Scope::Goto && matches!(s, Scope::Tables | Scope::Tabs));
            let hov = t.hov;
            div()
                .id(name)
                .h(px(24.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .text_size(px(11.5))
                .when(on, |d| d.bg(t.prs).text_color(t.tx).border_1().border_color(t.line2))
                .when(!on, |d| d.text_color(t.tx3).hover(move |st| st.bg(hov)))
                .child(name)
                .child(div().text_size(px(10.5)).opacity(0.6).child(prefix))
                .on_click(cx.listener(move |p, _, _, cx| {
                    p.scope = s;
                    let (_, q) = p.parsed(cx);
                    p.input.update(cx, |i, cx| i.set_text(q, cx));
                    p.refilter(cx);
                }))
        }).collect::<Vec<_>>();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut flat = 0;
        for (section, hits) in &self.groups {
            rows.push(
                div()
                    .h(px(26.))
                    .px(px(10.))
                    .pb(px(4.))
                    .flex()
                    .items_end()
                    .text_size(px(10.))
                    .text_color(if *section == Section::Offline { t.warn } else { t.tx3 })
                    .child(section.title())
                    .into_any_element(),
            );
            for h in hits {
                rows.push(self.render_item(flat, h, &t, cx));
                flat += 1;
            }
        }
        let empty = self.groups.is_empty();
        if self.hidden > 0 && !empty {
            rows.push(
                div()
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(11.))
                    .text_color(t.tx3)
                    .child(format!("+{} more", crate::grid::ui_count(self.hidden)))
                    .into_any_element(),
            );
        }
        let body: gpui::AnyElement = if empty {
            let q2 = q.clone();
            div()
                .py(px(26.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.))
                .child(ui::pixel("[ NO MATCH ]", 20., t.tx4))
                .child(
                    ui::button(&t, "pal-history", ui::Btn::Secondary)
                        .child("Search history")
                        .when_some(crate::keymap::key("history"), |d, k| d.child(ui::key_hint(k)))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PaletteEvent::SearchHistory(q2.clone())))),
                )
                .into_any_element()
        } else {
            div().id("pal-list").max_h(px(420.)).overflow_y_scroll().track_scroll(&self.scroll).p(px(6.)).children(rows).into_any_element()
        };
        let sheet = div()
            .id("palette")
            .w(px(640.))
            .flex()
            .flex_col()
            .bg(t.pop)
            .shadow(t.ring())
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(52.))
                    .px(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(div().font_family(PIXEL).text_size(px(16.)).text_color(t.acc).child("▶"))
                    .child(div().flex_1().min_w_0().text_size(px(15.)).text_color(t.tx).line_height(px(20.)).child(self.input.clone()))
                    .child(ui::keycap(&t, "esc")),
            )
            .child(
                div()
                    .h(px(36.))
                    .px(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .border_b_1()
                    .border_color(t.line)
                    .children(scopes),
            )
            .child(body);
        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            .bg(t.scrim.opacity(0.6))
            .key_context("Palette Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Dismiss, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .on_action(cx.listener(|p, _: &Up, _, cx| p.move_sel(-1, cx)))
            .on_action(cx.listener(|p, _: &Down, _, cx| p.move_sel(1, cx)))
            .on_action(cx.listener(|p, _: &PageUp, _, cx| p.move_sel(-8, cx)))
            .on_action(cx.listener(|p, _: &PageDown, _, cx| p.move_sel(8, cx)))
            .on_action(cx.listener(|p, _: &NextScope, _, cx| p.cycle_scope(1, cx)))
            .on_action(cx.listener(|p, _: &PrevScope, _, cx| p.cycle_scope(-1, cx)))
            .on_action(cx.listener(|p, _: &OpenNewTab, _, cx| p.pick(OpenMode::NewTab, cx)))
            .on_action(cx.listener(|p, _: &OpenInPlace, _, cx| p.pick(OpenMode::InPlace, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .child(div().absolute().top(px(84.)).left_0().right_0().bottom(px(12.)).flex().items_start().justify_center().child(sheet))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(section: Section, label: &str) -> Item {
        Item {
            kind: ItemKind::Tab(0),
            section,
            label: label.into(),
            sub: String::new(),
            key: String::new(),
            icon: PaletteIcon::Tab,
            lar: false,
            disabled: false,
        }
    }

    #[test]
    fn prefix_beats_word_start_beats_substring_beats_subsequence() {
        let prefix = fuzzy("ord", "orders").unwrap();
        let word = fuzzy("ord", "open_orders").unwrap();
        let sub = fuzzy("ord", "recorder").unwrap();
        let seq = fuzzy("ord", "o_r_d_x").unwrap();
        assert!(prefix.score > word.score, "{prefix:?} {word:?}");
        assert!(word.score > sub.score, "{word:?} {sub:?}");
        assert!(sub.score > seq.score, "{sub:?} {seq:?}");
        assert_eq!(prefix.positions, vec![0, 1, 2]);
        assert_eq!(word.positions, vec![5, 6, 7]);
        assert!(fuzzy("xyz", "orders").is_none());
        assert!(fuzzy("dro", "orders").is_none(), "letters must be in order");
        assert!(fuzzy("ped", "Copy TSV with header").is_none(), "scattered letters are not a match");
    }

    #[test]
    fn case_insensitive_and_shorter_first() {
        assert_eq!(fuzzy("ORD", "orders").unwrap().positions, vec![0, 1, 2]);
        assert_eq!(fuzzy("ord", "Order by column…").unwrap().positions, vec![0, 1, 2]);
        assert!(fuzzy("ord", "orders").unwrap().score > fuzzy("ord", "order_items").unwrap().score);
        assert_eq!(fuzzy("", "x").unwrap().score, 0);
    }

    #[test]
    fn subsequence_prefers_word_starts() {
        let m = fuzzy("oi", "order_items").unwrap();
        assert_eq!(m.positions, vec![0, 6]);
        let m = fuzzy("éo", "crème_éox").unwrap();
        assert_eq!(&"crème_éox"[m.positions[0]..m.positions[0] + 2], "é");
        assert_eq!(fuzzy("ct", "copyTsv").unwrap().positions, vec![0, 4]);
    }

    #[test]
    fn rank_groups_sections_and_caps() {
        let mut items = vec![];
        for i in 0..20 {
            items.push(item(Section::Tables, &format!("orders_{i:02}")));
        }
        items.push(item(Section::Tables, "orders"));
        items.push(item(Section::Tables, "customers"));
        items.push(item(Section::Tabs, "orders_90d.sql"));
        items.push(item(Section::Commands, "Order by column"));
        items.push(item(Section::Commands, "New query tab"));
        let (groups, hidden) = rank(&items, "ord", Scope::All);
        let sections: Vec<Section> = groups.iter().map(|g| g.0).collect();
        assert_eq!(sections, vec![Section::Tables, Section::Tabs, Section::Commands]);
        assert_eq!(items[groups[0].1[0].item].label, "orders");
        assert_eq!(groups[0].1.len(), 6);
        assert_eq!(hidden, 21 - 6);
        let (groups, _) = rank(&items, "ord", Scope::Tables);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 21);
        let (groups, _) = rank(&items, "ord", Scope::Goto);
        assert_eq!(groups.iter().map(|g| g.0).collect::<Vec<_>>(), vec![Section::Tables, Section::Tabs]);
        let (groups, _) = rank(&items, "", Scope::All);
        assert!(groups.iter().all(|g| g.0 != Section::Tables));
    }

    #[test]
    fn favorites_by_name_and_by_sql() {
        let fav = |label: &str, sql: &str| Item { kind: ItemKind::Favorite { id: label.into() }, section: Section::Favorites, sub: sql.into(), icon: PaletteIcon::Favorite, ..item(Section::Favorites, label) };
        let items = vec![
            fav("Report 497 revenue", "SELECT 497 AS n, 'marker_497' AS tag FROM orders"),
            fav("Report 123 churn", "SELECT 123 AS n, 'marker_123' AS tag FROM orders"),
            item(Section::Commands, "Run"),
        ];
        let top = |q: &str, s: Scope| rank(&items, q, s).0.first().and_then(|g| g.1.first()).map(|h| items[h.item].label.clone());
        assert_eq!(top("report 497 rev", Scope::All).as_deref(), Some("Report 497 revenue"));
        assert_eq!(top("marker_123", Scope::All).as_deref(), Some("Report 123 churn"), "a piece of the SQL");
        assert_eq!(top("Marker_123  AS", Scope::Favorites), None, "contiguous in the SQL, or nothing");
        assert_eq!(top("'marker_123' as", Scope::Favorites).as_deref(), Some("Report 123 churn"));
        assert!(rank(&items, "", Scope::All).0.iter().all(|g| g.0 != Section::Favorites));
        assert_eq!(rank(&items, "", Scope::Favorites).0[0].1.len(), 2);
        assert_eq!(Scope::from_prefix('*'), Some(Scope::Favorites));
    }

    #[test]
    fn prefixes_pick_scopes() {
        assert_eq!(Scope::from_prefix('>'), Some(Scope::Commands));
        assert_eq!(Scope::from_prefix('@'), Some(Scope::Tables));
        assert_eq!(Scope::from_prefix('#'), Some(Scope::Tabs));
        assert_eq!(Scope::from_prefix(':'), Some(Scope::Connections));
        assert_eq!(Scope::from_prefix('o'), None);
    }

    #[test]
    fn ranking_thousands_is_fast() {
        let items: Vec<Item> = (0..5000).map(|i| item(Section::Tables, &format!("table_{i}_orders_archive"))).collect();
        let t = std::time::Instant::now();
        for q in ["o", "or", "ord", "ordr", "t5oa"] {
            let _ = rank(&items, q, Scope::All);
        }
        let per = t.elapsed() / 5;
        assert!(per.as_millis() < 20, "{per:?} per keystroke");
    }
}

use crate::app::{self, Workspace};
use crate::conns::ConnState;

impl Workspace {
    pub(crate) fn open_palette(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        let items = self.palette_items(cx);
        let p = cx.new(|cx| Palette::new(items, scope, window, cx));
        cx.subscribe_in(&p, window, |w, _, ev: &PaletteEvent, window, cx| match ev {
            PaletteEvent::Dismiss => w.close_palette(window, cx),
            PaletteEvent::SearchHistory(q) => {
                let q = q.clone();
                w.close_palette(window, cx);
                w.show_nav(dataman_core::layout::NavView::History, window, cx);
                w.history.update(cx, |h, cx| h.set_filter(&q, cx));
            }
            PaletteEvent::Pick(kind, mode) => {
                let (kind, mode) = (kind.clone(), *mode);
                w.close_palette(window, cx);
                w.on_palette_pick(kind, mode, window, cx);
            }
        })
        .detach();
        self.palette = Some(p);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        match self.modal_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
        cx.notify();
    }

    fn on_palette_pick(&mut self, kind: ItemKind, mode: OpenMode, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            ItemKind::Table { conn, db, schema, name } => {
                if mode == OpenMode::InPlace {
                    self.select_star_in_place(&conn, &db, &schema, &name, window, cx);
                } else {
                    self.open_relation(&conn, &db, &schema, &name, window, cx);
                }
            }
            ItemKind::Tab(i) => self.activate(i, window, cx),
            ItemKind::Favorite { id } => {
                let fav = self.store.read(cx).core.favorites.list().into_iter().find(|f| f.id == id);
                match (mode, fav) {
                    (OpenMode::NewTab, Some(f)) => self.open_favorite(f, false, window, cx),
                    _ => {
                        self.show_favorite(&id, window, cx);
                        self.focus_visible = true;
                    }
                }
            }
            ItemKind::Command { action, target } => {
                match target {
                    Target::Workspace => {}
                    Target::Editor => self.focus_editor(window, cx),
                    Target::Grid => self.focus_region(app::Region::Grid, window, cx),
                }
                window.dispatch_action(action, cx);
            }
            ItemKind::Connection { id } => {
                let connected = self.store.read(cx).connection(&id);
                self.current_conn = Some(id.clone());
                match connected {
                    Some(c) => {
                        if mode == OpenMode::NewTab {
                            let tab = self.new_tab(window, cx);
                            tab.update(cx, |t, cx| t.set_connection(Some(c), cx));
                        } else if let Some(tab) = self.active_tab().cloned() {
                            self.retarget(&tab, c, None, None, window, cx);
                        }
                    }
                    None => self.store.update(cx, |s, cx| s.connect(&id, None, cx)),
                }
                let cid = format!("c:{id}");
                self.tree.update(cx, |t, cx| {
                    t.expand_id(&cid, cx);
                });
            }
        }
        cx.notify();
    }

    fn select_star_in_place(&mut self, conn: &str, db: &str, schema: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else { return };
        let Some(tab) = self.active_tab().cloned() else { return };
        let sql = dataman_core::sqlgen::select_star_in(c.dialect(), schema, name, Some(1000));
        self.retarget(&tab, c, Some(db.to_string()), Some(sql), window, cx);
    }

    pub(crate) fn palette_items(&self, cx: &App) -> Vec<Item> {
        let mut items = Vec::new();
        let store = self.store.read(cx);
        let active_conn = self.active_tab().and_then(|t| t.read(cx).connection().cloned());
        for p in &store.profiles {
            let Some(ConnState::Connected(c)) = store.states.get(&p.id) else { continue };
            let mut dbs = vec![c.database().to_string()];
            for d in c.schema.cached_databases().unwrap_or_default() {
                if !dbs.contains(&d) {
                    dbs.push(d);
                }
            }
            for db in dbs {
                let snap = c.schema.snapshot(&db);
                for r in &snap.relations {
                    if r.schema == "pg_catalog" || r.schema == "information_schema" {
                        continue;
                    }
                    items.push(Item {
                        kind: ItemKind::Table { conn: p.id.clone(), db: db.clone(), schema: r.schema.clone(), name: r.name.clone() },
                        section: Section::Tables,
                        label: r.name.clone(),
                        sub: format!("{} · {db}.{}{}", p.name, r.schema, if r.kind.is_view() { " · view" } else { "" }),
                        key: String::new(),
                        icon: if r.kind.is_view() { PaletteIcon::View } else { PaletteIcon::Table },
                        lar: c.info.is_lar,
                        disabled: false,
                    });
                }
            }
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            let tr = tab.read(cx);
            let conn = tr.connection().map(|c| c.profile.name.clone()).unwrap_or_else(|| "no connection".into());
            let state = match tr.result() {
                Some(rs) => match rs.phase() {
                    dataman_core::Phase::Paused => format!(" · paused · {}+ rows", crate::grid::ui_count(rs.rows())),
                    dataman_core::Phase::Running => " · running".into(),
                    dataman_core::Phase::Failed => " · error".into(),
                    _ => format!(" · {} rows", crate::grid::ui_count(rs.rows())),
                },
                None => String::new(),
            };
            items.push(Item {
                kind: ItemKind::Tab(i),
                section: Section::Tabs,
                label: tr.title(cx).to_string(),
                sub: format!("{conn}{state}{}", if i == self.active { " · current" } else { "" }),
                key: if i < 8 { crate::keymap::key_label(&format!("tab_{}", i + 1)) } else { String::new() },
                icon: PaletteIcon::Tab,
                lar: tr.connection().is_some_and(|c| c.info.is_lar),
                disabled: false,
            });
        }
        for f in store.core.favorites.list() {
            let flat = f.sql.split_whitespace().collect::<Vec<_>>().join(" ");
            let conn = f.connection_id.as_ref().and_then(|id| store.profile(id)).map(|p| p.name.clone());
            items.push(Item {
                kind: ItemKind::Favorite { id: f.id.clone() },
                section: Section::Favorites,
                label: f.name.clone(),
                sub: match conn {
                    Some(c) => format!("{flat} · {c}"),
                    None => flat,
                },
                key: String::new(),
                icon: PaletteIcon::Favorite,
                lar: false,
                disabled: false,
            });
        }
        items.extend(crate::undo::palette_items(self));
        items.extend(commands(active_conn.as_ref(), cx));
        for p in &store.profiles {
            let state = store.states.get(&p.id);
            let (sub, connected) = match state {
                Some(ConnState::Connected(c)) => (format!("connected · {} · {}", c.database(), c.info.short_version()), true),
                Some(ConnState::Connecting) => ("connecting…".to_string(), false),
                Some(ConnState::Failed(_)) => ("failed".to_string(), false),
                None => ("not connected".to_string(), false),
            };
            let lar = matches!(state, Some(ConnState::Connected(c)) if c.info.is_lar);
            items.push(Item {
                kind: ItemKind::Connection { id: p.id.clone() },
                section: Section::Connections,
                label: p.name.clone(),
                sub: format!("{} · {sub}", where_of(p)),
                key: String::new(),
                icon: PaletteIcon::Connection,
                lar,
                disabled: false,
            });
            if !connected {
                items.push(Item {
                    kind: ItemKind::Connection { id: p.id.clone() },
                    section: Section::Offline,
                    label: p.name.clone(),
                    sub: where_of(p),
                    key: String::new(),
                    icon: PaletteIcon::Connection,
                    lar: false,
                    disabled: false,
                });
            }
        }
        items
    }
}

fn commands(conn: Option<&std::sync::Arc<dataman_core::Connection>>, cx: &App) -> Vec<Item> {
    use crate::app::*;
    use Target::*;
    let c = |label: &str, sub: &str, key: &str, a: Box<dyn Action>, t: Target| Item::command(label, sub, key, a, t);
    let mut v = vec![
        c("Run", "the whole script, or the selection", "⌘↩", Box::new(crate::editor::RunScript), Editor),
        c("Run statement", "under the cursor, or the selection", "⌘⇧↩", Box::new(crate::editor::RunStatement), Editor),
        c("Run from statement", "the one under the cursor and the rest", "⌥⌘↩", Box::new(crate::editor::RunFromHere), Editor),
        c("Explain", "the statement under the cursor", "⌘L", Box::new(Explain), Workspace),
        c("Explain analyze", "runs it, rolled back", "⌥⌘L", Box::new(ExplainAnalyze), Workspace),
        c("Copy plan as text", "", "", Box::new(CopyPlanText), Workspace),
        c("Plan: tree / flame", "", "", Box::new(TogglePlanFlame), Workspace),
        c("Cancel on server", "sends a CancelRequest", "⌘.", Box::new(Cancel), Workspace),
        c("Commit transaction", "the tab's open transaction", "⌃⌘↩", Box::new(CommitTx), Workspace),
        c("Rollback transaction", "the tab's open transaction", "⌃⌘⌫", Box::new(RollbackTx), Workspace),
        c("Next result", "of the script", "⌥⌘]", Box::new(NextResult), Workspace),
        c("Previous result", "of the script", "⌥⌘[", Box::new(PrevResult), Workspace),
        c("Next page", "of the table", "⌥⌘→", Box::new(NextPage), Workspace),
        c("Previous page", "of the table", "⌥⌘←", Box::new(PrevPage), Workspace),
        c("New query tab", "on the explorer's connection", "⌘T", Box::new(NewTab), Workspace),
        c("Close tab", "", "⌘W", Box::new(CloseTab), Workspace),
        c("Next tab", "", "⌃Tab", Box::new(NextTab), Workspace),
        c("Previous tab", "", "⌃⇧Tab", Box::new(PrevTab), Workspace),
        c("Open to the side", "a copy of the tab in a second editor group", "⌘\\", Box::new(crate::split::OpenToSide), Workspace),
        c("Move tab to the other group", "the tab itself, its session and results", "", Box::new(crate::split::MoveToOtherGroup), Workspace),
        c("Focus the other group", "of the two editor groups", "", Box::new(crate::split::FocusOtherGroup), Workspace),
        c("Close editor group", "with its tabs (asks first)", "", Box::new(crate::split::CloseGroup), Workspace),
        c("New connection…", "", "⌘N", Box::new(NewConnection), Workspace),
        c("New connection from clipboard", "postgres:// URL or key=value", "", Box::new(NewConnectionFromClipboard), Workspace),
        c("Import connections…", "pg_service.conf, .pgpass", "", Box::new(ImportConnections), Workspace),
        c("Edit connection…", "the explorer's selected connection", "⌘I", Box::new(EditConnection), Workspace),
        c("Duplicate connection…", "the explorer's connection, as a new one", "", Box::new(crate::row_actions::DuplicateConnection), Workspace),
        c("Copy connection string", "postgresql://user@host:port/db, never the password", "", Box::new(crate::row_actions::CopyConnectionString), Workspace),
        c("Delete connection…", "the explorer's connection (asks first)", "", Box::new(crate::row_actions::DeleteConnection), Workspace),
        c("Show row actions", "the explorer's context menu for the selected row", "⇧F10", Box::new(crate::tree::ShowRowActions), Workspace),
        c("Reload schema", "of the explorer's connection", "⌘R", Box::new(Refresh), Workspace),
        c("Open .sql file…", "in a new tab", "⌘O", Box::new(OpenSqlFile), Workspace),
        c("Open database file…", "a SQLite or DuckDB file", "", Box::new(crate::app::OpenDatabaseFile), Workspace),
        c("Open project…", "a folder's shared settings, in .dataman", "", Box::new(OpenProject), Workspace),
        c("Close project", "keeps what it brought, stops syncing", "", Box::new(CloseProject), Workspace),
        c("Review edits made outside Dataman", "connections.toml or settings.toml, held until applied", "", Box::new(crate::outside_ui::ReviewOutsideEdits), Workspace),
        c("Export result…", "CSV, TSV, JSON, NDJSON, SQL INSERT, Parquet", "⌘E", Box::new(ExportResult), Workspace),
        c("Import data…", "CSV, TSV, JSON, NDJSON, Parquet", "", Box::new(ImportData), Workspace),
        c("Edit cell", "inline, in the grid", "↩", Box::new(crate::grid::EditCell), Grid),
        c("Insert row", "a new row at the end of the result", "⌘I", Box::new(crate::grid::InsertRow), Grid),
        c("Delete selected rows", "marked, removed on commit", "⌫", Box::new(crate::grid::DeleteRows), Grid),
        c("Review & commit changes", "the generated SQL, in one transaction", "⌘S", Box::new(ReviewChanges), Workspace),
        c("Discard changes", "every pending edit of this result", "⌘⌫", Box::new(DiscardChanges), Workspace),
        c("Copy as TSV", "the grid selection", "⌘C", Box::new(crate::grid::CopyTsv), Grid),
        c("Copy as CSV", "the grid selection", "⌥⌘C", Box::new(crate::grid::CopyCsv), Grid),
        c("Copy with headers", "the grid selection", "⇧⌘C", Box::new(crate::grid::CopyTsvWithHeader), Grid),
        c("Fetch next rows", "of a paused result", "⌘↓", Box::new(crate::grid::FetchMore), Grid),
        c("Fetch all rows", "of a paused result", "⇧⌘↓", Box::new(crate::grid::FetchAll), Grid),
        c("View value", "a JSON or array cell, as a tree", "⇧Space", Box::new(crate::grid::ViewValue), Grid),
        c("Jump to error", "in the editor", "⌘'", Box::new(JumpToError), Workspace),
        c("Use explorer connection", "for this tab", "⌥↩", Box::new(UseExplorerConnection), Workspace),
        c("Explorer", "the sidebar's connections and schema", "", Box::new(crate::sidebar::ShowExplorer), Workspace),
        c("Favorites", "the sidebar's saved queries", "", Box::new(crate::sidebar::ShowFavorites), Workspace),
        c("History", "the sidebar's history of this connection", "", Box::new(crate::sidebar::ShowHistory), Workspace),
        c("Favorite query", "the statement under the cursor", "⌘D", Box::new(FavoriteQuery), Workspace),
        c("Save as favorite", "the script, into the tab's favorite", "⌘S", Box::new(SaveFavorite), Workspace),
        c("Save as…", "the script, to a .sql file", "⇧⌘S", Box::new(SaveAs), Workspace),
        c("Format on save", "on or off: ⌘S formats the script first", "", Box::new(ToggleFormatOnSave), Workspace),
        c("Import favorites…", ".sql files, one favorite each", "", Box::new(ImportFavorites), Workspace),
        c("Activity monitor", "sessions, locks, tables, statements", "", Box::new(OpenActivity), Workspace),
        c("Agents", "audit, permissions, privacy, MCP", "", Box::new(ToggleMcpLog), Workspace),
        c("Toggle comment", "the selected lines", "⌘/", Box::new(crate::editor::ToggleComment), Editor),
        c("Format query", "the selection, or the whole script: SQL, MongoDB, the console, Redis", "⌥⇧F", Box::new(crate::editor::FormatQuery), Editor),
        c("Go to definition", "the name under the cursor, in the explorer", "F12", Box::new(crate::editor::GoToDefinition), Editor),
        c("Open definition", "the name under the cursor, in a new tab", "⌥F12", Box::new(crate::editor::PeekDefinition), Editor),
        c("Find", "", "⌘F", Box::new(crate::find::Find), Workspace),
        c("Find & replace", "", "⌥⌘F", Box::new(crate::find::FindReplace), Workspace),
        c("Find next", "", "⌘G", Box::new(crate::find::FindNext), Workspace),
        c("Find previous", "", "⇧⌘G", Box::new(crate::find::FindPrevious), Workspace),
        c("Toggle sidebar", "explorer, favorites, history", "⌘B", Box::new(ToggleExplorer), Workspace),
        c("Toggle side panel", "", "⌥⌘B", Box::new(ToggleSidePanel), Workspace),
        c("Move sidebar to the left", "drag it by the dot on its top edge", "", Box::new(crate::dock::MoveExplorerLeft), Workspace),
        c("Move sidebar to the right", "the side panel goes left", "", Box::new(crate::dock::MoveExplorerRight), Workspace),
        c("Reset panel layout", "the sidebar left, the side panel right", "", Box::new(crate::dock::ResetPanelLayout), Workspace),
        c("Focus explorer", "", "⌘⇧E", Box::new(FocusExplorer), Workspace),
        c("Focus grid", "", "⌘⇧G", Box::new(FocusGrid), Workspace),
        c("Focus editor", "", "esc", Box::new(FocusEditor), Workspace),
        c("Settings", "theme, editor, grid, export, keys", "⌘,", Box::new(OpenSettings), Workspace),
        c("Keyboard shortcuts", "the whole key map", "⇧⌘/", Box::new(ShowShortcuts), Workspace),
        c("Toggle theme", "dark → light → system", "", Box::new(ToggleTheme), Workspace),
        c("Theme: dark", "", "", Box::new(ThemeDark), Workspace),
        c("Theme: light", "", "", Box::new(ThemeLight), Workspace),
        c("Theme: follow the system", "", "", Box::new(ThemeSystem), Workspace),
        c("Transparency: on", "", "", Box::new(TranslucencyOn), Workspace),
        c("Transparency: off", "", "", Box::new(TranslucencyOff), Workspace),
        c("Font: Geist Mono", "", "", Box::new(FontGeistMono), Workspace),
        c("Font: Lilex", "", "", Box::new(FontLilex), Workspace),
        c("Font: Zed Mono", "", "", Box::new(FontZedMono), Workspace),
        c("Font: JetBrains Mono", "", "", Box::new(FontJetBrainsMono), Workspace),
        c("Zoom in", "bigger", "⌘=", Box::new(ZoomIn), Workspace),
        c("Zoom out", "smaller", "⌘-", Box::new(ZoomOut), Workspace),
        c("Actual size", "zoom 100%", "⌘0", Box::new(ZoomReset), Workspace),
        c("Quit Dataman", "", "⌘Q", Box::new(Quit), Workspace),
    ];
    if let Some(caps) = conn.map(|c| c.caps()) {
        v.retain(|it| match it.label.as_str() {
            "Explain" | "Explain analyze" | "Copy plan as text" | "Plan: tree / flame" => caps.explain,
            "Import data…" => caps.import,
            "Activity monitor" => caps.activity,
            _ => true,
        });
    }
    let projects = crate::project_ui::projects(cx);
    v.retain(|it| match it.label.as_str() {
        "Open project…" => projects.enabled,
        "Close project" => projects.open.is_some(),
        _ => true,
    });
    if let Some(feed) = crate::updates::offered(cx) {
        let notes = feed.notes.lines().next().unwrap_or("").to_string();
        v.push(c(&format!("Update to {}…", feed.version), &notes, "", Box::new(ShowUpdate), Workspace));
    }
    if crate::updates::enabled(cx) {
        v.push(c("Check for updates…", crate::VERSION, "", Box::new(CheckForUpdates), Workspace));
    }
    if let Some(conn) = conn.filter(|c| c.info.is_lar) {
        let registry = cx.try_global::<crate::panels::PanelRegistry>();
        let actions: [Box<dyn Action>; 1] = [Box::new(LarPanel1)];
        for ((id, label, ic, key, what), a) in crate::panels::LAR_BUTTONS.into_iter().zip(actions) {
            let on = registry.is_some_and(|r| r.find(&conn.info, id).is_some());
            let mut it = c(&format!("LAR · {label}"), if on { what } else { "coming in a later phase" }, key, a, Workspace);
            it.icon = PaletteIcon::Lar(ic);
            it.lar = true;
            it.disabled = !on;
            v.push(it);
        }
        let mut it = c("LAR · Pause / resume live", "the tab's SUBSCRIBE or LISTEN … AS", "⌥P", Box::new(crate::lar::PauseLive), Workspace);
        it.icon = PaletteIcon::Lar(crate::assets::Icon::Pause);
        it.lar = true;
        v.push(it);
    }
    if crate::keymap::LINUX {
        // No menu bar there: what only it reached.
        v.push(c("Go to table or tab", "", "", Box::new(GoTo), Workspace));
        v.push(c("Next region", "explorer → editor → grid", "", Box::new(CycleFocus), Workspace));
        if !crate::theme::translucency_works() {
            v.retain(|i| !i.label.starts_with("Transparency"));
        }
    }
    v
}

/// Where a connection points: the file of a file database, else `user@host:port`.
fn where_of(p: &dataman_core::ConnectionProfile) -> String {
    if p.engine.is_file() { p.summary() } else { format!("{}@{}:{}", p.user, p.host, p.port) }
}
