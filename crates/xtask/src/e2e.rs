//! `xtask e2e`: the end-to-end suite. Brings up the databases of `e2e/compose.yaml` (reusing a
//! server already listening on a port), reloads the seeds, then runs every check stage of
//! `target/release/dataman --demo` one after the other and sums up their PASS/FAIL lines.

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Db {
    Pg,
    MySql,
    MsSql,
    Mongo,
    Es,
    OpenSearch,
    Redis,
    Ssh,
}

use Db::*;

impl Db {
    fn service(self) -> &'static str {
        match self {
            Pg => "pg",
            MySql => "mysql",
            MsSql => "mssql",
            Mongo => "mongo",
            Es => "es",
            OpenSearch => "opensearch",
            Redis => "redis",
            Ssh => "ssh",
        }
    }

    fn port(self) -> u16 {
        match self {
            Pg => 55432,
            MySql => 33306,
            MsSql => 14330,
            Mongo => 27027,
            Es => 9201,
            OpenSearch => 9200,
            Redis => 63790,
            Ssh => 2299,
        }
    }
}

struct Stage {
    name: &'static str,
    needs: &'static [Db],
    secs: u64,
}

const fn st(name: &'static str, needs: &'static [Db], secs: u64) -> Stage {
    Stage { name, needs, secs }
}

const SQL3: &[Db] = &[Pg, MySql, MsSql];

/// Run in this order; every stage connects PostgreSQL first.
const STAGES: &[Stage] = &[
    st("newuser-check", SQL3, 300),
    st("connections-check", &[Pg, MySql, MsSql, Mongo, Es, OpenSearch, Redis, Ssh], 400),
    st("conns-check", &[Pg], 120),
    st("ui-check", &[Pg], 400),
    st("tabs-check", &[Pg, Mongo, Redis], 200),
    st("split-check", &[Pg], 300),
    st("session-check", &[Pg], 120),
    st("dock-check", &[Pg], 120),
    st("pin-check", &[Pg], 120),
    st("history-check", &[Pg], 300),
    st("history-filter-check", &[Pg, MySql], 200),
    st("dropdowns-check", &[Pg], 120),
    st("folders-check", &[Pg], 200),
    st("undo-check", &[Pg], 200),
    st("explorer-check", SQL3, 300),
    st("fk-check", &[Pg], 200),
    st("fk-pop-check", &[Pg], 200),
    st("fk-nav-check", &[Pg], 200),
    st("find-check", &[Pg], 200),
    st("results-check", &[Pg], 200),
    st("value-check", &[Pg], 120),
    st("table-check", &[Pg], 200),
    st("edit-check", &[Pg], 200),
    st("json-check", SQL3, 300),
    st("tx-check", &[Pg], 300),
    st("insert-check", SQL3, 300),
    st("mysql-check", &[Pg, MySql], 300),
    st("mssql-check", &[Pg, MsSql], 300),
    st("intel-check", &[Pg], 200),
    st("plan-check", &[Pg], 200),
    st("activity-check", &[Pg], 200),
    st("input-check", &[Pg], 120),
    st("export-check", &[Pg], 200),
    st("import-check", &[Pg], 300),
    st("settings-check", &[Pg], 200),
    st("welcome-check", &[Pg], 120),
    st("palette-check", &[Pg], 120),
    st("nosql-check", &[Pg, Mongo, Es, Redis], 400),
    st("nosql-engines-check", &[Pg, Mongo, Es, OpenSearch, Redis], 300),
    st("mcp-check", &[Pg], 300),
    st("mcp-tools-check", &[Pg, MySql, MsSql, Mongo, Es, OpenSearch, Redis], 400),
    st("update-check", &[Pg], 300),
];

struct Opts {
    only: Vec<String>,
    skip: Vec<String>,
    build: bool,
    fresh: bool,
    reset: bool,
    verbose: bool,
}

fn csv(args: &[String], name: &str) -> Vec<String> {
    crate::flag(args, name).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--list") {
        for s in STAGES {
            println!("{:<18} {}", s.name, s.needs.iter().map(|d| d.service()).collect::<Vec<_>>().join(" "));
        }
        return Ok(());
    }
    let o = Opts {
        only: csv(args, "--only"),
        skip: csv(args, "--skip"),
        build: !args.iter().any(|a| a == "--no-build"),
        fresh: args.iter().any(|a| a == "--fresh"),
        reset: !args.iter().any(|a| a == "--no-reset"),
        verbose: args.iter().any(|a| a == "-v" || a == "--verbose"),
    };
    for n in o.only.iter().chain(&o.skip) {
        if !STAGES.iter().any(|s| s.name == n) {
            bail!("no stage {n} (`xtask e2e --list`)");
        }
    }
    let stages: Vec<&Stage> = STAGES
        .iter()
        .filter(|s| (o.only.is_empty() || o.only.iter().any(|n| n == s.name)) && !o.skip.iter().any(|n| n == s.name))
        .collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let t0 = Instant::now();
    if o.build {
        step("build");
        // The stages live behind the `checks` feature: the app as shipped has none of them.
        let st = Command::new("cargo").args(["build", "--release", "-p", "dataman", "-p", "dataman-mcp", "--features", "dataman/checks"]).current_dir(&root).status()?;
        if !st.success() {
            bail!("the build failed");
        }
    }
    let app = root.join("target/release/dataman");
    if !app.exists() {
        bail!("{} is missing (drop --no-build)", app.display());
    }
    let needs: BTreeSet<Db> = stages.iter().flat_map(|s| s.needs.iter().copied()).collect();
    step("databases");
    up(&root, &needs, o.fresh)?;
    if o.reset {
        step("seeds");
        reset(&root, &needs)?;
    }
    fixtures(&root)?;
    let stamp = crate::now_utc().replace([':', '-'], "");
    let dir = root.join("target/e2e").join(&stamp);
    std::fs::create_dir_all(&dir)?;
    step(&format!("{} stages, logs in {}", stages.len(), dir.display()));
    let mut rows = Vec::new();
    for s in &stages {
        let r = run_stage(&root, &app, s, &dir, o.verbose)?;
        let mark = if r.ok() { "✓" } else { "✗" };
        println!("{mark} {:<18} {:>4}/{:<4} {:>6.1}s{}", s.name, r.pass, r.pass + r.fail.len(), r.secs, r.note.as_deref().map(|n| format!("  {n}")).unwrap_or_default());
        for f in &r.fail {
            println!("    FAIL {f}");
        }
        rows.push((s.name, r));
    }
    let failed: Vec<&str> = rows.iter().filter(|(_, r)| !r.ok()).map(|(n, _)| *n).collect();
    let (pass, total) = rows.iter().fold((0, 0), |(p, t), (_, r)| (p + r.pass, t + r.pass + r.fail.len()));
    let mut summary = format!(
        "E2E: {}/{} stages passed, {pass}/{total} checks, {:.0}s",
        rows.len() - failed.len(),
        rows.len(),
        t0.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        summary.push_str(&format!(" — failed: {}", failed.join(", ")));
    }
    println!("\n{summary}");
    std::fs::write(dir.join("summary.txt"), format!("{summary}\n"))?;
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn step(what: &str) {
    println!("── {what}");
}

fn answers(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn compose(root: &Path) -> Command {
    let mut c = Command::new("docker");
    c.args(["compose", "-f"]).arg(root.join("e2e/compose.yaml")).current_dir(root.join("e2e"));
    c
}

fn up(root: &Path, needs: &BTreeSet<Db>, fresh: bool) -> Result<()> {
    if needs.contains(&Ssh) {
        let keys = root.join("target/e2e/ssh");
        std::fs::create_dir_all(&keys)?;
        if !keys.join("id_ed25519").exists() {
            let st = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "dataman-e2e", "-f"]).arg(keys.join("id_ed25519")).status()?;
            if !st.success() {
                bail!("ssh-keygen failed");
            }
        }
    }
    if fresh {
        let st = compose(root).args(["down", "-v"]).status()?;
        if !st.success() {
            bail!("docker compose down failed");
        }
    }
    let mut start = Vec::new();
    for db in needs {
        if answers(db.port()) {
            println!("   {:<11} :{} already up, reused", db.service(), db.port());
        } else {
            start.push(db.service());
        }
    }
    if !start.is_empty() {
        println!("   starting {}", start.join(" "));
        let st = compose(root).args(["up", "-d", "--wait"]).args(&start).status()?;
        if !st.success() {
            bail!("docker compose up failed for {}", start.join(" "));
        }
    }
    Ok(())
}

fn password(db: Db) -> String {
    let (var, default) = match db {
        Pg => ("DATAMAN_PG_PASSWORD", "dataman"),
        MySql => ("DATAMAN_MYSQL_PASSWORD", "dataman-root"),
        MsSql => ("DATAMAN_MSSQL_PASSWORD", "Dataman-sa-1!"),
        Mongo => ("DATAMAN_MONGO_PASSWORD", "dataman"),
        Es => ("DATAMAN_ES_PASSWORD", "dataman"),
        Redis => ("DATAMAN_REDIS_PASSWORD", "dataman"),
        OpenSearch | Ssh => return String::new(),
    };
    std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into())
}

/// The container that publishes `port` (a compose service or one made by hand).
fn container(port: u16) -> Result<String> {
    let out = Command::new("docker").args(["ps", "--filter", &format!("publish={port}"), "--format", "{{.Names}}"]).output()?;
    String::from_utf8_lossy(&out.stdout).lines().next().map(str::to_string).with_context(|| format!("no container publishes port {port}: the seeds load through `docker exec`"))
}

fn exec(container: &str, cmd: &[&str], input: Option<&Path>) -> Result<String> {
    let mut c = Command::new("docker");
    c.arg("exec").arg("-i").arg(container).args(cmd);
    c.stdin(match input {
        Some(p) => Stdio::from(std::fs::File::open(p).with_context(|| format!("reading {}", p.display()))?),
        None => Stdio::null(),
    });
    let out = c.output()?;
    if !out.status.success() {
        bail!("{} in {container}: {}{}", cmd.join(" "), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sha(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(data_encoding::HEXLOWER.encode(&ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()[..8]))
}

/// The big PostgreSQL seed (about a minute) loads once per content: its hash is kept as the
/// database's comment, where no check looks. Everything else reloads on every run.
fn reset(root: &Path, needs: &BTreeSet<Db>) -> Result<()> {
    let seeds = root.join("seeds");
    if needs.contains(&Pg) {
        let c = container(Pg.port())?;
        let pw = format!("PGPASSWORD={}", password(Pg));
        let psql = |args: &[&str], file: Option<&Path>| {
            let mut cmd = vec!["env", pw.as_str(), "psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "dataman", "-v", "ON_ERROR_STOP=1", "-q"];
            cmd.extend_from_slice(args);
            if file.is_some() {
                cmd.extend_from_slice(&["-f", "-"]);
            }
            exec(&c, &cmd, file)
        };
        let want = format!("dataman-seed {}", sha(&seeds.join("postgres.sql"))?);
        let have = psql(&["-tAc", "SELECT coalesce(shobj_description(oid, 'pg_database'), '') FROM pg_database WHERE datname = 'dataman'"], None)?;
        if have != want {
            let t = Instant::now();
            println!("   pg: loading seeds/postgres.sql (about a minute)");
            psql(&[], Some(&seeds.join("postgres.sql")))?;
            psql(&["-c", &format!("COMMENT ON DATABASE dataman IS '{want}'")], None)?;
            println!("   pg: loaded in {:.0}s", t.elapsed().as_secs_f64());
        }
        psql(&[], Some(&seeds.join("phase2.sql")))?;
        psql(&[], Some(&seeds.join("e2e-pg.sql")))?;
        println!("   pg: reset");
    }
    if needs.contains(&MySql) {
        let c = container(MySql.port())?;
        exec(&c, &["mysql", "-uroot", &format!("-p{}", password(MySql))], Some(&seeds.join("mysql.sql")))?;
        println!("   mysql: reset");
    }
    if needs.contains(&MsSql) {
        let c = container(MsSql.port())?;
        let pw = password(MsSql);
        exec(&c, &["/opt/mssql-tools18/bin/sqlcmd", "-C", "-I", "-b", "-S", "localhost", "-U", "sa", "-P", &pw], Some(&seeds.join("mssql.sql")))?;
        println!("   mssql: reset");
    }
    Ok(())
}

/// libpq's files for the import checks, so a stage never reads the user's own (~/.pgpass, the
/// service files): one service and the pgpass line with its password.
fn fixtures(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = root.join("target/e2e/libpq");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("pg_service.conf"), format!("[e2e-service]\nhost=127.0.0.1\nport={}\ndbname=dataman_bench\nuser=postgres\n", Pg.port()))?;
    let pgpass = dir.join("pgpass");
    std::fs::write(&pgpass, format!("127.0.0.1:{}:*:postgres:{}\n", Pg.port(), password(Pg)))?;
    std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn stage_env(root: &Path) -> Vec<(String, String)> {
    let libpq = root.join("target/e2e/libpq");
    let mut env = vec![
        ("DATAMAN_NO_NOTIFY".into(), "1".into()),
        ("DATAMAN_BENCH_PG".into(), format!("127.0.0.1:{}:postgres:{}:dataman", Pg.port(), password(Pg))),
        ("DATAMAN_BENCH_MYSQL".into(), format!("127.0.0.1:{}:root:dataman_test", MySql.port())),
        ("DATAMAN_MYSQL_PASSWORD".into(), password(MySql)),
        ("DATAMAN_BENCH_MSSQL".into(), format!("127.0.0.1:{}:sa:dataman_test", MsSql.port())),
        ("DATAMAN_MSSQL_PASSWORD".into(), password(MsSql)),
        ("DATAMAN_BENCH_MONGO".into(), format!("127.0.0.1:{}:root:{}", Mongo.port(), password(Mongo))),
        ("DATAMAN_BENCH_ES".into(), format!("127.0.0.1:{}:elastic:{}", Es.port(), password(Es))),
        ("DATAMAN_BENCH_OPENSEARCH".into(), format!("127.0.0.1:{}", OpenSearch.port())),
        ("DATAMAN_BENCH_REDIS".into(), format!("127.0.0.1:{}:{}", Redis.port(), password(Redis))),
        ("DATAMAN_BENCH_SSH".into(), format!("127.0.0.1:{}:dataman", Ssh.port())),
        ("DATAMAN_BENCH_SSH_KEY".into(), root.join("target/e2e/ssh/id_ed25519").display().to_string()),
    ];
    env.retain(|(k, _)| std::env::var_os(k).is_none());
    env.extend([
        ("PGPASSFILE".into(), libpq.join("pgpass").display().to_string()),
        ("PGSERVICEFILE".into(), libpq.join("pg_service.conf").display().to_string()),
        ("PGSYSCONFDIR".into(), libpq.display().to_string()),
    ]);
    env
}

struct Outcome {
    pass: usize,
    fail: Vec<String>,
    secs: f64,
    note: Option<String>,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.fail.is_empty() && self.note.is_none()
    }
}

fn run_stage(root: &Path, app: &Path, s: &Stage, dir: &Path, verbose: bool) -> Result<Outcome> {
    let t = Instant::now();
    let log_path: PathBuf = dir.join(format!("{}.log", s.name));
    let mut log = std::fs::File::create(&log_path)?;
    let mut child = Command::new(app)
        .arg("--demo")
        .current_dir(root)
        .env("DATAMAN_SHOT_STAGE", s.name)
        .envs(stage_env(root))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", app.display()))?;
    let (tx, rx) = mpsc::channel::<String>();
    for r in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let (mut pass, mut fail) = (0, Vec::new());
    let deadline = t + Duration::from_secs(s.secs);
    let mut note = None;
    let mut take = |line: String, log: &mut std::fs::File| {
        let _ = writeln!(log, "{line}");
        if verbose {
            println!("    {line}");
        }
        let l = line.trim_start();
        if l.starts_with("PASS ") {
            pass += 1;
        } else if let Some(f) = l.strip_prefix("FAIL ") {
            fail.push(f.trim().to_string());
        }
    };
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => take(line, &mut log),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break child.wait()?,
        }
        if let Some(st) = child.try_wait()? {
            while let Ok(line) = rx.recv_timeout(Duration::from_millis(300)) {
                take(line, &mut log);
            }
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            note = Some(format!("timed out after {}s", s.secs));
            break child.wait()?;
        }
    };
    if note.is_none() && !status.success() && fail.is_empty() {
        note = Some(format!("exited with {status} (log {})", log_path.display()));
    }
    if note.is_none() && pass == 0 && fail.is_empty() {
        note = Some("no checks ran".into());
    }
    Ok(Outcome { pass, fail, secs: t.elapsed().as_secs_f64(), note })
}
