//! Suggestions under Privacy's two fields (Sensitive columns, Visible to agents): schemas, tables
//! and columns (collections, indices and fields on the document engines) from what the explorer
//! already read of the connection, fetching the rest in the background. A lean cousin of the
//! editor's completion: ↑ ↓ to move, Tab or ↩ to take one, Esc to close.

use dataman_core::Engine;
use dataman_core::nosql::ItemKind as TreeKind;
use dataman_core::privacy::ItemKind;
use gpui::{AnyElement, Context, Focusable, KeyDownEvent, SharedString, Window, deferred, div, prelude::*};

use crate::agents::AgentsPage;
use crate::assets::{Icon, icon};
use crate::theme::{Theme, px};

const MAX: usize = 8;

gpui::actions!(agents_suggest, [CloseSuggest]);

const CONTEXT: &str = "AgentsSuggest";

/// Esc closes the suggestions before the workspace's Esc takes the focus away.
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([crate::keymap::kb("escape", CloseSuggest, Some(CONTEXT))]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    Sensitive,
    Visible,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Choice {
    pub text: String,
    pub kind: ItemKind,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Suggest {
    pub which: Option<Which>,
    pub items: Vec<Choice>,
    pub sel: usize,
}

fn rank(name: &str, q: &str) -> Option<u8> {
    let (n, q) = (name.to_lowercase(), q.to_lowercase());
    if q.is_empty() || n.starts_with(&q) {
        Some(0)
    } else if n.contains(&q) {
        Some(1)
    } else {
        None
    }
}

fn system(s: &str) -> bool {
    let l = s.to_lowercase();
    matches!(l.as_str(), "information_schema" | "pg_catalog" | "sys" | "mysql" | "performance_schema" | "admin" | "local" | "config") || l.starts_with("pg_toast") || l.starts_with("pg_temp")
}

impl AgentsPage {
    fn input_of(&self, w: Which) -> &gpui::Entity<crate::input::TextInput> {
        match w {
            Which::Sensitive => &self.sens_input,
            Which::Visible => &self.vis_input,
        }
    }

    /// Reads the field again and lists what fits it.
    pub(crate) fn update_suggest(&mut self, w: Which, cx: &mut Context<Self>) {
        let text = self.input_of(w).read(cx).text().trim().to_string();
        let items = if text.is_empty() { Vec::new() } else { self.choices(w, &text, cx) };
        let sel = if self.suggest.which == Some(w) { self.suggest.sel.min(items.len().saturating_sub(1)) } else { 0 };
        self.suggest = Suggest { which: (!items.is_empty()).then_some(w), items, sel };
        cx.notify();
    }

    pub(crate) fn close_suggest(&mut self, cx: &mut Context<Self>) {
        if self.suggest.which.take().is_some() {
            cx.notify();
        }
    }

    /// Takes the highlighted choice into the field; a table taken in Sensitive columns goes on to
    /// its columns.
    pub(crate) fn accept_suggest(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(w) = self.suggest.which else { return false };
        let Some(c) = self.suggest.items.get(self.suggest.sel).cloned() else { return false };
        let typed = self.input_of(w).read(cx).text().trim().to_string();
        if typed == c.text {
            self.close_suggest(cx);
            return false;
        }
        self.input_of(w).update(cx, |i, cx| i.set_text(c.text.clone(), cx));
        if c.text.ends_with('.') {
            self.update_suggest(w, cx);
        } else {
            self.suggest = Suggest::default();
            cx.notify();
        }
        true
    }

    pub(crate) fn suggest_key(&mut self, w: Which, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.suggest.which != Some(w) || self.suggest.items.is_empty() {
            return false;
        }
        let n = self.suggest.items.len();
        match ev.keystroke.key.as_str() {
            "down" => self.suggest.sel = (self.suggest.sel + 1) % n,
            "up" => self.suggest.sel = (self.suggest.sel + n - 1) % n,
            "tab" => {
                self.accept_suggest(cx);
            }
            "escape" => self.close_suggest(cx),
            _ => return false,
        }
        cx.notify();
        true
    }

    fn choices(&mut self, w: Which, text: &str, cx: &mut Context<Self>) -> Vec<Choice> {
        let Some(p) = self.selected_profile(cx) else { return Vec::new() };
        let Some(conn) = self.store.read(cx).connection(&p.id) else { return Vec::new() };
        let db = conn.database().to_string();
        let taken: Vec<String> = match w {
            Which::Sensitive => p.mcp_rules.sensitive.iter().map(|s| s.col.to_lowercase()).collect(),
            Which::Visible => p.mcp_rules.visible.items.iter().map(|i| i.name.to_lowercase()).collect(),
        };
        let mut out: Vec<(u8, Choice)> = Vec::new();
        let mut push = |rank: Option<u8>, c: Choice| {
            if let Some(r) = rank
                && !taken.contains(&c.text.to_lowercase())
                && !out.iter().any(|(_, x)| x.text == c.text)
            {
                out.push((r, c));
            }
        };
        let (head, tail) = text.rsplit_once('.').map_or((None, text), |(h, t)| (Some(h), t));
        if p.engine.is_sql() {
            let schemas: Vec<String> = conn.schema.cached_schemas(&db).unwrap_or_default().into_iter().filter(|s| !system(s)).collect();
            if schemas.is_empty() {
                self.load(&p.id, "schemas", cx, {
                    let (conn, db) = (conn.clone(), db.clone());
                    async move { conn.schemas(&db).await.map(|_| ()) }
                });
            }
            let mut tables: Vec<(String, String)> = Vec::new();
            for s in &schemas {
                match conn.schema.cached_relations(&db, s) {
                    Some(rels) => tables.extend(rels.into_iter().map(|r| (s.clone(), r.name))),
                    None => self.load(&p.id, &format!("rels:{s}"), cx, {
                        let (conn, db, s) = (conn.clone(), db.clone(), s.clone());
                        async move { conn.relations(&db, &s).await.map(|_| ()) }
                    }),
                }
            }
            let table_of = |h: &str| -> Option<(String, String)> {
                let (hs, ht) = h.split_once('.').map_or((None, h), |(a, b)| (Some(a), b));
                tables.iter().find(|(s, t)| t.eq_ignore_ascii_case(ht) && hs.is_none_or(|hs| s.eq_ignore_ascii_case(hs))).cloned()
            };
            match head {
                // `schema.` → its tables; `table.` (or `schema.table.`) → its columns.
                Some(h) if schemas.iter().any(|s| s.eq_ignore_ascii_case(h)) && w == Which::Visible => {
                    for (s, t) in tables.iter().filter(|(s, _)| s.eq_ignore_ascii_case(h)) {
                        push(rank(t, tail), Choice { text: format!("{s}.{t}"), kind: ItemKind::Table, detail: "table".into() });
                    }
                    push(rank("*", tail).or(Some(1)), Choice { text: format!("{h}.*"), kind: ItemKind::Schema, detail: "schema".into() });
                }
                Some(h) => {
                    if let Some((s, t)) = table_of(h) {
                        match conn.schema.cached_columns(&db, &s, &t) {
                            Some(cols) => {
                                for c in cols {
                                    push(rank(&c.name, tail), Choice { text: format!("{h}.{}", c.name), kind: ItemKind::Column, detail: c.type_name.clone() });
                                }
                            }
                            None => self.load(&p.id, &format!("cols:{s}.{t}"), cx, {
                                let (conn, db) = (conn.clone(), db.clone());
                                async move { conn.columns(&db, &s, &t).await.map(|_| ()) }
                            }),
                        }
                    } else if w == Which::Sensitive && schemas.iter().any(|s| s.eq_ignore_ascii_case(h)) {
                        for (_, t) in tables.iter().filter(|(s, _)| s.eq_ignore_ascii_case(h)) {
                            push(rank(t, tail), Choice { text: format!("{h}.{t}."), kind: ItemKind::Table, detail: "table".into() });
                        }
                    }
                }
                None => {
                    for (s, t) in &tables {
                        let text = if w == Which::Visible { format!("{s}.{t}") } else { format!("{t}.") };
                        push(rank(t, tail), Choice { text, kind: ItemKind::Table, detail: "table".into() });
                    }
                    if w == Which::Visible {
                        for s in &schemas {
                            push(rank(s, tail).map(|r| r + 1), Choice { text: format!("{s}.*"), kind: ItemKind::Schema, detail: "schema".into() });
                        }
                    }
                    // Columns whose own name fits, from tables already read.
                    for (s, t) in &tables {
                        for c in conn.schema.cached_columns(&db, s, t).unwrap_or_default() {
                            push(rank(&c.name, tail).map(|r| r + 2), Choice { text: format!("{t}.{}", c.name), kind: ItemKind::Column, detail: c.type_name.clone() });
                        }
                    }
                }
            }
        } else if p.engine != Engine::Redis {
            let names: Vec<String> = [conn.cached_tree(Some(&db)), conn.cached_tree(None)]
                .into_iter()
                .flatten()
                .flat_map(|items| items.iter().filter(|i| i.kind != TreeKind::Database).map(|i| i.name.clone()).collect::<Vec<_>>())
                .collect();
            if names.is_empty() {
                self.load(&p.id, "tree", cx, {
                    let (conn, db) = (conn.clone(), db.clone());
                    async move {
                        let d = if conn.engine() == Engine::MongoDb { Some(db.as_str()) } else { None };
                        conn.nosql_tree(d).await.map(|_| ())
                    }
                });
            }
            match head {
                Some(h) => {
                    let key = format!("{}\u{0}{h}", p.id);
                    match self.fields.get(&key) {
                        Some(fields) => {
                            for (f, ty) in fields {
                                push(rank(f, tail), Choice { text: format!("{h}.{f}"), kind: ItemKind::Column, detail: ty.clone() });
                            }
                        }
                        None if names.iter().any(|n| n == h) => {
                            let (conn, db, coll) = (conn.clone(), db.clone(), h.to_string());
                            if self.loading.insert(key.clone()) {
                                cx.spawn(async move |this, cx| {
                                    let fields = conn.sample_fields(&db, &coll).await.unwrap_or_default();
                                    let _ = this.update(cx, |v, cx| {
                                        v.loading.remove(&key);
                                        v.fields.insert(key, fields.into_iter().map(|(p, t)| (p, t.join(" | "))).collect());
                                        v.refresh_suggest(cx);
                                    });
                                })
                                .detach();
                            }
                        }
                        None => {}
                    }
                }
                None => {
                    for n in &names {
                        let text = if w == Which::Visible { n.clone() } else { format!("{n}.") };
                        push(rank(n, tail), Choice { text, kind: ItemKind::Table, detail: if p.engine == Engine::MongoDb { "collection".into() } else { "index".into() } });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.text.len().cmp(&b.1.text.len())).then(a.1.text.cmp(&b.1.text)));
        out.into_iter().take(MAX).map(|(_, c)| c).collect()
    }

    /// Fetches something the suggestions need once, then lists again.
    fn load(&mut self, conn: &str, what: &str, cx: &mut Context<Self>, fut: impl std::future::Future<Output = anyhow::Result<()>> + 'static) {
        let key = format!("{conn}\u{0}{what}");
        if !self.loading.insert(key.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let ok = fut.await.is_ok();
            let _ = this.update(cx, |v, cx| {
                // A failed read stays marked, so it is not asked again on every keystroke.
                if ok {
                    v.loading.remove(&key);
                }
                v.refresh_suggest(cx);
            });
        })
        .detach();
    }

    fn refresh_suggest(&mut self, cx: &mut Context<Self>) {
        for w in [Which::Sensitive, Which::Visible] {
            if self.suggest.which == Some(w) || !self.input_of(w).read(cx).text().trim().is_empty() {
                self.update_suggest(w, cx);
            }
        }
    }

    /// The field with its suggestions under it (while it has the focus).
    pub(crate) fn suggest_field(&self, t: &Theme, w: Which, field: gpui::Div, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.input_of(w).read(cx).focus_handle(cx).is_focused(window);
        let open = focused && self.suggest.which == Some(w) && !self.suggest.items.is_empty();
        let rows = self.suggest.items.iter().enumerate().map(|(k, c)| {
            let sel = k == self.suggest.sel;
            let hov = t.hov;
            let glyph = match c.kind {
                ItemKind::Column => Icon::Column,
                ItemKind::Schema => Icon::Database,
                ItemKind::Table => Icon::GridTable,
            };
            div()
                .id(SharedString::from(format!("agents-suggest-{k}")))
                .h(px(26.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .when(sel, |d| d.bg(t.prs))
                .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                .child(icon(glyph).size(px(13.)).text_color(t.tx3))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(c.text.clone()))
                .child(div().flex_none().max_w(px(120.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(t.tx3).child(c.detail.clone()))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |v, _, window, cx| {
                    v.suggest.sel = k;
                    v.accept_suggest(cx);
                    let input = v.input_of(w).clone();
                    window.focus(&input.read(cx).focus_handle(cx), cx);
                    cx.stop_propagation();
                }))
        });
        div()
            .flex_1()
            .relative()
            .when(open, |d| d.key_context(CONTEXT))
            .on_action(cx.listener(|v, _: &CloseSuggest, _, cx| v.close_suggest(cx)))
            .capture_key_down(cx.listener(move |v, ev: &KeyDownEvent, _, cx| {
                if v.suggest_key(w, ev, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(field)
            .when(open, |d| {
                d.child(
                    deferred(
                        div()
                            .id(match w {
                                Which::Sensitive => "agents-suggest-sensitive",
                                Which::Visible => "agents-suggest-visible",
                            })
                            .absolute()
                            .top(px(31.))
                            .left_0()
                            .right_0()
                            .py(px(4.))
                            .bg(t.pop)
                            .shadow(t.ring())
                            .text_size(px(12.))
                            .occlude()
                            .children(rows),
                    )
                    .with_priority(1),
                )
            })
            .into_any_element()
    }
}
