//! The key map (Shortcuts board) and the ⇧⌘/ sheet that lists it. Only keys that do something
//! today are listed; the LAR group (violet) works on LAR servers. `{id}` in a key stands for a
//! remappable command's current key (`keymap`).

use gpui::{Div, div, prelude::*};
use crate::theme::px;

use crate::theme::{PIXEL, Theme};
use crate::ui;

pub const GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "GLOBAL",
        &[
            ("{command_palette}", "command palette"),
            ("{go_to}", "go to table · tab"),
            ("{new_connection}", "new connection"),
            ("{new_tab}", "new query tab"),
            ("{close_tab}", "close tab"),
            ("{tab_1}–{tab_9}", "jump to tab"),
            ("{next_tab}", "next tab"),
            ("{go_back} {go_forward}", "back · forward"),
            ("{toggle_explorer}", "toggle explorer"),
            ("{toggle_side_panel}", "toggle side panel"),
            ("{open_sql_file}", "open a .sql file"),
            ("{zoom_in} {zoom_out} {zoom_reset}", "zoom"),
            ("{settings}", "settings"),
            ("{shortcuts}", "all shortcuts"),
        ],
    ),
    (
        "EDITOR",
        &[
            ("{run_all}", "run script / selection"),
            ("{run}", "run statement"),
            ("{run_from_here}", "run from statement"),
            ("{explain} · {explain_analyze}", "explain · explain analyze"),
            ("{cancel}", "cancel on server"),
            ("{commit_tx} · {rollback_tx}", "commit · rollback"),
            ("{prev_result} {next_result}", "previous · next result"),
            ("⌃Space", "complete"),
            ("{format_sql}", "format SQL"),
            ("{go_to_definition} · ⌘-click", "go to definition"),
            ("{open_definition}", "open definition"),
            ("{jump_to_error}", "jump to error"),
            ("{use_explorer_connection}", "use explorer conn"),
            ("{history}", "history"),
            ("{favorite_query}", "favorite query"),
            ("{toggle_comment}", "comment line"),
        ],
    ),
    (
        "GRID",
        &[
            ("↑↓←→", "move cell"),
            ("⇧ + arrows", "extend range"),
            ("{copy_tsv}", "copy TSV"),
            ("{copy_with_headers}", "copy with headers"),
            ("{copy_csv}", "copy CSV"),
            ("{cell_viewer}", "view JSON / array value"),
            ("{prev_page} {next_page}", "previous · next page"),
            ("{fetch_more}", "fetch next rows"),
            ("{fetch_all}", "fetch all"),
            ("↩", "edit cell"),
            ("⌥↓ · ⌘-click", "go to referenced row"),
            ("{insert_row} · ⌫", "insert · delete row"),
            ("{review_changes}", "review & commit"),
            ("{discard_changes}", "discard edits"),
            ("{export}", "export"),
        ],
    ),
    (
        "CELL EDIT",
        &[
            ("esc", "cancel the cell"),
            ("⇥ / ⇧⇥", "next · previous cell"),
            ("↩ / ↑↓", "keep and move"),
            ("⌘⇧N", "set NULL"),
        ],
    ),
    (
        "FOCUS",
        &[
            ("{cycle_focus}", "next region"),
            ("{cycle_focus_back}", "previous region"),
            ("{focus_explorer}", "explorer"),
            ("{focus_grid}", "grid"),
            ("esc", "editor"),
        ],
    ),
    (
        "EXPLORER",
        &[
            ("↑↓", "move"),
            ("→ ←", "expand · collapse"),
            ("/", "filter tree"),
            ("↩", "table data in new tab"),
            ("⇧F10 / ⌃-click", "row actions menu"),
            ("⌘R", "reload node"),
            ("⌘I", "edit connection"),
            ("⌘⌫", "disconnect"),
            ("esc", "clear filter → editor"),
        ],
    ),
    (
        "◆ LAR",
        &[
            ("{lar_live}", "live queries"),
            ("{lar_pause}", "pause · resume live"),
        ],
    ),
    (
        "FIND",
        &[
            ("⌘F", "find in editor · grid"),
            ("⌥⌘F", "find & replace"),
            ("⌘G · ↩", "next match"),
            ("⇧⌘G · ⇧↩", "previous match"),
        ],
    ),
    (
        "PLAN",
        &[
            ("↑↓", "move node"),
            ("← →", "collapse · expand"),
        ],
    ),
    (
        "ACTIVITY",
        &[
            ("⌘R", "read now"),
            ("↑↓", "move in locks"),
            ("↩", "details"),
        ],
    ),
    (
        "SHEETS",
        &[
            ("⌘↩", "confirm · run · save"),
            ("⌘⌫", "rollback · discard"),
            ("esc", "cancel · close"),
            ("⌘T", "test connection"),
            ("⌘.", "stop import · export"),
            ("⇥ / ⇧⇥", "next · previous field"),
        ],
    ),
    (
        "MCP",
        &[
            ("⌘↩", "allow agent write"),
            ("esc", "deny"),
            ("⌘R", "reload log"),
        ],
    ),
];

/// The sheet's columns, as indices into [`GROUPS`]: short groups share a column.
const COLUMNS: &[&[usize]] = &[&[0, 9, 8], &[1, 7], &[2, 11], &[3, 4, 10], &[5, 6]];

/// `template` with each `{id}` as `key(id)`; separators left dangling by a removed key go.
fn resolve_with(template: &str, key: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else { break };
        out.push_str(&key(&rest[i + 1..i + j]));
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    let mut words: Vec<&str> = Vec::new();
    for w in out.split_whitespace() {
        let sep = w == "·";
        if !(sep && words.last().is_none_or(|l| *l == "·")) {
            words.push(w);
        }
    }
    while words.last() == Some(&"·") {
        words.pop();
    }
    words.join(" ").trim_matches('–').to_string()
}

/// The keys of one row of [`GROUPS`] as they are now.
pub fn keys(template: &str) -> String {
    resolve_with(template, crate::keymap::key_label)
}

fn group(t: &Theme, title: &str, keys: &[(&str, &str)]) -> Div {
    let lar = title.starts_with('◆');
    let rows: Vec<(String, &str)> = keys.iter().map(|(k, d)| (self::keys(k), *d)).filter(|(k, _)| !k.is_empty()).collect();
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().pb(px(8.)).text_size(px(10.5)).text_color(if lar { t.lar } else { t.tx3 }).child(title.to_string()))
        .children(rows.into_iter().map(|(k, d)| {
            div()
                .min_h(px(26.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b_1()
                .border_color(t.line)
                // A narrow column (a small window, a zoomed UI) cuts the words, never the key.
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(t.tx2).child(d.to_string()))
                .child(div().flex_none().child(ui::keycap(t, k).when(lar, |d| d.text_color(t.lar_t).border_color(t.lar_r))))
        }))
}

/// The sheet, with `close` (the modals' ×) beside its title.
pub fn sheet(t: &Theme, close: impl IntoElement) -> gpui::Stateful<Div> {
    crate::ui::modal(t, "shortcuts-sheet")
        .w(px(1260.))
        .max_w(gpui::relative(0.96))
        .p(px(28.))
        .gap(px(22.))
        .child(div().flex().flex_row().items_center().child(div().flex_1().font_family(PIXEL).text_size(px(26.)).child("KEYS & FOCUS")).child(close))
        // Taller than the window (a zoomed UI): the columns scroll under the title.
        .child(div().id("shortcuts-body").flex_1().min_h_0().overflow_y_scroll().child(div().flex().flex_row().gap(px(18.)).children(COLUMNS.iter().map(|ixs| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(22.))
                .children(ixs.iter().map(|&i| group(t, GROUPS[i].0, GROUPS[i].1)))
        }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_on_the_sheet_once() {
        let mut seen: Vec<usize> = COLUMNS.iter().flat_map(|c| c.iter().copied()).collect();
        seen.sort();
        assert_eq!(seen, (0..GROUPS.len()).collect::<Vec<_>>());
    }

    fn defaults(id: &str) -> String {
        crate::keymap::find(id).map(|c| crate::keymap::label(c.keys[0])).expect(id)
    }

    #[test]
    fn features_have_their_keys() {
        let has = |group: &str, key: &str| GROUPS.iter().any(|(g, keys)| *g == group && keys.iter().any(|(k, _)| resolve_with(k, defaults) == key));
        // Activity monitor, plan, confirmation sheets, MCP approval, parameters, find, results.
        for (g, k) in [("ACTIVITY", "⌘R"), ("ACTIVITY", "↩"), ("PLAN", "← →"), ("SHEETS", "⌘⌫"), ("SHEETS", "⌘T"), ("MCP", "⌘↩"), ("SHEETS", "⇥ / ⇧⇥"), ("FIND", "⌥⌘F"), ("EDITOR", "⌥⌘[ ⌥⌘]")] {
            assert!(has(g, k), "{g} {k}");
        }
        // The copy keys: ⇧⌘C with headers, ⌥⌘C CSV.
        assert!(has("GRID", "⇧⌘C") && has("GRID", "⌥⌘C"));
    }

    #[test]
    fn every_placeholder_is_a_command_and_removed_keys_leave_no_separators() {
        for (_, keys) in GROUPS {
            for (k, _) in *keys {
                resolve_with(k, |id| {
                    assert!(crate::keymap::find(id).is_some(), "{id}");
                    String::new()
                });
            }
        }
        let gone = |id: &str| if id == "explain" { String::new() } else { defaults(id) };
        assert_eq!(resolve_with("{explain} · {explain_analyze}", gone), "⌥⌘L");
        assert_eq!(resolve_with("{explain_analyze} · {explain}", gone), "⌥⌘L");
        assert_eq!(resolve_with("{explain}", gone), "");
        assert_eq!(resolve_with("{tab_1}–{tab_9}", defaults), "⌘1–⌘9");
    }
}
