//! Open Project's sync under hostile, huge, racing and odd files (public API only).
//!
//! Each test states the safe behaviour. One that fails today is `#[ignore = "finding: …"]`;
//! the rest are regression tests. `PROJECT_ROBUST_MEASURE=1` prints the timings of the huge
//! files (`cargo test --release -p dataman-core --test project_robust measure -- --nocapture`).
#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dataman_core::config::{ConfigStore, ConnectionProfile, Dirs, McpAccess};
use dataman_core::favorites::{Favorite, FavoritesStore};
use dataman_core::format::KeywordCase;
use dataman_core::project::{self, ProjectFile, ProjectStore, Stores, Synced};
use dataman_core::secret::MemorySecrets;
use dataman_core::settings::{SettingsStore, ThemePref, UiFont};

// ---------------------------------------------------------------------------------------------
// One person's Dataman: its local files and its project store.

struct Machine {
    _dir: Option<tempfile::TempDir>,
    dirs: Dirs,
    config: ConfigStore,
    settings: SettingsStore,
    favorites: FavoritesStore,
    secrets: MemorySecrets,
    project: ProjectStore,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dataman");
        let mut m = Machine::at(&path);
        m._dir = Some(dir);
        m
    }

    /// Another Dataman on the same local files (a second instance, or a restart).
    fn at(path: &Path) -> Machine {
        let dirs = Dirs::at(path);
        dirs.ensure_private().unwrap();
        Machine {
            config: ConfigStore::open(&dirs).unwrap(),
            settings: SettingsStore::open(&dirs),
            favorites: FavoritesStore::open(&dirs).unwrap(),
            secrets: MemorySecrets::default(),
            project: ProjectStore::open(&dirs),
            dirs,
            _dir: None,
        }
    }

    fn st(&self) -> Stores<'_> {
        Stores { config: &self.config, settings: &self.settings, favorites: &self.favorites, secrets: &self.secrets }
    }

    fn open(&self, root: &Path) -> (bool, Synced) {
        self.project.open_project(root, self.st()).unwrap()
    }

    fn sync(&self) -> anyhow::Result<Synced> {
        self.project.sync(self.st())
    }

    fn problem(&self) -> Option<String> {
        self.project.info(&self.settings.get()).and_then(|i| i.problem)
    }
}

fn conn(name: &str) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(name);
    p.host = format!("{name}.db.internal");
    p
}

/// A project folder (canonical: /var is /private/var on macOS).
fn project_dir() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap().join("app");
    std::fs::create_dir(&root).unwrap();
    (d, root)
}

fn text(root: &Path) -> String {
    std::fs::read_to_string(project::file_in(root)).unwrap()
}

fn parsed(root: &Path) -> ProjectFile {
    ProjectFile::parse(&text(root)).unwrap()
}

/// As git or an editor saving through a temporary file: a new inode.
fn replace(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let tmp = path.with_extension("swap");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A project with one shared connection, synced: (machine, the connection).
fn opened(root: &Path) -> (Machine, ConnectionProfile) {
    let m = Machine::new();
    m.open(root);
    let s = conn("shop");
    m.config.upsert(s.clone()).unwrap();
    assert!(m.sync().unwrap().written);
    (m, s)
}

/// Each machine syncs in turn, `rounds` times: what each sync did.
fn alternate(ms: &[&Machine], rounds: usize) -> Vec<Vec<Synced>> {
    (0..rounds).map(|_| ms.iter().map(|m| m.sync().unwrap()).collect()).collect()
}

fn quiet(round: &[Synced]) -> bool {
    round.iter().all(|s| *s == Synced::default())
}

// ---------------------------------------------------------------------------------------------
// 1. Huge, deep and odd files.

fn huge_file(n: usize, pad: usize) -> String {
    let name = "n".repeat(pad);
    let mut s = String::with_capacity(n * (pad + 80));
    s.push_str("version = 1\n\n");
    for i in 0..n {
        s.push_str(&format!("[[connection]]\nid = \"c{i:06}\"\nname = \"{name}\"\nhost = \"h{i}.db.internal\"\n\n"));
    }
    s
}

fn rss_mb() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    // Bytes on macOS, kilobytes on Linux.
    if cfg!(target_os = "macos") { u.ru_maxrss as f64 / 1e6 } else { u.ru_maxrss as f64 / 1e3 }
}

/// Timings (run alone, with --test-threads=1, for the peak memory to mean anything).
#[test]
fn measure_huge_files() {
    if std::env::var_os("PROJECT_ROBUST_MEASURE").is_none() {
        return;
    }
    let big = huge_file(100_000, 430);
    let t = Instant::now();
    let r = ProjectFile::parse(&big);
    println!("{:.1} MB, 100k connections: {:?} in {:?}", big.len() as f64 / 1e6, r.err().map(|e| e.to_string()), t.elapsed());
    // Just under MAX_BYTES (2 MB): the largest file a sync takes.
    let near = huge_file(4_000, 430);
    let t = Instant::now();
    let f = ProjectFile::parse(&near).unwrap();
    println!("parse of {:.2} MB, {} connections: {:?}, peak rss {:.0} MB", near.len() as f64 / 1e6, f.connections.len(), t.elapsed(), rss_mb());
    for n in [1_000usize, 4_000] {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.open(&root);
        replace(&project::file_in(&root), &huge_file(n, 430));
        let t = Instant::now();
        let r = m.sync().unwrap();
        println!("sync of {n} connections: {:?} ({r:?}); peak rss {:.0} MB", t.elapsed(), rss_mb());
        let t = Instant::now();
        m.sync().unwrap();
        println!("  the next sync (nothing changed): {:?}", t.elapsed());
    }
}

/// A file far larger than any team's (here 5 MB) is refused with a problem, not loaded into
/// connections.toml.
#[test]
fn a_huge_file_is_refused_with_a_problem() {
    let (_d, root) = project_dir();
    let m = Machine::new();
    m.open(&root);
    let big = huge_file(10_000, 430);
    assert!(big.len() > 4_000_000);
    replace(&project::file_in(&root), &big);
    let t = Instant::now();
    let r = m.sync();
    println!("sync of {:.1} MB: {:?}", big.len() as f64 / 1e6, t.elapsed());
    assert!(r.is_err() || m.problem().is_some(), "a huge file is a problem");
    assert!(m.config.connections().is_empty(), "nothing of it reaches connections.toml");
}

const CHILD: &str = "PROJECT_ROBUST_CHILD";

/// The deep file cases, run in a child process (a stack overflow aborts the whole process):
/// `kind:depth:stack`.
fn deep_text(kind: &str, depth: usize) -> String {
    match kind {
        "array" => format!("version = 1\na = {}{}\n", "[".repeat(depth), "]".repeat(depth)),
        "inline" => format!("version = 1\na = {}1{}\n", "{b = ".repeat(depth), "}".repeat(depth)),
        "in_connection" => format!("version = 1\n[[connection]]\nid = \"x\"\nname = \"x\"\nextra = {}{}\n", "[".repeat(depth), "]".repeat(depth)),
        "dotted" => format!("version = 1\n{} = 1\n", vec!["a"; depth].join(".")),
        _ => unreachable!(),
    }
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var(CHILD) else { return };
    let mut it = case.split(':');
    let (kind, depth, stack) = (it.next().unwrap().to_string(), it.next().unwrap().parse().unwrap(), it.next().unwrap().parse().unwrap());
    let text = deep_text(&kind, depth);
    let h = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let r = ProjectFile::parse(&text);
            println!("CHILD-RESULT {kind}: {}", match &r {
                Ok(_) => "parsed".to_string(),
                Err(e) => format!("error: {}", e.to_string().lines().last().unwrap_or("")),
            });
        })
        .unwrap();
    h.join().unwrap();
}

fn run_child(kind: &str, depth: usize, stack: usize) -> (bool, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{kind}:{depth}:{stack}"))
        .output()
        .unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let line = all.lines().find(|l| l.contains("CHILD-RESULT") || l.contains("overflowed")).unwrap_or("(no output)").to_string();
    // A child that ran nothing (a renamed test) must not pass.
    (out.status.success() && line.contains("CHILD-RESULT"), line)
}

/// The sync runs on GPUI's background executor: on macOS a GCD worker, 512 KiB of stack. A
/// file nested 100k deep must be an error there, never a stack overflow (which kills the app).
#[test]
fn deeply_nested_files_never_overflow_the_sync_threads_stack() {
    let mut bad = Vec::new();
    for kind in ["array", "inline", "in_connection", "dotted"] {
        let (ok, line) = run_child(kind, 100_000, 512 * 1024);
        println!("{kind} 100k deep, 512 KiB stack: {}", if ok { line.clone() } else { format!("CRASHED: {line}") });
        if !ok {
            bad.push(kind);
        }
    }
    assert!(bad.is_empty(), "stack overflow parsing {bad:?}");
}

const ODD: [char; 8] = ['\u{0}', '\u{1b}', '\u{202e}', '\u{2066}', '\u{200b}', '\u{200d}', '\u{feff}', '\u{7f}'];

fn odd(s: &str) -> bool {
    s.chars().any(|c| ODD.contains(&c) || c.is_control())
}

/// NUL, escape, bidi overrides and zero-width characters in an id, a name or a host make a
/// connection that looks like another one (`shop` / `shop\u{200b}`), or a host shown as one
/// thing and dialled as another: refused (or stripped), never taken as they are.
#[test]
fn control_and_bidi_characters_are_not_taken_from_the_file() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let text = format!(
        "{}\n[[connection]]\nid = \"odd1\"\nname = \"Shop\\u200b\\u0000admin\"\nhost = \"db.internal\\u202elive.evil\"\nsslmode = \"require\"\nuser = \"a\\u001b[2Jb\"\n",
        text(&root)
    );
    replace(&project::file_in(&root), &text);
    let _ = m.sync();
    for p in m.config.connections() {
        for (what, v) in [("id", &p.id), ("name", &p.name), ("host", &p.host), ("user", &p.user)] {
            assert!(!odd(v), "{what} {v:?} taken from the file");
        }
    }
}

/// Whatever the file brings, the local files stay readable (a NUL refuses the file).
#[test]
fn a_nul_in_a_name_does_not_break_the_local_files() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let long = "x".repeat(1 << 20);
    replace(&project::file_in(&root), &format!("{}\n[[connection]]\nid = \"nul\"\nname = \"a\\u0000b\"\nhost = \"{long}\"\nsslmode = \"require\"\n", text(&root)));
    assert!(m.sync().is_err(), "a NUL in a name: the file is refused");
    let again = ConfigStore::open(&m.dirs).unwrap();
    assert!(again.get("nul").is_none());
    assert!(m.problem().is_some_and(|p| p.contains("invisible")));
}

/// Two `[[connection]]` (or `[[favorite]]`) with one id (a merge that kept both sides): the
/// file is refused whole with a problem, nothing applied and the file left as it is.
#[test]
fn duplicate_ids_in_the_file_are_refused() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    for dup in [
        format!("{}\n[[connection]]\nid = \"dup\"\nname = \"dup\"\nhost = \"first\"\nsslmode = \"require\"\n\n[[connection]]\nid = \"dup\"\nname = \"dup\"\nhost = \"second\"\nsslmode = \"require\"\n", text(&root)),
        format!(
            "{}\n[[favorite]]\nid = \"f\"\nname = \"f\"\nsql = \"SELECT 1\"\nconnection_id = \"{}\"\n\n[[favorite]]\nid = \"f\"\nname = \"f\"\nsql = \"SELECT 2\"\nconnection_id = \"{}\"\n",
            text(&root),
            s.id,
            s.id
        ),
    ] {
        replace(&project::file_in(&root), &dup);
        assert!(m.sync().is_err());
        assert!(m.problem().unwrap().contains("twice"), "{:?}", m.problem());
        assert!(m.config.get("dup").is_none() && m.favorites.list().is_empty());
        assert_eq!(text(&root), dup, "left as it is");
    }
}

/// A connection this person had before the project (not one of its connections) is not taken
/// over by a file naming its id: its host would change and the saved password (the Keychain's,
/// by id) would go to the file's host.
#[test]
fn a_personal_connection_is_never_taken_over_by_the_file() {
    let m = Machine::new();
    let mut prod = conn("prod");
    prod.save_password = true;
    m.config.upsert(prod.clone()).unwrap();
    let (_d, root) = project_dir();
    m.open(&root);
    replace(&project::file_in(&root), &format!("version = 1\n[[connection]]\nid = \"{}\"\nname = \"prod\"\nhost = \"evil.example\"\n", prod.id));
    let _ = m.sync();
    assert_eq!(m.config.get(&prod.id).unwrap().host, prod.host, "the personal connection's address");
}

/// A favorite in the file on a connection that is not the project's (a personal one here) is
/// not the project's: it is not added (it would sit on this person's own connection).
#[test]
fn a_favorite_on_a_personal_connection_is_not_taken_from_the_file() {
    let m = Machine::new();
    let own = conn("own");
    m.config.upsert(own.clone()).unwrap();
    let (_d, root) = project_dir();
    m.open(&root);
    replace(&project::file_in(&root), &format!("version = 1\n[[favorite]]\nid = \"daily\"\nname = \"Daily report\"\nsql = \"DELETE FROM users\"\nconnection_id = \"{}\"\n", own.id));
    let _ = m.sync();
    assert!(m.favorites.list().iter().all(|f| f.id != "daily"), "{:?}", m.favorites.list());
}

/// A favorite in the file with the id of a local favorite saved on a personal connection does
/// not replace it (moved to the project's connection, its SQL rewritten).
#[test]
fn a_personal_favorite_is_never_replaced_by_the_file() {
    let m = Machine::new();
    let own = conn("own");
    m.config.upsert(own.clone()).unwrap();
    let mut mine = Favorite::new("mine", "SELECT 1");
    mine.connection_id = Some(own.id.clone());
    m.favorites.add(mine.clone()).unwrap();
    let (_d, root) = project_dir();
    m.open(&root);
    let s = conn("shop");
    m.config.upsert(s.clone()).unwrap();
    m.sync().unwrap();
    replace(&project::file_in(&root), &format!("{}\n[[favorite]]\nid = \"{}\"\nname = \"mine\"\nsql = \"DROP TABLE users\"\nconnection_id = \"{}\"\n", text(&root), mine.id, s.id));
    let _ = m.sync();
    let got = m.favorites.list().into_iter().find(|f| f.id == mine.id).unwrap();
    assert_eq!((got.connection_id.as_deref(), got.sql.as_str()), (Some(own.id.as_str()), "SELECT 1"));
}

// ---------------------------------------------------------------------------------------------
// 2. Two Datamans on one folder.

/// Alice and Bob (each their own local files) syncing one folder in turn settle: after a change
/// on either side, a round or two, then nobody writes.
#[test]
fn two_machines_syncing_in_turn_settle() {
    let (_d, root) = project_dir();
    let alice = Machine::new();
    alice.open(&root);
    let mut s1 = conn("s1");
    s1.save_password = true;
    s1.options.insert("sslpassword".into(), "alice-secret".into());
    s1.options.insert("application_name".into(), "dm".into());
    s1.mcp_row_limit = Some(5);
    alice.config.upsert(s1.clone()).unwrap();
    let home_rel = dataman_core::config::tilde(&root.join("data/x.sqlite").to_string_lossy());
    alice.config.upsert(ConnectionProfile::new_file(dataman_core::config::Engine::Sqlite, home_rel)).unwrap();
    let folder = alice.favorites.add_folder("Reports").unwrap();
    let mut f = Favorite::new("top", "SELECT 1");
    (f.connection_id, f.folder) = (Some(s1.id.clone()), Some(folder.id.clone()));
    alice.favorites.add(f).unwrap();
    alice.favorites.add(Favorite::new("loose", "SELECT 2")).unwrap();
    alice.settings.update(|s| s.keyword_case = KeywordCase::Lower).unwrap();
    alice.sync().unwrap();

    let bob = Machine::new();
    bob.config.upsert(conn("bob-own")).unwrap();
    bob.settings.update(|s| s.indent = 4).unwrap();
    bob.open(&root);
    let log = alternate(&[&alice, &bob], 3);
    assert!(quiet(&log[1]) && quiet(&log[2]), "{log:?}");

    // Agents no longer shared, and Bob keeps his own limit.
    alice.project.edit(alice.st(), |share, _| share.agents = false).unwrap();
    let mut b1 = bob.config.get(&s1.id).unwrap();
    b1.mcp_row_limit = Some(99);
    bob.config.upsert(b1).unwrap();
    let log = alternate(&[&bob, &alice], 4);
    assert!(quiet(&log[2]) && quiet(&log[3]), "{log:?}");
    assert_eq!(bob.config.get(&s1.id).unwrap().mcp_row_limit, Some(99));
    assert!(!text(&root).contains("alice-secret"));
}

/// The order of the connections is each person's (the sidebar's): two people with different
/// orders must not rewrite the file in turn forever (each sync a git diff of the order only).
#[test]
fn different_sidebar_orders_do_not_rewrite_the_file_forever() {
    let (_d, root) = project_dir();
    let alice = Machine::new();
    alice.open(&root);
    let (s1, s2) = (conn("s1"), conn("s2"));
    alice.config.upsert(s1.clone()).unwrap();
    alice.config.upsert(s2.clone()).unwrap();
    alice.sync().unwrap();
    let bob = Machine::new();
    bob.open(&root);
    bob.config.reorder(&[s2.id.clone(), s1.id.clone()]).unwrap();
    let log = alternate(&[&bob, &alice], 6);
    let writes: usize = log.iter().flatten().filter(|s| s.written).count();
    assert!(quiet(&log[4]) && quiet(&log[5]), "{writes} writes in 12 syncs: {log:?}");
}

/// Two instances on the same local files (or the app and another writer of settings.toml):
/// the sync's settings.update writes this instance's whole Settings, so whatever the other one
/// changed since this one started is lost.
#[test]
#[ignore = "finding: SettingsStore::update writes a stale whole Settings; a sync bringing the format reverts the other instance's settings"]
fn two_instances_on_the_same_local_files_keep_each_others_settings() {
    let (_d, root) = project_dir();
    let a = Machine::new();
    a.open(&root);
    let b = Machine::at(&a.dirs.config);
    b.sync().unwrap();
    a.settings.update(|s| s.export_dir = Some("/a/exports".into())).unwrap();
    a.settings.update(|s| s.indent = 4).unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    let disk = SettingsStore::open(&a.dirs);
    assert_eq!(disk.get().export_dir.as_deref(), Some("/a/exports"), "instance B wrote its stale settings over A's");
}

/// Two Datamans in one process (same pid: one `.project.toml.<pid>`) syncing the same folder
/// as fast as they can: a reader never sees a torn file and no sync fails.
#[test]
fn concurrent_syncs_never_tear_the_file() {
    let (_d, root) = project_dir();
    let alice = Machine::new();
    alice.open(&root);
    let bob = Machine::new();
    bob.open(&root);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (mut errors, mut torn, mut reads) = (Vec::new(), 0usize, 0usize);
    std::thread::scope(|sc| {
        let workers: Vec<_> = [&alice, &bob]
            .into_iter()
            .enumerate()
            .map(|(n, m)| {
                sc.spawn(move || {
                    let mut errs = Vec::new();
                    for i in 0..150u16 {
                        let mut p = conn(&format!("m{n}-{}", i % 5));
                        p.id = format!("m{n}-{}", i % 5);
                        p.port = 1000 + i;
                        m.config.upsert(p).unwrap();
                        if let Err(e) = m.sync() {
                            errs.push(format!("{e:#}"));
                        }
                    }
                    errs
                })
            })
            .collect();
        let reader = sc.spawn(|| {
            let (mut torn, mut reads) = (0, 0);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(t) = std::fs::read_to_string(project::file_in(&root)) {
                    reads += 1;
                    if ProjectFile::parse(&t).is_err() {
                        torn += 1;
                    }
                }
            }
            (torn, reads)
        });
        for w in workers {
            errors.extend(w.join().unwrap());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        (torn, reads) = reader.join().unwrap();
    });
    errors.dedup();
    println!("{reads} reads, {torn} torn; sync errors: {errors:?}");
    assert!(torn == 0 && errors.is_empty(), "{torn} torn reads of {reads}; errors {errors:?}");
}

/// After a restart the last agreed copy (project-base.toml) tells a teammate's deletion from a
/// favorite this side added: the deleted favorite goes.
#[test]
fn deletions_travel_across_a_restart() {
    let (_d, root) = project_dir();
    let (alice, s) = opened(&root);
    let mut f = Favorite::new("orders", "SELECT * FROM orders");
    f.connection_id = Some(s.id.clone());
    alice.favorites.add(f.clone()).unwrap();
    alice.sync().unwrap();
    let bob = Machine::new();
    bob.open(&root);
    let bob_dir = bob.dirs.config.clone();
    let bob2 = Machine::at(&bob_dir);
    alice.favorites.remove(&f.id).unwrap();
    alice.sync().unwrap();
    bob2.sync().unwrap();
    assert!(bob2.favorites.list().is_empty());
    assert!(parsed(&root).favorites.is_empty());
    drop(bob);
}

/// project-base.toml keeps one project: after another project was open, a teammate's deletion
/// in the first comes back (the old copy is gone, so the local favorite is written back).
#[test]
fn deletions_survive_opening_another_project_in_between() {
    let (_d, root) = project_dir();
    let (alice, s) = opened(&root);
    let mut f = Favorite::new("orders", "SELECT * FROM orders");
    f.connection_id = Some(s.id.clone());
    alice.favorites.add(f.clone()).unwrap();
    alice.sync().unwrap();
    let bob = Machine::new();
    bob.open(&root);
    let (_d2, other) = project_dir();
    bob.open(&other);
    alice.favorites.remove(&f.id).unwrap();
    alice.sync().unwrap();
    bob.open(&root);
    bob.sync().unwrap();
    assert!(parsed(&root).favorites.is_empty(), "the deleted favorite is back in the file");
}

/// project-base.toml is written in place (std::fs::write): a crash or a reader mid-write sees
/// half of it, which reads as no base at all, with the same resurrection.
#[test]
#[ignore = "residual: a base torn by hand (Dataman now writes it whole, through a rename) cannot tell a deletion"]
fn a_torn_base_does_not_resurrect_deleted_favorites() {
    let (_d, root) = project_dir();
    let (alice, s) = opened(&root);
    let mut f = Favorite::new("orders", "SELECT * FROM orders");
    f.connection_id = Some(s.id.clone());
    alice.favorites.add(f.clone()).unwrap();
    alice.sync().unwrap();
    let bob = Machine::new();
    bob.open(&root);
    let base = bob.dirs.project_base_file(&root);
    let full = std::fs::read_to_string(&base).unwrap();
    std::fs::write(&base, &full[..full.len() / 2]).unwrap();
    alice.favorites.remove(&f.id).unwrap();
    alice.sync().unwrap();
    let bob2 = Machine::at(&bob.dirs.config);
    bob2.sync().unwrap();
    assert!(parsed(&root).favorites.is_empty(), "the deleted favorite is back in the file");
}

// ---------------------------------------------------------------------------------------------
// 3. Git while the app runs.

/// A checkout to an older version (a new inode, an mtime that goes backwards) is followed:
/// what the old version lacks is no longer shared, and the file is not rewritten.
#[test]
fn a_checkout_to_an_older_version_is_followed() {
    let (_d, root) = project_dir();
    let (m, s1) = opened(&root);
    let v1 = text(&root);
    let old_mtime = std::fs::metadata(project::file_in(&root)).unwrap().modified().unwrap() - Duration::from_secs(86_400);
    let s2 = conn("s2");
    m.config.upsert(s2.clone()).unwrap();
    m.sync().unwrap();
    replace(&project::file_in(&root), &v1);
    std::fs::File::options().write(true).open(project::file_in(&root)).unwrap().set_modified(old_mtime).unwrap();
    let r = m.sync().unwrap();
    assert!(!r.written, "the checked-out version is left as it is");
    assert_eq!(text(&root), v1);
    let info = m.project.info(&m.settings.get()).unwrap();
    assert_eq!(info.connections, BTreeSet::from([s1.id.clone()]));
    assert!(m.config.get(&s2.id).is_some(), "the local copy stays");
}

/// A rewrite in place (same inode), same size, mtime put back (`cp -p`, `touch -r`, rsync -t):
/// FileStamp compares (mtime, len, inode), so the change is never seen. ctime would catch it.
#[test]
fn an_in_place_same_size_rewrite_is_seen() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    let path = project::file_in(&root);
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let t = text(&root).replace("name = \"shop\"", "name = \"spoh\"");
    assert_ne!(t, text(&root));
    let mut f = std::fs::File::options().write(true).truncate(true).open(&path).unwrap();
    f.write_all(t.as_bytes()).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    m.sync().unwrap();
    assert_eq!(m.config.get(&s.id).unwrap().name, "spoh");
}

/// A checkout of a branch without `.dataman/project.toml`: the project's file is gone, which is
/// a problem to show, not a file to write (an untracked file there makes `git checkout` back to
/// the branch that has it refuse: "untracked working tree files would be overwritten").
#[test]
fn a_deleted_project_file_is_not_recreated() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    std::fs::remove_file(project::file_in(&root)).unwrap();
    let _ = m.sync();
    let what = std::fs::read_to_string(project::file_in(&root)).unwrap_or_default();
    assert!(!project::file_in(&root).exists(), "recreated as:\n{what}");
    assert!(m.problem().is_some());
}

#[test]
fn a_deleted_dataman_dir_is_not_recreated() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    std::fs::remove_dir_all(root.join(project::DIR)).unwrap();
    let _ = m.sync();
    assert!(!root.join(project::DIR).exists());
    assert!(m.problem().is_some());
}

/// The folder moved or deleted: a problem, nothing written, nothing created; back, it syncs.
#[test]
fn a_missing_folder_is_a_problem_and_nothing_is_written() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let moved = root.with_file_name("moved");
    std::fs::rename(&root, &moved).unwrap();
    m.config.upsert(conn("while-away")).unwrap();
    assert_eq!(m.sync().unwrap(), Synced::default());
    assert!(!root.exists(), "the folder is not recreated");
    assert!(m.problem().unwrap().contains("is not there"));
    std::fs::rename(&moved, &root).unwrap();
    assert!(m.sync().unwrap().written);
    assert!(m.problem().is_none());
}

/// A teammate's change landing between the sync's read of the file and its write (a `git pull`
/// during a sync) is not overwritten by this side's file computed from the older one. The read
/// of connections.toml (inside the sync) is made to wait on a FIFO while the file changes.
#[test]
fn a_change_during_a_sync_is_not_overwritten() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    // A rename: applied without review, so the sync goes on to write connections.toml.
    let v2 = text(&root).replace("name = \"shop\"", "name = \"Shop v2\"");
    assert_ne!(v2, text(&root));
    let v3 = format!("{v2}\n[[connection]]\nid = \"teammate\"\nname = \"teammate\"\nhost = \"t.internal\"\nsslmode = \"require\"\n");
    replace(&project::file_in(&root), &v2);
    m.config.upsert(conn("local-new")).unwrap();
    let cfg = m.dirs.connections_file();
    let cfg_text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::remove_file(&cfg).unwrap();
    let c = std::ffi::CString::new(cfg.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let path = project::file_in(&root);
    let helper = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Opens only once the sync is reading connections.toml, i.e. past its read of the file.
            match std::fs::File::options().write(true).custom_flags(libc::O_NONBLOCK).open(&cfg) {
                Ok(mut w) => {
                    replace(&path, &v3);
                    w.write_all(cfg_text.as_bytes()).unwrap();
                    return true;
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => {
                    let _ = std::fs::remove_file(&cfg);
                    std::fs::write(&cfg, cfg_text).unwrap();
                    return false;
                }
            }
        }
    });
    let r = m.sync();
    assert!(helper.join().unwrap(), "the sync did not read connections.toml in between");
    r.unwrap();
    m.sync().unwrap();
    assert!(text(&root).contains("t.internal"), "the teammate's connection was written over:\n{}", text(&root));
    assert!(m.config.get("teammate").is_some());
}

// ---------------------------------------------------------------------------------------------
// 4. Symlinks.

/// `.dataman` a symlink (a cloned repo can hold one) pointing out of the folder: refused with a
/// problem; nothing read from or written to where it points.
#[test]
fn a_symlinked_dataman_dir_is_refused() {
    let (_d, root) = project_dir();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join(project::DIR)).unwrap();
    let m = Machine::new();
    let _ = m.project.open_project(&root, m.st());
    m.config.upsert(conn("shop")).unwrap();
    let _ = m.sync();
    let written: Vec<_> = std::fs::read_dir(outside.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(written.is_empty(), "written outside the folder: {written:?}");
    assert!(m.problem().is_some());
}

/// `project.toml` a symlink to a valid project file elsewhere: refused with a problem; the
/// link stays a link (today the sync reads the target, then renames over the link).
#[test]
fn a_symlinked_project_file_is_refused_and_kept() {
    let (_d, root) = project_dir();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("other.toml");
    std::fs::write(&target, "version = 1\n[[connection]]\nid = \"far\"\nname = \"far\"\nhost = \"far.internal\"\nsslmode = \"require\"\n").unwrap();
    std::fs::create_dir(root.join(project::DIR)).unwrap();
    std::os::unix::fs::symlink(&target, project::file_in(&root)).unwrap();
    let m = Machine::new();
    let _ = m.project.open_project(&root, m.st());
    m.config.upsert(conn("shop")).unwrap();
    let _ = m.sync();
    assert!(m.config.get("far").is_none(), "read through the link");
    assert!(std::fs::symlink_metadata(project::file_in(&root)).unwrap().file_type().is_symlink(), "the link was replaced");
    assert!(m.problem().is_some());
}

/// Linked to a private file (an ssh config, the local connections.toml): it does not parse,
/// so it is never written, and the link and its target stay.
#[test]
fn a_project_file_linked_to_a_private_file_is_never_written() {
    let (_d, root) = project_dir();
    let outside = tempfile::tempdir().unwrap();
    let ssh = outside.path().join("config");
    let secret = "Host bastion\n  User deploy\n  IdentityFile ~/.ssh/prod\n";
    std::fs::write(&ssh, secret).unwrap();
    std::fs::create_dir(root.join(project::DIR)).unwrap();
    std::os::unix::fs::symlink(&ssh, project::file_in(&root)).unwrap();
    let m = Machine::new();
    assert!(m.project.open_project(&root, m.st()).is_err());
    assert_eq!(std::fs::read_to_string(&ssh).unwrap(), secret);
    assert!(std::fs::symlink_metadata(project::file_in(&root)).unwrap().file_type().is_symlink());
    // The problem quotes the line it stopped at (shown in Settings › Project only).
    println!("problem: {:?}", m.problem());
    // The local connections.toml: no `version`, so it does not parse either.
    let other = Machine::new();
    other.config.upsert(conn("private")).unwrap();
    let (_d2, root2) = project_dir();
    std::fs::create_dir(root2.join(project::DIR)).unwrap();
    std::os::unix::fs::symlink(other.dirs.connections_file(), project::file_in(&root2)).unwrap();
    let before = std::fs::read_to_string(other.dirs.connections_file()).unwrap();
    assert!(m.project.open_project(&root2, m.st()).is_err());
    assert_eq!(std::fs::read_to_string(other.dirs.connections_file()).unwrap(), before);
}

/// The project's folder opened through a symlink is the real folder.
#[test]
fn a_symlinked_root_is_the_real_folder() {
    let (d, root) = project_dir();
    let link = root.with_file_name("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    let m = Machine::new();
    m.open(&link);
    assert_eq!(m.settings.get().project.as_deref(), Some(root.to_string_lossy().as_ref()));
    assert_eq!(m.project.info(&m.settings.get()).unwrap().root, root);
    assert!(std::fs::symlink_metadata(project::file_in(&root)).unwrap().is_file());
    drop(d);
}

// ---------------------------------------------------------------------------------------------
// 5. Permissions.

/// A read-only project.toml (Perforce keeps files read-only until opened for edit) is not
/// replaced: rename(2) only needs the folder writable, so today it is, and comes back 0644.
#[test]
fn a_read_only_project_file_is_not_replaced() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let path = project::file_in(&root);
    let before = text(&root);
    chmod(&path, 0o444);
    m.config.upsert(conn("more")).unwrap();
    let _ = m.sync();
    let (after, mode_after, problem) = (text(&root), mode(&path), m.problem());
    chmod(&path, 0o644);
    assert_eq!((mode_after, after == before), (0o444, true), "replaced");
    assert!(problem.is_some());
}

/// A read-only `.dataman`: the write fails on every sync (each second), so it is a problem the
/// user sees, not only an Err the app logs at debug level; no temp file is left behind; and
/// the connection created meanwhile still joins the project once the folder is writable.
#[test]
fn a_read_only_dataman_dir_is_a_problem() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let dir = root.join(project::DIR);
    chmod(&dir, 0o555);
    m.config.upsert(conn("more")).unwrap();
    let r1 = m.sync();
    let problem = m.problem();
    let r2 = m.sync();
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    chmod(&dir, 0o755);
    let r3 = m.sync();
    assert!(r1.is_err(), "{r1:?}");
    assert_eq!(left, ["project.toml"], "a temp file left behind");
    assert!(r2.is_err(), "the second sync no longer tries: {r2:?}");
    assert!(problem.is_some(), "the failed write is not shown");
    assert!(r3.is_ok() && text(&root).contains("more.db.internal"), "the connection created meanwhile never joined:\n{}", text(&root));
}

/// What Dataman writes in the folder is an ordinary file (0644 under the usual umask); the
/// local folder is the owner's only.
#[test]
fn what_dataman_writes_has_ordinary_modes() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let p = mode(&project::file_in(&root));
    assert_eq!(p & 0o111, 0, "not executable: {p:o}");
    assert_eq!(p & 0o022, 0, "not writable by others: {p:o}");
    assert_eq!(p & 0o444, 0o444, "readable, as a versioned file: {p:o}");
    assert_eq!(mode(&m.dirs.config), 0o700);
}

/// connections.toml can hold a secret driver option (sslpassword, a token): kept local, but
/// written 0644; only the 0700 folder protects it (a backup or a dotfiles copy keeps 0644).
#[test]
fn local_files_holding_secrets_are_the_owners_only() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    let mut p = s.clone();
    p.options.insert("sslpassword".into(), "hunter2".into());
    m.config.upsert(p).unwrap();
    m.sync().unwrap();
    assert!(std::fs::read_to_string(m.dirs.connections_file()).unwrap().contains("hunter2"));
    for f in [m.dirs.connections_file(), m.dirs.project_base_file(&root)] {
        assert_eq!(mode(&f) & 0o077, 0, "{} is {:o}", f.display(), mode(&f));
    }
}

// ---------------------------------------------------------------------------------------------
// 6. Each person's own: never in the file, never from it.

#[test]
fn personal_settings_never_reach_the_file() {
    let (_d, root) = project_dir();
    let m = Machine::new();
    m.open(&root);
    m.settings
        .update(|s| {
            s.theme = ThemePref::Light;
            s.font = UiFont::Lilex;
            s.system_font = Some("Comic-Xyz".into());
            s.zoom = 1.5;
            s.editor_font_size = 19;
            s.keys.insert("editor.run".into(), "cmd-shift-zz".into());
            s.export_dir = Some("/Users/someone/exports-xyz".into());
            s.mcp_enabled = false;
            s.update_feed = Some("https://feed.example/xyz".into());
            s.crash_report_url = Some("https://crash.example/xyz".into());
        })
        .unwrap();
    let mut s = conn("shop");
    s.save_password = true;
    s.options.insert("sslpassword".into(), "hunter2".into());
    s.options.insert("api_token".into(), "tok-123".into());
    s.options.insert("ssh_private_key".into(), "-----BEGIN".into());
    s.options.insert("client_secret".into(), "cs-456".into());
    m.config.upsert(s).unwrap();
    let mut made = conn("by-agent");
    made.agent_made = true;
    m.config.upsert(made.clone()).unwrap();
    m.sync().unwrap();
    m.project.edit(m.st(), |_, members| {
        members.insert(made.id.clone());
    })
    .unwrap();
    let file = text(&root);
    let base = std::fs::read_to_string(m.dirs.project_base_file(&root)).unwrap();
    for (what, t) in [("project.toml", &file), ("project-base.toml", &base)] {
        for never in [
            "theme", "Lilex", "Comic-Xyz", "zoom", "editor_font_size", "cmd-shift-zz", "exports-xyz", "mcp_enabled", "feed.example", "crash.example",
            "save_password", "agent_made", "hunter2", "tok-123", "BEGIN", "cs-456", "project_enabled",
        ] {
            assert!(!t.contains(never), "{never} in {what}:\n{t}");
        }
    }
}

/// favorites.rs: a favorite saved by an agent "stays this person's, never the open project's".
#[test]
fn agent_made_favorites_stay_personal() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    let mut f = Favorite::new("by agent", "SELECT 42");
    (f.connection_id, f.agent_made) = (Some(s.id.clone()), true);
    m.favorites.add(f).unwrap();
    m.sync().unwrap();
    let t = text(&root);
    assert!(!t.contains("SELECT 42") && !t.contains("agent_made"), "{t}");
}

#[test]
fn the_file_cannot_mark_a_favorite_agent_made() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    replace(&project::file_in(&root), &format!("{}\n[[favorite]]\nid = \"x\"\nname = \"x\"\nsql = \"SELECT 1\"\nconnection_id = \"{}\"\nagent_made = true\n", text(&root), s.id));
    m.sync().unwrap();
    assert!(m.favorites.list().iter().all(|f| !f.agent_made));
}

/// A hostile file with each person's own settings in it changes none of them here, and a
/// connection's personal fields from the file are ignored.
#[test]
fn a_hostile_file_cannot_change_personal_settings() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    m.settings.update(|x| x.export_dir = Some("/mine".into())).unwrap();
    let before = m.settings.get();
    let hostile = text(&root)
        .replace(
            "version = 1\n",
            "version = 1\nmcp_enabled = false\ntheme = \"light\"\nzoom = 3.0\nexport_dir = \"/evil\"\nproject_enabled = false\nproject = \"/elsewhere\"\nupdate_feed = \"https://evil.example\"\n",
        )
        .replace("[editor]\n", "[editor]\ntheme = \"light\"\nmcp_enabled = false\n")
        .replace("name = \"shop\"\n", "name = \"shop\"\nsave_password = true\nagent_made = true\nfrom_project = false\noptions = { sslpassword = \"evil\", api_token = \"evil\" }\n")
        + "\n[[connection]]\nid = \"new\"\nname = \"new\"\nhost = \"new.internal\"\nsslmode = \"require\"\nsave_password = true\nagent_made = true\noptions = { sslpassword = \"evil\" }\n\n[settings]\nmcp_enabled = false\ntheme = \"light\"\n\n[keys]\n\"editor.run\" = \"cmd-q\"\n";
    assert!(hostile.contains("api_token = \"evil\""), "{hostile}");
    replace(&project::file_in(&root), &hostile);
    let r = m.sync();
    assert!(r.is_ok() && m.project.review().is_none(), "{r:?} {:?}", m.project.review());
    let after = m.settings.get();
    assert_eq!(
        (after.mcp_enabled, after.theme, after.zoom, after.export_dir.as_deref(), after.project_enabled, after.project.as_deref(), after.update_feed.as_deref(), after.keys.get("editor.run")),
        (before.mcp_enabled, before.theme, before.zoom, Some("/mine"), true, Some(root.to_string_lossy().as_ref()), before.update_feed.as_deref(), before.keys.get("editor.run"))
    );
    for id in [s.id.as_str(), "new"] {
        let p = m.config.get(id).unwrap();
        assert!(!p.save_password && !p.agent_made && p.options.values().all(|v| v != "evil"), "{p:?}");
    }
    let disk = SettingsStore::open(&m.dirs).get();
    assert_eq!(disk.mcp_enabled, before.mcp_enabled);
}

// ---------------------------------------------------------------------------------------------
// 7. [share] and version.

/// "Each part left out is neither written nor read" (Share): a file that says it shares no
/// connections or favorites but lists some brings none of them.
#[test]
fn unshared_connections_and_favorites_are_not_read() {
    let (_d, root) = project_dir();
    let m = Machine::new();
    m.open(&root);
    replace(
        &project::file_in(&root),
        "version = 1\n[share]\nconnections = false\nfavorites = false\n[[connection]]\nid = \"c\"\nname = \"c\"\nhost = \"c.internal\"\nsslmode = \"require\"\n\
         [[favorite]]\nid = \"f\"\nname = \"f\"\nsql = \"SELECT 1\"\nconnection_id = \"c\"\n",
    );
    let _ = m.sync();
    assert!(m.config.get("c").is_none(), "an unshared connection was read");
    assert!(m.favorites.list().is_empty(), "an unshared favorite was read");
}

#[test]
fn an_unshared_editor_format_is_not_read() {
    let (_d, root) = project_dir();
    let m = Machine::new();
    m.open(&root);
    m.settings.update(|s| s.indent = 4).unwrap();
    replace(&project::file_in(&root), "version = 1\n[share]\neditor = false\n[editor]\nindent = 8\nkeyword_case = \"lower\"\n");
    let _ = m.sync();
    assert_eq!(m.settings.get().indent, 4);
}

/// share.agents = false: the agents' rules (sensitive data types, each connection's mcp
/// fields) in the file are not read, for a connection this side already has or a new one.
#[test]
fn unshared_agent_rules_are_not_read() {
    let (_d, root) = project_dir();
    let m = Machine::new();
    m.open(&root);
    replace(
        &project::file_in(&root),
        "version = 1\n[share]\nagents = false\n[agents.sensitive_data.builtin.email]\non = false\n\
         [[connection]]\nid = \"n\"\nname = \"n\"\nhost = \"n.internal\"\nsslmode = \"require\"\nmcp = \"full\"\nmcp_yolo = true\nmcp_row_limit = 1000000\n",
    );
    let _ = m.sync();
    assert_eq!(m.settings.get().agent_types, Default::default(), "sensitive data types from an unshared [agents]");
    let n = m.config.get("n").unwrap();
    assert_eq!((n.mcp, n.mcp_yolo, n.mcp_row_limit), (None, false, None), "agent rules from an unshared file");
}

/// Agents shared: a file that loosens a connection's rules (read → full, yolo on) does not
/// silently apply (held for the person's review).
#[test]
fn a_file_loosening_agent_rules_is_not_applied_silently() {
    let (_d, root) = project_dir();
    let (m, s) = opened(&root);
    let mut p = m.config.get(&s.id).unwrap();
    p.mcp = Some(McpAccess::Read);
    m.config.upsert(p).unwrap();
    m.sync().unwrap();
    let loose = text(&root).replace("mcp = \"read\"", "mcp = \"full\"\nmcp_yolo = true");
    assert!(loose.contains("mcp_yolo"), "{loose}");
    replace(&project::file_in(&root), &loose);
    let _ = m.sync();
    let got = m.config.get(&s.id).unwrap();
    assert_eq!((got.mcp, got.mcp_yolo), (Some(McpAccess::Read), false));
}

/// `version = 2` (a newer Dataman's file) is refused with a problem: nothing applied, and the
/// file not rewritten as version 1 without what this version does not know.
#[test]
fn a_future_version_is_refused() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let future = format!("{}\n[[connection]]\nid = \"v2\"\nname = \"v2\"\nhost = \"v2.internal\"\nsslmode = \"require\"\n", text(&root).replace("version = 1", "version = 2"));
    replace(&project::file_in(&root), &future);
    m.config.upsert(conn("local-new")).unwrap();
    let r = m.sync();
    assert!(r.is_err() && m.problem().is_some());
    assert!(m.config.get("v2").is_none());
    assert_eq!(text(&root), future, "rewritten");
}

/// No `version` at all: not a project file; a problem, the file left as it is.
#[test]
fn a_file_without_a_version_is_refused() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    let t = "[[connection]]\nid = \"x\"\nname = \"x\"\n";
    replace(&project::file_in(&root), t);
    assert!(m.sync().is_err());
    assert!(m.problem().is_some());
    assert_eq!(text(&root), t);
    assert!(m.config.get("x").is_none());
}

/// A connection this version cannot read (a newer engine) is skipped on read, as connections.toml
/// does, but connections.toml keeps it on write; the project's file must too.
#[test]
fn connections_this_version_cannot_read_stay_in_the_file() {
    let (_d, root) = project_dir();
    let (m, _) = opened(&root);
    replace(&project::file_in(&root), &format!("{}\n[[connection]]\nid = \"future\"\nname = \"future\"\nengine = \"cassandra\"\n", text(&root)));
    m.sync().unwrap();
    m.config.upsert(conn("local-new")).unwrap();
    m.sync().unwrap();
    assert!(text(&root).contains("cassandra"), "a teammate's connection was dropped:\n{}", text(&root));
}
