mod activity;
#[cfg(feature = "checks")]
mod activity_demo;
mod agent;
mod agents;
#[cfg(feature = "checks")]
mod agents_check;
mod agents_clear;
mod agents_mcp;
mod agents_login;
mod agents_rules;
mod agents_suggest;
mod agents_types;
mod app;
mod assets;
#[cfg(feature = "checks")]
mod bench;
#[cfg(feature = "checks")]
mod check;
mod chrome;
mod conn_dialog;
#[cfg(feature = "checks")]
mod conn_folders_check;
mod conns;
#[cfg(feature = "checks")]
mod conns_check;
mod crash_ui;
mod csd;
mod data_import;
mod dnd;
mod doc_view;
mod dock;
#[cfg(feature = "checks")]
mod dock_check;
mod dock_icon;
#[cfg(feature = "checks")]
mod edit_check;
mod edit_ui;
mod editor;
#[cfg(feature = "checks")]
mod engines_check;
#[cfg(feature = "checks")]
mod explorer_check;
#[cfg(feature = "checks")]
mod explorer_demo;
#[cfg(feature = "checks")]
mod export_check;
mod export_ui;
mod fades;
mod find;
#[cfg(feature = "checks")]
mod find_check;
#[cfg(feature = "checks")]
mod fk_check;
#[cfg(feature = "checks")]
mod format_check;
mod glass;
#[cfg(feature = "checks")]
mod mssql_check;
#[cfg(feature = "checks")]
mod mysql_check;
mod restore;
#[cfg(feature = "checks")]
mod results_check;
#[cfg(feature = "checks")]
mod session_check;
mod grid;
mod grid_menu;
mod history;
#[cfg(feature = "checks")]
mod history_check;
#[cfg(feature = "checks")]
mod import_demo;
mod import_ui;
mod input;
#[cfg(feature = "checks")]
mod input_check;
#[cfg(target_os = "linux")]
mod instance;
#[cfg(feature = "checks")]
mod lite_check;
#[cfg(feature = "checks")]
mod intel_demo;
mod keymap;
mod keys;
mod lar;
mod mcp;
#[cfg(feature = "checks")]
mod mcp_check;
#[cfg(feature = "checks")]
mod mcp_tools_check;
#[cfg(feature = "checks")]
mod menus_check;
mod messages;
mod motion;
mod nav;
#[cfg(feature = "checks")]
mod nav_check;
#[cfg(feature = "checks")]
mod insert_check;
#[cfg(feature = "checks")]
mod newuser_check;
#[cfg(feature = "checks")]
mod nosql_check;
mod notify;
#[cfg(feature = "checks")]
mod outside_check;
mod outside_ui;
mod palette;
mod panels;
mod params_ui;
#[cfg(feature = "checks")]
mod pin_check;
mod plan;
#[cfg(feature = "checks")]
mod plan_demo;
mod privileges;
#[cfg(feature = "checks")]
mod project_check;
mod project_ui;
mod result_bar;
mod row_actions;
#[cfg(feature = "checks")]
mod settings_check;
mod settings_view;
mod shortcuts;
mod sidebar;
mod split;
#[cfg(feature = "checks")]
mod split_check;
mod sql_picker;
mod tab;
mod tab_motion;
#[cfg(feature = "checks")]
mod tabs_check;
#[cfg(feature = "checks")]
mod table_check;
mod table_view;
mod theme;
mod tree;
mod tree_menu;
#[cfg(feature = "checks")]
mod tx_check;
mod ui;
#[cfg(feature = "checks")]
mod ui_check;
mod undo;
#[cfg(feature = "checks")]
mod undo_check;
#[cfg(feature = "checks")]
mod update_check;
mod update_ui;
mod updates;
#[cfg(feature = "checks")]
mod value_check;
#[cfg(feature = "checks")]
mod values_check;
mod value_edit;
mod value_view;
mod welcome;
#[cfg(feature = "checks")]
mod welcome_check;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext, Bounds, Menu, MenuItem, OsAction, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use gpui_platform::application;
use dataman_core::{Core, Dirs, Keychain, MemorySecrets, SecretStore, ThemePref};

/// The desktop entry's name on Linux (Wayland app id, icon, notifications).
pub const APP_ID: &str = "dev.dataman.Dataman";

pub const VERSION: &str = match option_env!("DATAMAN_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

fn mark(t: Instant, what: &str) {
    if std::env::var_os("DATAMAN_TRACE_STARTUP").is_some() {
        eprintln!("startup: {what} at {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    }
}

pub(crate) fn refresh_menus(cx: &mut App) {
    let m = menus(cx);
    cx.set_menus(m);
}

pub(crate) struct MenuCaps(pub dataman_core::Caps);

impl gpui::Global for MenuCaps {}

/// The groups that have items, a separator between each.
fn sections(groups: Vec<Vec<MenuItem>>) -> Vec<MenuItem> {
    let mut out = Vec::new();
    for g in groups.into_iter().filter(|g| !g.is_empty()) {
        if !out.is_empty() {
            out.push(MenuItem::separator());
        }
        out.extend(g);
    }
    out
}

fn menus(cx: &App) -> Vec<Menu> {
    use app::*;
    let caps = cx.try_global::<MenuCaps>().map_or(dataman_core::Caps::POSTGRES, |m| m.0);
    let projects = project_ui::projects(cx);
    let when = |on: bool, items: Vec<MenuItem>| if on { items } else { Vec::new() };
    let mut app_menu: Vec<MenuItem> = Vec::new();
    if let Some(label) = updates::menu_label(cx) {
        app_menu.push(MenuItem::action(label, ShowUpdate));
    }
    if updates::enabled(cx) {
        let checking = updates::state(cx) == updates::State::Checking;
        app_menu.push(MenuItem::action(if checking { "Checking for Updates…" } else { "Check for Updates…" }, CheckForUpdates));
        app_menu.push(MenuItem::separator());
    }
    vec![
        Menu::new("Dataman").items(app_menu.into_iter().chain([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
            MenuItem::separator(),
            MenuItem::action("Quit Dataman", Quit),
        ])),
        // Groups of what belongs together, a separator between those that show.
        Menu::new("File").items(sections(vec![
            vec![MenuItem::action("New Query Tab", NewTab)],
            vec![
                MenuItem::action("New Connection…", NewConnection),
                MenuItem::action("New Connection from Clipboard", NewConnectionFromClipboard),
                MenuItem::action("Import Connections…", ImportConnections),
            ],
            when(projects.enabled, vec![MenuItem::action("Open Project…", OpenProject)])
                .into_iter()
                .chain(when(projects.open.is_some(), vec![MenuItem::action("Close Project", CloseProject)]))
                .collect(),
            [MenuItem::action("Open .sql File…", OpenSqlFile), MenuItem::action("Open Database File…", OpenDatabaseFile)]
                .into_iter()
                .chain(when(caps.import, vec![MenuItem::action("Import Data…", ImportData)]))
                .collect(),
            vec![
                MenuItem::action("Save As…", SaveAs),
                MenuItem::action("Save as Favorite", SaveFavorite),
                MenuItem::action("Format on Save", ToggleFormatOnSave).checked(editor::format_on_save()),
                MenuItem::action("Import Favorites…", ImportFavorites),
            ],
            vec![MenuItem::action("Close Tab", CloseTab)],
        ])),
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
            MenuItem::action("Format Query", editor::FormatQuery),
            MenuItem::separator(),
            MenuItem::action("Fold", editor::Fold),
            MenuItem::action("Unfold", editor::Unfold),
            MenuItem::action("Fold All", editor::FoldAll),
            MenuItem::action("Unfold All", editor::UnfoldAll),
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
            ]),
        ),
        Menu::new("View").items(
            [
                MenuItem::action("Command Palette", CommandPalette),
                MenuItem::action("Go to Table or Tab", GoTo),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleExplorer),
                MenuItem::action("Explorer", sidebar::ShowExplorer),
                MenuItem::action("Favorites", sidebar::ShowFavorites),
                MenuItem::action("History", sidebar::ShowHistory),
                MenuItem::action("Toggle Side Panel", ToggleSidePanel),
                MenuItem::action("Reset Panel Layout", dock::ResetPanelLayout),
                MenuItem::action("Open to the Side", split::OpenToSide),
            ]
            .into_iter()
            .chain(when(caps.activity, vec![MenuItem::action("Activity Monitor", OpenActivity)]))
            .chain([
            MenuItem::action("Agents", ToggleMcpLog),
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
    // The packages ship `dataman-mcp` as a link to this binary, not a second copy of everything.
    if args.first().is_some_and(|a| std::path::Path::new(a).file_name().is_some_and(|n| n == "dataman-mcp")) {
        dataman_mcp::cli::main();
    }
    if args.iter().any(|a| a == "--version") {
        println!("{VERSION}");
        return;
    }
    dataman_core::crash::install(dataman_core::crash::dir(), VERSION);
    log::info!("Dataman {VERSION}, {} {}", dataman_core::os::NAME, std::env::consts::ARCH);
    // The check stages and benchmarks (`--demo`, `--bench-*`) exist only in builds with the
    // `checks` feature; the app ships without them.
    #[cfg(feature = "checks")]
    let bench = bench::Mode::from_args(&args);
    #[cfg(not(feature = "checks"))]
    let bench: Option<()> = None;
    #[cfg(feature = "checks")]
    let startup = bench == Some(bench::Mode::Startup);
    #[cfg(not(feature = "checks"))]
    let startup = false;

    let (dirs, secrets): (Dirs, Arc<dyn SecretStore>) = match bench {
        Some(_) => {
            // A run that exited early leaves its folder; a later one can get the same pid.
            let dir = std::env::temp_dir().join(format!("dataman-bench-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            (Dirs::at(dir), Arc::new(MemorySecrets::default()))
        }
        None => (Dirs::default_dirs().expect("no home directory"), Arc::new(Keychain)),
    };
    #[cfg(target_os = "linux")]
    let (launch_urls, instance_socket) = {
        let urls = instance::urls_from_args(&args);
        let socket = instance::socket_path(&dirs);
        if bench.is_none() && instance::hand_over(&socket, &urls) {
            return;
        }
        (urls, socket)
    };
    // outside-check keeps the approved rules in its memory store, as the Keychain would.
    #[cfg(feature = "checks")]
    let approved = match bench {
        Some(_) if std::env::var("DATAMAN_SHOT_STAGE").as_deref() == Ok("outside-check") => dataman_core::approved::Approved::always_in(secrets.clone()),
        _ => dataman_core::approved::Approved::kept_in(secrets.clone()),
    };
    #[cfg(not(feature = "checks"))]
    let approved = dataman_core::approved::Approved::kept_in(secrets.clone());
    let core = match Core::with_approved(dirs.clone(), secrets, approved) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Dataman: {e:#}");
            std::process::exit(1);
        }
    };

    let session_dir = restore::dir_for(&dirs, bench.is_some());
    mark(t_main, "core ready");
    let app = application().with_assets(assets::Assets);
    let (open_tx, mut open_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    #[cfg(target_os = "linux")]
    if bench.is_none() {
        if !launch_urls.is_empty() {
            let _ = open_tx.send(launch_urls);
        }
        let tx = open_tx.clone();
        instance::listen(instance_socket, move |urls| {
            let _ = tx.send(urls);
        });
    }
    // `dataman shop.sqlite` (or a .sql file) from a terminal: as if opened from Finder.
    #[cfg(target_os = "macos")]
    if bench.is_none() {
        let urls: Vec<String> = args
            .iter()
            .skip(1)
            .filter(|a| !a.starts_with('-'))
            .filter_map(|a| std::fs::canonicalize(a).ok())
            .filter(|p| p.is_file())
            .map(|p| {
                let mut url = String::from("file://");
                for b in p.to_string_lossy().bytes() {
                    if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) { url.push(b as char) } else { url.push_str(&format!("%{b:02X}")) }
                }
                url
            })
            .collect();
        if !urls.is_empty() {
            let _ = open_tx.send(urls);
        }
    }
    app.on_open_urls(move |urls| {
        let _ = open_tx.send(urls);
    });
    mark(t_main, "platform ready");
    app.run(move |cx: &mut App| {
        mark(t_main, "app launched");
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
        // Set before the theme: it decides the window background and the theme's glass.
        let level = theme::effective_level(core.settings.get().translucency);
        cx.set_global(theme::WindowGlass { level });
        theme::init(pref, cx);
        let prefs = core.settings.get();
        editor::set_code_size(prefs.editor_font_size);
        editor::set_format_options(prefs.format_options());
        editor::set_format_on_save(prefs.format_on_save);
        dataman_core::result::set_prefetch(prefs.prefetch_rows);
        motion::set_reduce_motion_pref(prefs.reduce_motion, cx);
        assets::set_logos(prefs.db_logos);
        cx.set_global(panels::PanelRegistry::default());
        cx.set_global(restore::Sessions(session_dir.clone().map(|d| Arc::new(dataman_core::session::SessionStore::new(d)))));
        lar::register(cx);
        keymap::set_overrides(keymap::clean(&core.settings.get().keys));
        keymap::bind_all(cx);
        cx.on_action(|_: &app::Quit, cx| cx.quit());
        cx.on_action(|_: &app::CheckForUpdates, cx| updates::check(true, cx));
        updates::init(&core, cx);
        cx.set_menus(menus(cx));

        // Check stages can draw a narrower window (DATAMAN_SHOT_WIDTH, down to the smallest).
        let width = if cfg!(feature = "checks") { std::env::var("DATAMAN_SHOT_WIDTH").ok().and_then(|w| w.parse::<f32>().ok()).unwrap_or(1400.) } else { 1400. };
        let bounds = Bounds::centered(None, size(px(width.max(theme::metrics::MIN_WINDOW.0)), px(900.)), cx);
        let (min_w, min_h) = theme::metrics::MIN_WINDOW;
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Dataman".into()),
                        appears_transparent: true,
                        traffic_light_position: {
                            let (x, y) = theme::metrics::traffic_light_position_at(theme::zoom());
                            Some(point(px(x), px(y)))
                        },
                    }),
                    window_background: theme::background_for(level),
                    window_min_size: Some(size(px(min_w), px(min_h))),
                    app_owns_titlebar_drag: true,
                    app_id: Some(APP_ID.into()),
                    window_decorations: crate::keymap::LINUX.then(|| match std::env::var("DATAMAN_DECORATIONS").as_deref() {
                        Ok("server") => gpui::WindowDecorations::Server,
                        _ => gpui::WindowDecorations::Client,
                    }),
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
                window.on_next_frame(move |_, cx| {
                    // After the first frame, off the UI thread: they never delay the window.
                    if bench.is_none() {
                        mcp::apply(&mcp_core, cx);
                        updates::start(weak.clone(), handle, cx);
                        crash_ui::check(weak.clone(), cx);
                        crash_ui::maybe_crash_for_test(cx);
                        cx.spawn(async move |cx| {
                            while let Some(urls) = open_rx.recv().await {
                                let _ = handle.update(cx, |_, window, cx| {
                                    if crate::keymap::LINUX {
                                        window.activate_window();
                                    }
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
                    } else if startup {
                        println!("startup: main to first frame {main_ms:.1} ms");
                    }
                    if startup {
                        cx.quit();
                    }
                });
                let _ = ws;
                crate::glass::apply(level, window);
                cx.activate(true);
            })
            .expect("window update");
        #[cfg(feature = "checks")]
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
