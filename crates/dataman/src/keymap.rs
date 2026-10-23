// Command ids are the keys of `[keys]` in settings.toml: never rename one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use gpui::{Action, App, KeyBinding, Keystroke};

pub use crate::keys::*;

use crate::app::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    App,
    Workspace,
    Editor,
    Grid,
}

impl Scope {
    const fn context(self) -> Option<&'static str> {
        match self {
            Scope::App => None,
            Scope::Workspace => Some(WORKSPACE_KEYS),
            Scope::Editor => Some("CodeEditor"),
            Scope::Grid => Some(crate::grid::GRID_KEYS),
        }
    }

    pub fn overlaps(self, o: Scope) -> bool {
        self == o || matches!(self, Scope::App | Scope::Workspace) || matches!(o, Scope::App | Scope::Workspace)
    }
}

pub struct Command {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub keys: &'static [&'static str],
    pub scope: Scope,
    context: Option<&'static str>,
    action: fn() -> Box<dyn Action>,
}

const fn c(id: &'static str, label: &'static str, group: &'static str, scope: Scope, keys: &'static [&'static str], action: fn() -> Box<dyn Action>) -> Command {
    Command { id, label, group, keys, scope, context: scope.context(), action }
}

// ⌘↓ is Ctrl+End on Linux, the grid's last row there.
const FETCH_MORE: &[&str] = &[if LINUX { "ctrl-down" } else { "cmd-down" }];
const FETCH_ALL: &[&str] = &[if LINUX { "ctrl-shift-down" } else { "cmd-shift-down" }];

pub const COMMANDS: &[Command] = &[
    c("command_palette", "Command palette", "GLOBAL", Scope::Workspace, &["cmd-k"], || Box::new(CommandPalette)),
    c("go_to", "Go to table or tab", "GLOBAL", Scope::Workspace, &["cmd-p"], || Box::new(GoTo)),
    c("new_connection", "New connection", "GLOBAL", Scope::Workspace, &["cmd-n"], || Box::new(NewConnection)),
    c("new_tab", "New query tab", "GLOBAL", Scope::Workspace, &["cmd-t"], || Box::new(NewTab)),
    c("close_tab", "Close tab", "GLOBAL", Scope::Workspace, &["cmd-w"], || Box::new(CloseTab)),
    Command { context: Some(crate::undo::KEYS), ..c("undo", "Undo", "GLOBAL", Scope::Workspace, &["cmd-z", "ctrl-z"], || Box::new(crate::undo::Undo)) },
    Command { context: Some(crate::undo::KEYS), ..c("redo", "Redo", "GLOBAL", Scope::Workspace, &["cmd-shift-z", "ctrl-shift-z"], || Box::new(crate::undo::Redo)) },
    c("open_to_side", "Open to the side", "GLOBAL", Scope::Workspace, &["cmd-\\"], || Box::new(crate::split::OpenToSide)),
    c("next_tab", "Next tab", "GLOBAL", Scope::Workspace, &["ctrl-tab", "cmd-shift-]"], || Box::new(NextTab)),
    c("prev_tab", "Previous tab", "GLOBAL", Scope::Workspace, &["ctrl-shift-tab", "cmd-shift-["], || Box::new(PrevTab)),
    c("tab_1", "Tab 1", "GLOBAL", Scope::Workspace, &["cmd-1"], || Box::new(Tab1)),
    c("tab_2", "Tab 2", "GLOBAL", Scope::Workspace, &["cmd-2"], || Box::new(Tab2)),
    c("tab_3", "Tab 3", "GLOBAL", Scope::Workspace, &["cmd-3"], || Box::new(Tab3)),
    c("tab_4", "Tab 4", "GLOBAL", Scope::Workspace, &["cmd-4"], || Box::new(Tab4)),
    c("tab_5", "Tab 5", "GLOBAL", Scope::Workspace, &["cmd-5"], || Box::new(Tab5)),
    c("tab_6", "Tab 6", "GLOBAL", Scope::Workspace, &["cmd-6"], || Box::new(Tab6)),
    c("tab_7", "Tab 7", "GLOBAL", Scope::Workspace, &["cmd-7"], || Box::new(Tab7)),
    c("tab_8", "Tab 8", "GLOBAL", Scope::Workspace, &["cmd-8"], || Box::new(Tab8)),
    c("tab_9", "Tab 9", "GLOBAL", Scope::Workspace, &["cmd-9"], || Box::new(Tab9)),
    // ⌃⌘ (Xcode's): plain ⌘← ⌘→ are a text's line start / end and the grid's row ends.
    c("go_back", "Back", "GLOBAL", Scope::Workspace, &["ctrl-cmd-left"], || Box::new(GoBack)),
    c("go_forward", "Forward", "GLOBAL", Scope::Workspace, &["ctrl-cmd-right"], || Box::new(GoForward)),
    c("toggle_explorer", "Toggle sidebar", "GLOBAL", Scope::Workspace, &["cmd-b"], || Box::new(ToggleExplorer)),
    c("toggle_side_panel", "Toggle side panel", "GLOBAL", Scope::Workspace, &["cmd-alt-b"], || Box::new(ToggleSidePanel)),
    c("open_sql_file", "Open .sql file", "GLOBAL", Scope::Workspace, &["cmd-o"], || Box::new(OpenSqlFile)),
    c("refresh", "Reload schema", "GLOBAL", Scope::Workspace, &["cmd-r"], || Box::new(Refresh)),
    c("zoom_in", "Zoom in", "GLOBAL", Scope::App, &["cmd-=", "cmd-+", "cmd-shift-="], || Box::new(ZoomIn)),
    c("zoom_out", "Zoom out", "GLOBAL", Scope::App, &["cmd--"], || Box::new(ZoomOut)),
    c("zoom_reset", "Actual size", "GLOBAL", Scope::App, &["cmd-0"], || Box::new(ZoomReset)),
    c("settings", "Settings", "GLOBAL", Scope::Workspace, &["cmd-,"], || Box::new(OpenSettings)),
    c("shortcuts", "All shortcuts", "GLOBAL", Scope::Workspace, &["cmd-shift-/", "cmd-?"], || Box::new(ShowShortcuts)),
    c("quit", "Quit", "GLOBAL", Scope::App, &["cmd-q"], || Box::new(Quit)),
    // The ids predate the swap of 15/10 (⌘↩ ran the statement): a saved override stays with its
    // command.
    c("run_all", "Run", "EDITOR", Scope::Editor, &["cmd-enter"], || Box::new(crate::editor::RunScript)),
    c("run", "Run statement", "EDITOR", Scope::Editor, &["cmd-shift-enter"], || Box::new(crate::editor::RunStatement)),
    c("run_from_here", "Run from statement", "EDITOR", Scope::Editor, &["cmd-alt-enter"], || Box::new(crate::editor::RunFromHere)),
    c("explain", "Explain", "EDITOR", Scope::Workspace, &["cmd-l"], || Box::new(Explain)),
    c("explain_analyze", "Explain analyze", "EDITOR", Scope::Workspace, &["cmd-alt-l"], || Box::new(ExplainAnalyze)),
    c("cancel", "Cancel on server", "EDITOR", Scope::Workspace, &["cmd-."], || Box::new(Cancel)),
    c("commit_tx", "Commit", "EDITOR", Scope::Workspace, &["ctrl-cmd-enter"], || Box::new(CommitTx)),
    c("rollback_tx", "Rollback", "EDITOR", Scope::Workspace, &["ctrl-cmd-backspace"], || Box::new(RollbackTx)),
    c("next_result", "Next result", "EDITOR", Scope::Workspace, &["cmd-alt-]"], || Box::new(NextResult)),
    c("prev_result", "Previous result", "EDITOR", Scope::Workspace, &["cmd-alt-["], || Box::new(PrevResult)),
    c("format_sql", "Format query", "EDITOR", Scope::Editor, &["alt-shift-f"], || Box::new(crate::editor::FormatQuery)),
    c("toggle_comment", "Comment line", "EDITOR", Scope::Editor, &["cmd-/"], || Box::new(crate::editor::ToggleComment)),
    // VS Code's (Windows, Linux): ⌥⌘[ and ⌥⌘] are the results' here, ⌘K the palette.
    c("fold", "Fold", "EDITOR", Scope::Editor, &["ctrl-shift-["], || Box::new(crate::editor::Fold)),
    c("unfold", "Unfold", "EDITOR", Scope::Editor, &["ctrl-shift-]"], || Box::new(crate::editor::Unfold)),
    c("fold_all", "Fold all", "EDITOR", Scope::Editor, &["ctrl-alt-["], || Box::new(crate::editor::FoldAll)),
    c("unfold_all", "Unfold all", "EDITOR", Scope::Editor, &["ctrl-alt-]"], || Box::new(crate::editor::UnfoldAll)),
    c("fold_level_1", "Fold level 1", "EDITOR", Scope::Editor, &["ctrl-alt-1"], || Box::new(crate::editor::FoldLevel1)),
    c("fold_level_2", "Fold level 2", "EDITOR", Scope::Editor, &["ctrl-alt-2"], || Box::new(crate::editor::FoldLevel2)),
    c("fold_level_3", "Fold level 3", "EDITOR", Scope::Editor, &["ctrl-alt-3"], || Box::new(crate::editor::FoldLevel3)),
    c("go_to_definition", "Go to definition", "EDITOR", Scope::Editor, &["f12"], || Box::new(crate::editor::GoToDefinition)),
    c("open_definition", "Open definition", "EDITOR", Scope::Editor, &["alt-f12"], || Box::new(crate::editor::PeekDefinition)),
    c("jump_to_error", "Jump to error", "EDITOR", Scope::Workspace, &["cmd-'"], || Box::new(JumpToError)),
    c("use_explorer_connection", "Use explorer connection", "EDITOR", Scope::Workspace, &["alt-enter"], || Box::new(UseExplorerConnection)),
    c("favorite_query", "Favorite query", "EDITOR", Scope::Workspace, &["cmd-d"], || Box::new(FavoriteQuery)),
    c("save_favorite", "Save as favorite", "EDITOR", Scope::Editor, &["cmd-s"], || Box::new(SaveFavorite)),
    c("save_as", "Save as…", "EDITOR", Scope::Workspace, &["cmd-shift-s"], || Box::new(SaveAs)),
    c("copy_tsv", "Copy TSV", "GRID", Scope::Grid, &["cmd-c"], || Box::new(crate::grid::CopyTsv)),
    c("copy_with_headers", "Copy with headers", "GRID", Scope::Grid, &["cmd-shift-c"], || Box::new(crate::grid::CopyTsvWithHeader)),
    c("copy_csv", "Copy CSV", "GRID", Scope::Grid, &["cmd-alt-c"], || Box::new(crate::grid::CopyCsv)),
    // (The id is the cell viewer's: a key set for it in settings.toml keeps working.)
    c("cell_viewer", "View value", "GRID", Scope::Grid, &["shift-space"], || Box::new(crate::grid::ViewValue)),
    c("next_page", "Next page", "GRID", Scope::Workspace, &["cmd-alt-right"], || Box::new(NextPage)),
    c("prev_page", "Previous page", "GRID", Scope::Workspace, &["cmd-alt-left"], || Box::new(PrevPage)),
    c("fetch_more", "Fetch next rows", "GRID", Scope::Grid, FETCH_MORE, || Box::new(crate::grid::FetchMore)),
    c("fetch_all", "Fetch all", "GRID", Scope::Grid, FETCH_ALL, || Box::new(crate::grid::FetchAll)),
    Command { context: Some("ResultGrid"), ..c("insert_row", "Insert row", "GRID", Scope::Grid, &["cmd-i"], || Box::new(crate::grid::InsertRow)) },
    Command { context: Some("ResultGrid"), ..c("review_changes", "Review & commit", "GRID", Scope::Grid, &["cmd-s"], || Box::new(ReviewChanges)) },
    c("discard_changes", "Discard edits", "GRID", Scope::Grid, &["cmd-backspace"], || Box::new(DiscardChanges)),
    c("export", "Export", "GRID", Scope::Workspace, &["cmd-e"], || Box::new(ExportResult)),
    c("zoom_result", "Zoom result", "GRID", Scope::Grid, &["cmd-shift-enter"], || Box::new(ToggleResultZoom)),
    c("cycle_focus", "Next region", "FOCUS", Scope::Workspace, &["f6"], || Box::new(CycleFocus)),
    c("cycle_focus_back", "Previous region", "FOCUS", Scope::Workspace, &["shift-f6"], || Box::new(CycleFocusBack)),
    c("focus_explorer", "Explorer", "FOCUS", Scope::Workspace, &["cmd-shift-e"], || Box::new(FocusExplorer)),
    c("focus_grid", "Grid", "FOCUS", Scope::Workspace, &["cmd-shift-g"], || Box::new(FocusGrid)),
    c("lar_live", "Live queries", "◆ LAR", Scope::Workspace, &["alt-1"], || Box::new(LarPanel1)),
    c("lar_pause", "Pause · resume live", "◆ LAR", Scope::Workspace, &["alt-p"], || Box::new(crate::lar::PauseLive)),
];

pub fn find(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

pub fn parse(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let k = Keystroke::parse(s).ok()?;
    if k.key.is_empty() || matches!(k.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
        return None;
    }
    Some(k.unparse())
}

pub fn effective_in(overrides: &BTreeMap<String, String>, cmd: &Command) -> Vec<String> {
    match overrides.get(cmd.id).map(|s| s.trim()) {
        Some("") => Vec::new(),
        Some(s) => match parse(s).and_then(|k| parse(&native_chosen(&k))) {
            Some(k) => vec![k],
            None => defaults(cmd),
        },
        None => defaults(cmd),
    }
}

fn defaults(cmd: &Command) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in cmd.keys.iter().filter_map(|k| parse(&native(k))) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

pub fn conflicts_in(overrides: &BTreeMap<String, String>) -> BTreeSet<&'static str> {
    let keys: Vec<Vec<String>> = COMMANDS.iter().map(|c| effective_in(overrides, c)).collect();
    let mut out = BTreeSet::new();
    for (i, a) in COMMANDS.iter().enumerate() {
        for (j, b) in COMMANDS.iter().enumerate().skip(i + 1) {
            if a.scope.overlaps(b.scope) && keys[i].iter().any(|k| keys[j].contains(k)) {
                out.insert(a.id);
                out.insert(b.id);
            }
        }
    }
    out
}

pub fn clean(overrides: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    overrides
        .iter()
        .filter_map(|(id, k)| {
            let cmd = find(id)?;
            let k = if k.trim().is_empty() { String::new() } else { parse(&native_chosen(&parse(k)?))? };
            let changes = if k.is_empty() { !cmd.keys.is_empty() } else { defaults(cmd) != [k.clone()] };
            changes.then(|| (id.clone(), k))
        })
        .collect()
}

/// A shortcut as people type it (⇧⌘P, Shift+Cmd+P, Ctrl+Shift+P), in this platform's keys.
pub fn parse_typed(s: &str) -> Option<String> {
    parse_typed_as_written(s).and_then(|k| parse(&native_chosen(&k)))
}

fn parse_typed_as_written(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    let mut mods: Vec<&str> = Vec::new();
    let mut rest = s.as_str();
    while let Some(c) = rest.chars().next() {
        let m = match c {
            '⌃' => "ctrl",
            '⌥' => "alt",
            '⇧' => "shift",
            '⌘' => "cmd",
            _ => break,
        };
        mods.push(m);
        rest = rest[c.len_utf8()..].trim_start();
    }
    if mods.is_empty() {
        let gpui = match s.strip_suffix("++") {
            Some(head) => format!("{}-+", head.replace('+', "-")),
            None => s.replace('+', "-"),
        };
        let gpui = match gpui.rsplit_once('-') {
            Some((head, tail)) if !head.is_empty() => format!("{head}-{}", typed_key(tail)),
            _ => gpui,
        };
        return gpui.contains('-').then(|| parse(&gpui)).flatten().filter(|k| k.contains('-'));
    }
    let key = typed_key(rest);
    if key.is_empty() {
        return None;
    }
    mods.push(key);
    parse(&mods.join("-"))
}


static OVERRIDES: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());

pub fn overrides() -> BTreeMap<String, String> {
    OVERRIDES.read().map(|m| m.clone()).unwrap_or_default()
}

pub fn set_overrides(map: BTreeMap<String, String>) {
    if let Ok(mut m) = OVERRIDES.write() {
        *m = map;
    }
}

pub fn effective(id: &str) -> Vec<String> {
    find(id).map(|c| OVERRIDES.read().map(|m| effective_in(&m, c)).unwrap_or_else(|_| defaults(c))).unwrap_or_default()
}

pub fn key_label(id: &str) -> String {
    effective(id).first().map(|k| label_native(k)).unwrap_or_default()
}

pub fn key(id: &str) -> Option<String> {
    Some(key_label(id)).filter(|k| !k.is_empty())
}

pub fn label_for_action(action: &dyn Action) -> Option<String> {
    COMMANDS.iter().find(|c| (c.action)().name() == action.name()).map(|c| key_label(c.id))
}

pub fn conflicts() -> BTreeSet<&'static str> {
    conflicts_in(&overrides())
}

/// The modules' fixed keys first, then the commands' (a later binding wins in the same
/// context; a command's first key is bound last, the one menus show).
pub fn bind_all(cx: &mut App) {
    cx.clear_key_bindings();
    crate::app::bind_keys(cx);
    crate::editor::bind_keys(cx);
    crate::grid::bind_keys(cx);
    crate::find::bind_keys(cx);
    crate::input::bind_keys(cx);
    crate::agents_suggest::bind_keys(cx);
    crate::agents::bind_keys(cx);
    crate::tree::bind_keys(cx);
    crate::row_actions::bind_keys(cx);
    crate::history::bind_keys(cx);
    crate::conn_dialog::bind_keys(cx);
    crate::doc_view::bind_keys(cx);
    crate::import_ui::bind_keys(cx);
    crate::welcome::bind_keys(cx);
    crate::palette::bind_keys(cx);
    crate::edit_ui::bind_keys(cx);
    crate::value_edit::bind_keys(cx);
    crate::value_view::bind_keys(cx);
    crate::table_view::bind_keys(cx);
    crate::export_ui::bind_keys(cx);
    crate::data_import::bind_keys(cx);
    crate::settings_view::bind_keys(cx);
    crate::params_ui::bind_keys(cx);
    crate::plan::bind_keys(cx);
    crate::activity::bind_keys(cx);
    crate::mcp::bind_keys(cx);
    crate::lar::bind_keys(cx);
    crate::update_ui::bind_keys(cx);
    let map = overrides();
    let mut bindings = Vec::new();
    for cmd in COMMANDS {
        let extra = (cmd.id == "zoom_result").then_some("DocView");
        for k in effective_in(&map, cmd).iter().rev() {
            for context in std::iter::once(cmd.context).chain(extra.map(Some)) {
                match KeyBinding::load(k, (cmd.action)(), context.map(|c| gpui::KeyBindingContextPredicate::parse(c).unwrap().into()), false, None, &gpui::DummyKeyboardMapper) {
                    Ok(b) => bindings.push(b),
                    Err(e) => log::warn!("key {k} for {}: {e}", cmd.id),
                }
            }
        }
    }
    cx.bind_keys(bindings);
}

pub fn apply(map: BTreeMap<String, String>, cx: &mut App) {
    set_overrides(clean(&map));
    bind_all(cx);
    crate::refresh_menus(cx);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        parse(&native(s)).unwrap()
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn ids_unique_and_defaults_parse() {
        let mut ids: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), COMMANDS.len());
        for c in COMMANDS {
            assert!(!c.keys.is_empty(), "{}", c.id);
            for k in c.keys {
                assert!(parse(k).is_some(), "{} {k}", c.id);
            }
            if let Some(ctx) = c.context {
                assert!(gpui::KeyBindingContextPredicate::parse(ctx).is_ok(), "{ctx}");
            }
        }
    }

    #[test]
    fn parse_canonical_and_rejects() {
        assert_eq!(parse("shift-cmd-c"), parse("cmd-shift-c"));
        assert_eq!(parse(" cmd-shift-c "), parse("cmd-shift-c"));
        assert_eq!(parse("ctrl-shift-c").as_deref(), Some("ctrl-shift-c"));
        #[cfg(target_os = "macos")]
        assert_eq!((parse("shift-cmd-c").as_deref(), parse("cmd--").as_deref()), (Some("cmd-shift-c"), Some("cmd--")));
        for bad in ["", "cmd-k cmd-s", "cmd-", "shift", "cmd-shift", "foo-bar-c", "cmd-x-y"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn overrides_replace_remove_or_are_ignored() {
        let run = find("run").unwrap();
        assert_eq!(effective_in(&map(&[]), run), vec![n("cmd-shift-enter")]);
        assert_eq!(effective_in(&map(&[("run", "cmd-shift-r")]), run), vec![n("cmd-shift-r")]);
        assert_eq!(effective_in(&map(&[("run", "")]), run), Vec::<String>::new());
        assert_eq!(effective_in(&map(&[("run", "cmd-k cmd-r")]), run), vec![n("cmd-shift-enter")], "a chord is ignored");
        assert_eq!(effective_in(&map(&[("run", "nonsense-key-x")]), run), vec![n("cmd-shift-enter")]);
        assert_eq!(effective_in(&map(&[("zoom_in", "cmd-u")]), find("zoom_in").unwrap()), vec![n("cmd-u")]);
        let kept = clean(&map(&[("nope", "cmd-j"), ("run", "bad-bad-x"), ("history", "cmd-y"), ("favorite_query", ""), ("export", "shift-cmd-j")]));
        assert_eq!(kept, BTreeMap::from([("export".to_string(), n("cmd-shift-j")), ("favorite_query".to_string(), String::new())]));
    }

    #[test]
    fn defaults_have_no_conflicts_and_new_copy_keys() {
        assert!(conflicts_in(&map(&[])).is_empty(), "{:?}", conflicts_in(&map(&[])));
        assert_eq!(effective_in(&map(&[]), find("copy_with_headers").unwrap()), vec![n("cmd-shift-c")]);
        assert_eq!(effective_in(&map(&[]), find("copy_csv").unwrap()), vec![n("alt-cmd-c")]);
    }

    #[test]
    fn run_runs_the_script() {
        let script = find("run_all").unwrap();
        let stmt = find("run").unwrap();
        assert_eq!(effective_in(&map(&[]), script), vec![n("cmd-enter")]);
        assert_eq!((script.action)().name(), "code_editor::RunScript");
        assert_eq!((stmt.action)().name(), "code_editor::RunStatement");
        assert_eq!(effective_in(&map(&[("run", "cmd-shift-r")]), stmt), vec![n("cmd-shift-r")]);
        assert_eq!(effective_in(&map(&[("run", "cmd-shift-r")]), script), vec![n("cmd-enter")]);
    }

    #[test]
    fn conflicts_only_where_both_work() {
        assert!(conflicts_in(&map(&[("run", "cmd-c")])).is_empty());
        assert_eq!(conflicts_in(&map(&[("run", "cmd-t")])), BTreeSet::from(["run", "new_tab"]));
        assert_eq!(conflicts_in(&map(&[("copy_csv", "cmd-c")])), BTreeSet::from(["copy_csv", "copy_tsv"]));
        assert!(conflicts_in(&map(&[("copy_csv", "cmd-c"), ("copy_tsv", "cmd-alt-c")])).is_empty());
    }

    #[test]
    fn typed_shortcuts() {
        for (typed, want) in [
            ("⌘k", "cmd-k"),
            ("⇧⌘P", "cmd-shift-p"),
            ("⌘⇧p", "cmd-shift-p"),
            ("⌃⌘↩", "ctrl-cmd-enter"),
            ("⌥⌘[", "alt-cmd-["),
            ("⌥F12", "alt-f12"),
            ("⌘ ⇧ p", "cmd-shift-p"),
            ("cmd-shift-p", "cmd-shift-p"),
            ("Shift+Cmd+P", "cmd-shift-p"),
            ("cmd+=", "cmd-="),
            ("cmd++", "cmd-+"),
            ("cmd--", "cmd--"),
        ] {
            assert_eq!(parse_typed(typed), Some(n(want)), "{typed}");
        }
        for typed in ["", "run", "copy csv", "⌘", "⇧⌘", "cmd-", "foo-bar"] {
            assert_eq!(parse_typed(typed), None, "{typed}");
        }
        for c in COMMANDS {
            for k in defaults(c).into_iter().filter(|k| k.contains('-')) {
                assert_eq!(parse_typed(&label_native(&k)).as_deref(), Some(k.as_str()), "{} {k}", c.id);
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_keys_and_labels() {
        for (mac, linux, shown) in [
            ("cmd-shift-c", "ctrl-shift-c", "Ctrl+Shift+C"),
            ("cmd-enter", "ctrl-enter", "Ctrl+Enter"),
            ("ctrl-cmd-enter", "alt-shift-enter", "Alt+Shift+Enter"),
            ("ctrl-cmd-backspace", "alt-shift-backspace", "Alt+Shift+Backspace"),
            ("ctrl-cmd-left", "alt-left", "Alt+←"),
            ("cmd-alt-right", "alt-pagedown", "Alt+PgDn"),
            ("cmd-alt-l", "ctrl-shift-l", "Ctrl+Shift+L"),
            ("cmd-left", "home", "Home"),
            ("cmd-shift-right", "shift-end", "Shift+End"),
            ("cmd-up", "ctrl-home", "Ctrl+Home"),
            ("cmd-shift-down", "ctrl-shift-end", "Ctrl+Shift+End"),
            ("alt-left", "ctrl-left", "Ctrl+←"),
            ("alt-shift-right", "ctrl-shift-right", "Ctrl+Shift+→"),
            ("alt-backspace", "ctrl-backspace", "Ctrl+Backspace"),
            ("cmd-backspace", "ctrl-shift-backspace", "Ctrl+Shift+Backspace"),
            ("cmd--", "ctrl--", "Ctrl+-"),
            ("cmd-\\", "ctrl-\\", "Ctrl+\\"),
            ("f6", "f6", "F6"),
            ("ctrl-tab", "ctrl-tab", "Ctrl+Tab"),
        ] {
            assert_eq!(native(mac), linux, "{mac}");
            assert_eq!(native_chosen(linux), linux, "{linux} chosen on Linux stays");
            assert_eq!(label_native(linux), shown);
            assert_eq!(label(mac), shown, "{mac}");
            if shown.contains('+') {
                assert_eq!(parse_typed(shown).as_deref(), Some(linux), "{shown}");
            }
        }
        assert_eq!(parse_typed("⌘K").as_deref(), Some("ctrl-k"), "typed as on a Mac");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_defaults_leave_the_desktop_its_keys() {
        for c in COMMANDS {
            for k in defaults(c) {
                assert!(!k.contains("super"), "{} {k}", c.id);
                for taken in ["ctrl-alt-t", "ctrl-alt-l", "ctrl-alt-delete", "ctrl-alt-backspace", "ctrl-alt-left", "ctrl-alt-right", "ctrl-alt-up", "ctrl-alt-down", "ctrl-alt-d"] {
                    assert_ne!(k, taken, "{}", c.id);
                }
            }
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn labels() {
        assert_eq!(label("cmd-shift-c"), "⇧⌘C");
        assert_eq!(label("ctrl-cmd-enter"), "⌃⌘↩");
        assert_eq!(label("cmd-alt-]"), "⌥⌘]");
        assert_eq!(label("alt-f12"), "⌥F12");
        assert_eq!(label("ctrl-tab"), "⌃Tab");
        assert_eq!(label("cmd-alt-left"), "⌥⌘←");
        assert_eq!(label("cmd-\\"), "⌘\\");
    }
}
