//! Dataman: a native PostgreSQL client, with extra panels for LAR servers.

mod activity;
mod activity_demo;
mod agent;
mod app;
mod assets;
mod bench;
mod chrome;
mod conn_dialog;
mod conns;
mod crash_ui;
mod data_import;
mod dnd;
mod dock;
mod dock_check;
mod dock_icon;
mod edit_ui;
mod editor;
mod explorer_demo;
mod export_ui;
mod find;
mod find_check;
mod fk_check;
mod glass;
mod mssql_check;
mod mysql_check;
mod restore;
mod results_check;
mod session_check;
mod grid;
mod grid_menu;
mod history;
mod history_check;
mod import_demo;
mod import_ui;
mod input;
mod intel_demo;
mod keymap;
mod lar;
mod mcp;
mod mcp_check;
mod messages;
mod motion;
mod nav;
mod nav_check;
mod notify;
mod palette;
mod panels;
mod params_ui;
mod plan;
mod plan_demo;
mod privileges;
mod result_bar;
mod result_stack;
mod row_actions;
mod settings_view;
mod shortcuts;
mod split;
mod split_check;
mod sql_picker;
mod tab;
mod table_check;
mod table_view;
mod theme;
mod tree;
mod tree_menu;
mod tx_check;
mod ui;
mod ui_check;
mod update_check;
mod update_ui;
mod updates;
mod value_edit;
mod welcome;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext, Bounds, Menu, MenuItem, OsAction, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use gpui_platform::application;
use dataman_core::{Core, Dirs, Keychain, MemorySecrets, SecretStore, ThemePref};

/// The app's version: `DATAMAN_VERSION` at build time (scripts/bundle.sh), else the crate's.
pub const VERSION: &str = match option_env!("DATAMAN_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// `DATAMAN_TRACE_STARTUP=1` prints where startup time goes.
fn mark(t: Instant, what: &str) {
    if std::env::var_os("DATAMAN_TRACE_STARTUP").is_some() {
        eprintln!("startup: {what} at {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    }
}

/// The menus again (the app menu's update entries follow the update checker).
pub(crate) fn refresh_menus(cx: &mut App) {
    let m = menus(cx);
    cx.set_menus(m);
}

/// What the active connection's engine can do: the menu bar leaves out the rest (Explain, the
/// activity monitor and imports on MySQL). Set by the workspace when it changes.
pub(crate) struct MenuCaps(pub dataman_core::Caps);

impl gpui::Global for MenuCaps {}

fn menus(cx: &App) -> Vec<Menu> {
    use app::*;
    let caps = cx.try_global::<MenuCaps>().map_or(dataman_core::Caps::POSTGRES, |m| m.0);
    let when = |on: bool, items: Vec<MenuItem>| if on { items } else { Vec::new() };
    let mut app_menu: Vec<MenuItem> = Vec::new();
    if let Some(label) = updates::menu_label(cx) {
        app_menu.push(MenuItem::action(label, ShowUpdate));
    }
    if updates::enabled(cx) {
        app_menu.push(MenuItem::action("Check for Updates…", CheckForUpdates));
        app_menu.push(MenuItem::separator());
    }
    vec![
        Menu::new("Dataman").items(app_menu.into_iter().chain([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
            MenuItem::separator(),
            MenuItem::action("Quit Dataman", Quit),
        ])),
        Menu::new("File").items(
            [
                MenuItem::action("New Query Tab", NewTab),
                MenuItem::action("New Connection…", NewConnection),
                MenuItem::action("New Connection from Clipboard", NewConnectionFromClipboard),
                MenuItem::action("Import Connections…", ImportConnections),
            ]
            .into_iter()
            .chain(when(caps.import, vec![MenuItem::action("Import Data…", ImportData)]))
            .chain([
                MenuItem::action("Open .sql File…", OpenSqlFile),
                MenuItem::action("Save as Favorite", SaveFavorite),
                MenuItem::action("Save As…", SaveAs),
                MenuItem::action("Import Favorites…", ImportFavorites),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
            ]),
        ),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", editor::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", editor::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", editor::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", editor::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", editor::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", editor::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find…", find::Find),
            MenuItem::action("Find Next", find::FindNext),
            MenuItem::action("Find Previous", find::FindPrevious),
            MenuItem::action("Replace…", find::FindReplace),
            MenuItem::separator(),
            MenuItem::action("Toggle Comment", editor::ToggleComment),
            MenuItem::action("Format SQL", editor::FormatSql),
            MenuItem::separator(),
            MenuItem::action("Go to Definition", editor::GoToDefinition),
            MenuItem::action("Open Definition", editor::PeekDefinition),
        ]),
        Menu::new("Query").items(
            [
                MenuItem::action("Export Result…", ExportResult),
                MenuItem::action("Review & Commit Edits", ReviewChanges),
                MenuItem::separator(),
                MenuItem::action("Run", editor::RunScript),
                MenuItem::action("Run Statement", editor::RunStatement),
                MenuItem::action("Run From Statement", editor::RunFromHere),
                MenuItem::action("Cancel on Server", Cancel),
                MenuItem::separator(),
            ]
            .into_iter()
            .chain(when(
                caps.explain,
                vec![
                    MenuItem::action("Explain", Explain),
                    MenuItem::action("Explain Analyze", ExplainAnalyze),
                    MenuItem::action("Copy Plan as Text", CopyPlanText),
                    MenuItem::action("Plan: Tree / Flame", TogglePlanFlame),
                    MenuItem::separator(),
                ],
            ))
            .chain([
            MenuItem::action("Commit Transaction", CommitTx),
            MenuItem::action("Rollback Transaction", RollbackTx),
            MenuItem::separator(),
            MenuItem::action("Next Result", NextResult),
            MenuItem::action("Previous Result", PrevResult),
            MenuItem::separator(),
            MenuItem::action("Next Page", NextPage),
            MenuItem::action("Previous Page", PrevPage),
            MenuItem::separator(),
            MenuItem::action("Jump to Error", JumpToError),
            MenuItem::action("Use Explorer Connection", UseExplorerConnection),
            MenuItem::action("History", ToggleHistory),
            ]),
        ),
        Menu::new("View").items(
            [
                MenuItem::action("Command Palette", CommandPalette),
                MenuItem::action("Go to Table or Tab", GoTo),
                MenuItem::separator(),
                MenuItem::action("Toggle Explorer", ToggleExplorer),
                MenuItem::action("Toggle Side Panel", ToggleSidePanel),
                MenuItem::action("Reset Panel Layout", dock::ResetPanelLayout),
                MenuItem::action("Open to the Side", split::OpenToSide),
            ]
            .into_iter()
            .chain(when(caps.activity, vec![MenuItem::action("Activity Monitor", OpenActivity)]))
            .chain([
            MenuItem::action("MCP Log", ToggleMcpLog),
            MenuItem::separator(),
            MenuItem::action("Focus Explorer", FocusExplorer),
            MenuItem::action("Focus Editor", FocusEditor),
            MenuItem::action("Focus Grid", FocusGrid),
            MenuItem::action("Next Region", CycleFocus),
            MenuItem::separator(),
            MenuItem::action("Toggle Theme (dark → light → system)", ToggleTheme),
            MenuItem::action("Dark Theme", ThemeDark),
            MenuItem::action("Light Theme", ThemeLight),
            MenuItem::action("Theme Follows System", ThemeSystem),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("Transparency").items([
                MenuItem::action("On", TranslucencyOn),
                MenuItem::action("Off", TranslucencyOff),
            ])),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Actual Size", ZoomReset),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("Font").items([
                MenuItem::action("Geist Mono", FontGeistMono),
                MenuItem::action("Lilex", FontLilex),
                MenuItem::action("Zed Mono", FontZedMono),
                MenuItem::action("JetBrains Mono", FontJetBrainsMono),
            ])),
            ]),
        ),
        Menu::new("Window").items([
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PrevTab),
        ]),
    ]
}

fn main() {
    let t_main = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("{VERSION}");
        return;
    }
    // The panic hook and the in-memory log the crash report draws on (no I/O here).
    dataman_core::crash::install(dataman_core::crash::dir(), VERSION);
    log::info!("Dataman {VERSION}, macOS {}", std::env::consts::ARCH);
    let bench = bench::Mode::from_args(&args);

    // Benchmarks use a throwaway config directory and in-memory secrets: they never touch the
    // user's connections or Keychain.
    let (dirs, secrets): (Dirs, Arc<dyn SecretStore>) = match bench {
        Some(_) => (
            Dirs::at(std::env::temp_dir().join(format!("dataman-bench-{}", std::process::id()))),
            Arc::new(MemorySecrets::default()),
        ),
        None => (Dirs::default_dirs().expect("no home directory"), Arc::new(Keychain)),
    };
    let core = match Core::new(dirs.clone(), secrets) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Dataman: {e:#}");
            std::process::exit(1);
        }
    };

    // The workspace kept between launches; none for the benchmarks and `--demo` unless
    // `DATAMAN_SESSION_DIR` asks for one.
    let session_dir = restore::dir_for(&dirs, bench.is_some());
    mark(t_main, "core ready");
    let app = application().with_assets(assets::Assets);
    // .sql files opened from Finder (or `open -a`) arrive as file:// URLs, each in a new tab.
    let (open_tx, mut open_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = open_tx.send(urls);
    });
    mark(t_main, "platform ready");
    app.run(move |cx: &mut App| {
        mark(t_main, "app launched");
        // `DATAMAN_FONT` (any spelling of a family) overrides the setting for one run (shots).
        let font = std::env::var("DATAMAN_FONT").ok().and_then(|f| dataman_core::UiFont::parse(&f)).unwrap_or(core.settings.get().font);
        theme::register_fonts(font, cx);
        mark(t_main, "fonts registered");
        let pref = match std::env::var("DATAMAN_THEME").ok().as_deref() {
            Some("light") => ThemePref::Light,
            Some("dark") => ThemePref::Dark,
            Some("system") => ThemePref::System,
            _ => core.settings.get().theme,
        };
        let settings_core = core.clone();
        cx.set_global(theme::SavePref(Box::new(move |pref| {
            if let Err(e) = settings_core.settings.update(|s| s.theme = pref) {
                log::warn!("could not save the theme: {e:#}");
            }
        })));
        let settings_core = core.clone();
        cx.set_global(theme::SaveFont(Box::new(move |font| {
            if let Err(e) = settings_core.settings.update(|s| s.font = font) {
                log::warn!("could not save the font: {e:#}");
            }
        })));
        if let Some(name) = core.settings.get().system_font.as_deref() {
            theme::activate_system_font(name, cx);
        }
        let settings_core = core.clone();
        cx.set_global(theme::SaveSystemFont(Box::new(move |name| {
            if let Err(e) = settings_core.settings.update(|s| s.system_font = name) {
                log::warn!("could not save the system font: {e:#}");
            }
        })));
        // The UI zoom; `DATAMAN_ZOOM=1.25` overrides it for one run (shots) without saving.
        let zoom = std::env::var("DATAMAN_ZOOM").ok().and_then(|z| z.parse::<f32>().ok()).unwrap_or(core.settings.get().zoom);
        theme::init_zoom(zoom);
        let settings_core = core.clone();
        cx.set_global(theme::SaveZoom(Box::new(move |zoom| {
            if let Err(e) = settings_core.settings.update(|s| s.zoom = zoom) {
                log::warn!("could not save the zoom: {e:#}");
            }
        })));
        let settings_core = core.clone();
        cx.set_global(theme::SaveTranslucency(Box::new(move |level| {
            if let Err(e) = settings_core.settings.update(|s| s.translucency = level) {
                log::warn!("could not save the translucency: {e:#}");
            }
        })));
        // `translucency` in settings.toml (`DATAMAN_OPAQUE=1` forces it off) sets the window's
        // background and the theme's glass, surface and cards: set before the theme.
        let level = theme::effective_level(core.settings.get().translucency);
        cx.set_global(theme::WindowGlass { level });
        theme::init(pref, cx);
        // The other preferences of the Settings sheet.
        let prefs = core.settings.get();
        editor::set_code_size(prefs.editor_font_size);
        editor::set_format_options(prefs.format_options());
        dataman_core::result::set_prefetch(prefs.prefetch_rows);
        result_stack::set_layout(prefs.result_layout);
        // Reduce motion: the setting, or macOS's accessibility switch (re-read on activation).
        motion::set_reduce_motion_pref(prefs.reduce_motion, cx);
        cx.set_global(panels::PanelRegistry::default());
        cx.set_global(restore::Sessions(session_dir.clone().map(|d| Arc::new(dataman_core::session::SessionStore::new(d)))));
        // LAR's panels: live queries.
        lar::register(cx);
        // Every module's fixed keys and the commands, with `[keys]` from settings.toml.
        keymap::set_overrides(keymap::clean(&core.settings.get().keys));
        keymap::bind_all(cx);
        cx.on_action(|_: &app::Quit, cx| cx.quit());
        updates::init(&core, cx);
        cx.set_menus(menus(cx));

        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let (min_w, min_h) = theme::metrics::MIN_WINDOW;
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Dataman".into()),
                        appears_transparent: true,
                        // The circles' top-left at (16, 14), centred on the title bar's middle
                        // (lower in a zoomed, taller bar).
                        traffic_light_position: {
                            let (x, y) = theme::metrics::traffic_light_position_at(theme::zoom());
                            Some(point(px(x), px(y)))
                        },
                    }),
                    // The title bar, explorer and status bar sit on the system blur; the app
                    // draws only a tint over it (theme `glass`) and idles at 0 fps.
                    window_background: theme::background_for(level),
                    window_min_size: Some(size(px(min_w), px(min_h))),
                    app_owns_titlebar_drag: true,
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| app::Workspace::new(core.clone(), window, cx)),
            )
            .expect("cannot open the window");
        mark(t_main, "window opened");
        let mcp_core = core.clone();
        let handle: gpui::AnyWindowHandle = window.into();
        window
            .update(cx, |ws, window, cx| {
                let weak = cx.weak_entity();
                // Time to the first presented frame: from `main`, and from the launcher's clock
                // when it passes one (that includes process start and dynamic loading).
                window.on_next_frame(move |_, cx| {
                    // The MCP socket (when agent access is on), the update check and last
                    // session's crash report start after the first frame, off the UI thread:
                    // they never delay the window.
                    if bench.is_none() {
                        mcp::apply(&mcp_core, cx);
                        mcp::forget_http_token(&mcp_core);
                        updates::start(weak.clone(), handle, cx);
                        crash_ui::check(weak.clone(), cx);
                        crash_ui::maybe_crash_for_test(cx);
                        cx.spawn(async move |cx| {
                            while let Some(urls) = open_rx.recv().await {
                                let _ = handle.update(cx, |_, window, cx| {
                                    let _ = weak.update(cx, |w, cx| w.open_urls(urls, window, cx));
                                });
                            }
                        })
                        .detach();
                    }
                    let main_ms = t_main.elapsed().as_secs_f64() * 1e3;
                    if let Some(t0) = std::env::var("DATAMAN_T0_MS").ok().and_then(|v| v.parse::<u128>().ok()) {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                        println!("startup: launch to first frame {} ms (main to first frame {main_ms:.1} ms)", now - t0);
                    } else if bench == Some(bench::Mode::Startup) {
                        println!("startup: main to first frame {main_ms:.1} ms");
                    }
                    if bench == Some(bench::Mode::Startup) {
                        cx.quit();
                    }
                });
                let _ = ws;
                // Dataman's own blur view, where GPUI's does not work (`glass`).
                crate::glass::apply(level, window);
                cx.activate(true);
            })
            .expect("window update");
        if let Some(mode) = bench {
            let ws = window.entity(cx).expect("workspace");
            cx.update_window(window.into(), |_, window, cx| bench::start(mode, ws, window, cx))
                .expect("window update");
        }
    });
    if bench.is_some() {
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
