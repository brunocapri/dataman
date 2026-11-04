//! Ignored by default: it runs this machine's cloud CLIs, signed in as their user. Each cloud
//! is skipped when its CLI is not signed in; `DATAMAN_LIVE_CLOUD=az,aws,gcloud` makes the named
//! ones required. Only the CLIs are asked for tokens, except where a database is named:
//! `DATAMAN_LIVE_ENTRA=<engine>:<host>/<database>[:<user>]` (engine `mssql`, `pg` or `mysql`)
//! dials it with an Entra token.

mod common;

use common::*;
use dataman_core::cloudauth::TokenSource;
use dataman_core::{Auth, ConnectionProfile, Engine};

fn profile(engine: Engine, host: &str, user: &str, auth: Auth) -> ConnectionProfile {
    let mut p = ConnectionProfile::new("cloud");
    (p.engine, p.host, p.port, p.user, p.auth) = (engine, host.into(), engine.default_port(), user.into(), auth);
    p
}

fn required(cli: &str) -> bool {
    std::env::var("DATAMAN_LIVE_CLOUD").is_ok_and(|v| v.split(',').any(|c| c.trim() == cli))
}

#[tokio::test]
#[ignore]
async fn entra_tokens_from_az() {
    let src = TokenSource::default();
    let cases = [
        profile(Engine::Postgres, "live.postgres.database.azure.com", "ana@contoso.com", Auth::EntraId { tenant: None }),
        profile(Engine::MySql, "live.mysql.database.azure.com", "ana@contoso.com", Auth::EntraId { tenant: None }),
        profile(Engine::SqlServer, "live.database.windows.net", "", Auth::EntraId { tenant: None }),
    ];
    for p in &cases {
        match src.check(p).await {
            Ok(s) => {
                assert!(s.who.as_deref().is_some_and(|w| !w.is_empty()), "{:?}: who {s:?}", p.engine);
                assert!(s.valid_for.as_secs() > 5 * 60, "{:?}: {s:?}", p.engine);
                let t = src.token(p).await.unwrap();
                assert_eq!(t.split('.').count(), 3, "a JWT");
                eprintln!("{:?}: {} for {} min", p.engine, s.who.unwrap_or_default(), s.valid_for.as_secs() / 60);
            }
            Err(e) if !required("az") => {
                eprintln!("az skipped: {e}");
                return;
            }
            Err(e) => panic!("{:?}: {e}", p.engine),
        }
    }
}

#[tokio::test]
#[ignore]
async fn rds_tokens_from_aws() {
    // Signing is local: any endpoint gets a token once the CLI has credentials.
    let p = profile(Engine::Postgres, "live.abc123.us-east-1.rds.amazonaws.com", "dataman_iam", Auth::AwsIam { profile: None, region: None });
    match TokenSource::default().token(&p).await {
        Ok(t) => assert!(t.starts_with("live.abc123.us-east-1.rds.amazonaws.com:5432/?") && t.contains("X-Amz-Signature="), "{t}"),
        Err(e) if !required("aws") => eprintln!("aws skipped: {e}"),
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
#[ignore]
async fn cloud_sql_tokens_from_gcloud() {
    let p = profile(Engine::Postgres, "live.sql.goog", "ana@example.com", Auth::GoogleIam { account: None });
    match TokenSource::default().token(&p).await {
        Ok(t) => assert!(!t.is_empty()),
        Err(e) if !required("gcloud") => eprintln!("gcloud skipped: {e}"),
        Err(e) => panic!("{e}"),
    }
}

#[test]
#[ignore]
fn dials_with_an_entra_token() {
    let Ok(spec) = std::env::var("DATAMAN_LIVE_ENTRA") else {
        eprintln!("skipped: DATAMAN_LIVE_ENTRA not set");
        return;
    };
    let (engine, rest) = spec.split_once(':').expect("engine:host/database[:user]");
    let (target, user) = rest.split_once(':').unwrap_or((rest, ""));
    let (host, database) = target.split_once('/').expect("host/database");
    let (engine, who) = match engine {
        "mssql" => (Engine::SqlServer, "SELECT SUSER_SNAME(), DB_NAME()"),
        "pg" => (Engine::Postgres, "SELECT current_user, current_database()"),
        "mysql" => (Engine::MySql, "SELECT CURRENT_USER(), DATABASE()"),
        e => panic!("engine {e}"),
    };
    let mut p = profile(engine, host, user, Auth::EntraId { tenant: None });
    p.database = database.into();
    p.sslmode = dataman_core::SslMode::VerifyFull;
    let e = env();
    let c = e.core.block_on(e.core.connect(p.clone(), None)).unwrap_or_else(|err| panic!("{err:#}"));
    // Every session is a new login: the cached token signs them all in.
    for _ in 0..3 {
        let s = c.session(None);
        let rs = s.execute(who);
        rs.fetch_all();
        let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
        assert_eq!(ph, Phase::Done, "{:?}", rs.status().error);
        let snap = rs.snapshot();
        let row: Vec<String> = (0..snap.columns.len()).map(|col| snap.text(0, col).map(|t| t.into_owned()).unwrap_or_default()).collect();
        eprintln!("{engine:?} signed in: {row:?}");
        assert!(row.iter().all(|v| !v.is_empty()), "{row:?}");
    }
    // A password, typed or not, never goes in place of the token.
    let (info, _) = e.core.block_on(e.core.test_connection(p, Some("not-a-password".into()), None)).unwrap_or_else(|err| panic!("{err:#}"));
    eprintln!("test connection: {info:?}");
}
