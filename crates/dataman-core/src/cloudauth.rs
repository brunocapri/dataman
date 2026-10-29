//! Passwordless sign-in to cloud databases, through the cloud CLI the user already signed in
//! with: Microsoft Entra ID (`az account get-access-token`), AWS RDS IAM (`aws rds
//! generate-db-auth-token`, which takes profiles and IAM Identity Center) and Google Cloud SQL
//! IAM (`gcloud sql generate-login-token`). Dataman registers nothing anywhere: the CLI holds
//! the user's session and mints a short-lived token, which goes where a password would.
//!
//! A server checks a token only at login, so a token is asked for on every new connection (a
//! tab, another database), from a cache that keeps it until a few minutes before it expires: one
//! CLI run per method and server at a time, whatever the number of tabs. Tokens live in memory
//! only: never in the keychain, a file, history or a log.
//!
//! An Entra or Google token is a bearer token, valid for every database of that user wherever
//! it is sent: a connection a project's file or an agent pointed somewhere gets one only for a
//! host of that cloud (see [`host_allowed`]), and never through its tunnel. An AWS token is
//! signed for its host and is useless anywhere else.
//!
//! The CLI runs with fixed arguments (`--name=value`, values checked), no shell, no input, a
//! timeout, and a PATH that also has the usual install folders (an app started from the Dock
//! does not get the shell's).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::config::{Auth, AuthKind, ConnectionProfile, Engine};

const TIMEOUT: Duration = Duration::from_secs(30);
/// A cached token is handed out while it has more than this left.
const MARGIN: Duration = Duration::from_secs(5 * 60);
/// A token whose expiry the CLI does not tell (gcloud's) is taken to last this long.
const UNTOLD: Duration = Duration::from_secs(15 * 60);
/// RDS tokens: 15 minutes.
const AWS_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cli {
    Az,
    Aws,
    Gcloud,
}

impl Cli {
    pub fn of(kind: AuthKind) -> Option<Cli> {
        match kind {
            AuthKind::Password => None,
            AuthKind::EntraId => Some(Cli::Az),
            AuthKind::AwsIam => Some(Cli::Aws),
            AuthKind::GoogleIam => Some(Cli::Gcloud),
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Cli::Az => "az",
            Cli::Aws => "aws",
            Cli::Gcloud => "gcloud",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Cli::Az => "Azure CLI",
            Cli::Aws => "AWS CLI",
            Cli::Gcloud => "Google Cloud CLI",
        }
    }

    pub fn install_url(self) -> &'static str {
        match self {
            Cli::Az => "https://learn.microsoft.com/cli/azure/install-azure-cli",
            Cli::Aws => "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html",
            Cli::Gcloud => "https://cloud.google.com/sdk/docs/install",
        }
    }

    fn cloud(self) -> &'static str {
        match self {
            Cli::Az => "Azure",
            Cli::Aws => "AWS",
            Cli::Gcloud => "Google Cloud",
        }
    }
}

/// Why no token: said so the user knows what to do (the command to run, the page to install from).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignInError {
    pub title: String,
    pub body: String,
}

impl SignInError {
    fn new(title: impl Into<String>, body: impl Into<String>) -> SignInError {
        SignInError { title: title.into(), body: body.into() }
    }
}

impl std::fmt::Display for SignInError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.body.is_empty() { write!(f, "{}", self.title) } else { write!(f, "{}. {}", self.title, self.body) }
    }
}

impl std::error::Error for SignInError {}

/// A token, zeroed when dropped.
struct Token {
    secret: String,
    expires: SystemTime,
}

impl Drop for Token {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.secret).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

impl Token {
    fn usable(&self, now: SystemTime) -> bool {
        self.expires.duration_since(now).is_ok_and(|left| left > MARGIN)
    }
}

/// What the CLI is asked: also the cache's key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Request {
    cli: Cli,
    args: Vec<String>,
}

/// Who the CLI signs in as, for the connection dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedIn {
    pub who: Option<String>,
    pub valid_for: Duration,
}

// ---- Hosts ---------------------------------------------------------------------------------

/// The cloud's own database hosts (the public, US Government and China clouds; private-link
/// names are under them too).
const ENTRA_HOSTS: [&str; 6] = [
    ".database.windows.net",
    ".database.azure.com",
    ".database.usgovcloudapi.net",
    ".database.chinacloudapi.cn",
    ".sql.azuresynapse.net",
    ".datawarehouse.fabric.microsoft.com",
];
const AWS_HOSTS: [&str; 2] = [".rds.amazonaws.com", ".rds.amazonaws.com.cn"];
/// Cloud SQL's DNS names (Private Service Connect, the instance's DNS name). Its plain IPs are
/// nobody's in particular: no list can tell them.
const GOOGLE_HOSTS: [&str; 1] = [".sql.goog"];

/// Whether `host` is one of the cloud's own database hosts: where a connection the user did not
/// set up in Dataman (a project's file, an agent) may get a token.
pub fn host_allowed(kind: AuthKind, host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let list: &[&str] = match kind {
        AuthKind::Password => return true,
        AuthKind::EntraId => &ENTRA_HOSTS,
        AuthKind::AwsIam => &AWS_HOSTS,
        AuthKind::GoogleIam => &GOOGLE_HOSTS,
    };
    // A name of letters, digits, dots and hyphens only (no userinfo, port or path tricks).
    let plain = h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') && !h.contains("..");
    plain && list.iter().any(|s| h.ends_with(s) && h.len() > s.len())
}

/// The token never leaves for a host the user did not choose in Dataman: a connection from a
/// project's file or made by an agent gets one only for the cloud's own hosts, and no bearer
/// token goes through its tunnel (the tunnel's end would see where TLS is not verified).
pub fn guard(p: &ConnectionProfile) -> Result<(), SignInError> {
    let kind = p.auth.kind();
    if kind == AuthKind::Password {
        return Ok(());
    }
    if !p.engine.supports(kind) {
        return Err(SignInError::new(format!("{} does not sign in with {}", p.engine.label(), kind.label()), "Pick Password in the connection's settings."));
    }
    if !(p.agent_made || p.from_project) {
        return Ok(());
    }
    let whose = if p.from_project { "an open project's file" } else { "an agent" };
    if !host_allowed(kind, &p.host) {
        return Err(SignInError::new(
            format!("No {} token for {}", kind.label(), p.host),
            format!("The connection comes from {whose} and the host is not a known {} database address: open it in Edit Connection and save it to send your token there.", Cli::of(kind).map_or("", Cli::cloud)),
        ));
    }
    if p.ssh.is_some() && kind != AuthKind::AwsIam {
        return Err(SignInError::new(
            format!("No {} token through this SSH tunnel", kind.label()),
            format!("The connection comes from {whose}: open it in Edit Connection and save it to send your token through the tunnel."),
        ));
    }
    Ok(())
}

/// The region in an RDS endpoint: `name.id.REGION.rds.amazonaws.com`.
pub fn aws_region_of(host: &str) -> Option<String> {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').collect();
    let rds = labels.iter().rposition(|l| *l == "rds")?;
    let region = labels.get(rds.checked_sub(1)?)?;
    let ok = region.split('-').count() >= 3 && region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| region.to_string())
}

// ---- Arguments -----------------------------------------------------------------------------

/// What a tenant, profile, region, account, host or user may be to reach a CLI: plain characters,
/// no leading `-`.
pub fn plain_value(v: &str) -> bool {
    !v.is_empty() && v.len() <= 256 && !v.starts_with('-') && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.@:/+-".contains(&b))
}

/// A value the CLI gets as `--name=value`: plain characters only, so a project's file cannot
/// smuggle a flag or anything a shell would read (there is no shell either).
fn arg(what: &str, v: &str) -> Result<String, SignInError> {
    let v = v.trim();
    if plain_value(v) { Ok(v.to_string()) } else { Err(SignInError::new(format!("The {what} {v:?} cannot be passed to the cloud CLI"), "Use letters, digits and . _ @ : / + - only.")) }
}

fn opt(name: &str, what: &str, v: Option<&str>) -> Result<Option<String>, SignInError> {
    match v.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Ok(Some(format!("--{name}={}", arg(what, v)?))),
        None => Ok(None),
    }
}

/// The token's audience for SQL Server: the database resource of the cloud the host is in.
fn sql_resource(host: &str) -> &'static str {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.ends_with(".usgovcloudapi.net") {
        "https://database.usgovcloudapi.net/"
    } else if h.ends_with(".chinacloudapi.cn") {
        "https://database.chinacloudapi.cn/"
    } else {
        "https://database.windows.net/"
    }
}

fn request(p: &ConnectionProfile) -> Result<Request, SignInError> {
    let mut args: Vec<String> = Vec::new();
    let cli = match &p.auth {
        Auth::Password => return Err(SignInError::new("This connection signs in with a password", "")),
        Auth::EntraId { tenant } => {
            args.extend(["account", "get-access-token", "--output=json"].map(String::from));
            // PostgreSQL and MySQL take the `ossrdbms` audience (Azure MySQL refuses any other
            // from 2027); SQL Server its database resource.
            args.push(if p.engine == Engine::SqlServer { format!("--resource={}", sql_resource(&p.host)) } else { "--resource-type=oss-rdbms".into() });
            args.extend(opt("tenant", "tenant", tenant.as_deref())?);
            Cli::Az
        }
        Auth::AwsIam { profile, region } => {
            let region = match region.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                Some(r) => r.to_string(),
                None => aws_region_of(&p.host).ok_or_else(|| SignInError::new("No AWS region", format!("{} is not an RDS endpoint name: set the region in the connection's settings.", p.host)))?,
            };
            if p.user.trim().is_empty() {
                return Err(SignInError::new("No database user", "AWS IAM signs in as a database user granted rds_iam (PostgreSQL) or created WITH AWSAuthenticationPlugin (MySQL)."));
            }
            args.extend(["rds", "generate-db-auth-token", "--output=text"].map(String::from));
            args.push(format!("--hostname={}", arg("host", &p.host)?));
            args.push(format!("--port={}", p.port));
            args.push(format!("--username={}", arg("user", &p.user)?));
            args.push(format!("--region={}", arg("region", &region)?));
            args.extend(opt("profile", "profile", profile.as_deref())?);
            Cli::Aws
        }
        Auth::GoogleIam { account } => {
            args.extend(["sql", "generate-login-token", "--quiet"].map(String::from));
            args.extend(opt("account", "account", account.as_deref())?);
            Cli::Gcloud
        }
    };
    Ok(Request { cli, args })
}

// ---- Running the CLI -------------------------------------------------------------------------

/// Where the CLIs are looked for: the PATH, then where their installers put them.
fn default_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/snap/bin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/opt/homebrew/share/google-cloud-sdk/bin",
        "/usr/local/share/google-cloud-sdk/bin",
        "/usr/lib/google-cloud-sdk/bin",
        "/usr/local/aws-cli",
    ] {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) {
        for d in [".local/bin", "bin", "google-cloud-sdk/bin"] {
            dirs.push(home.join(d));
        }
    }
    // Absolute folders only: a relative PATH entry would be wherever the app was started.
    dirs.retain(|d| d.is_absolute());
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

fn locate(program: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    dirs.iter().map(|d| d.join(program)).find(|f| std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Runs the CLI: its standard output, or what went wrong said plainly.
async fn run(cli: Cli, args: &[String], dirs: &[PathBuf], auth: &Auth) -> Result<String, SignInError> {
    let Some(path) = locate(cli.program(), dirs) else {
        return Err(SignInError::new(format!("{} ({}) not found", cli.name(), cli.program()), format!("Install it ({}), sign in once, then try again.", cli.install_url())));
    };
    // The CLI's own folder first (az and gcloud are scripts that look for their Python there).
    let mut path_var: Vec<PathBuf> = path.parent().map(Path::to_path_buf).into_iter().collect();
    path_var.extend(dirs.iter().cloned());
    let mut cmd = tokio::process::Command::new(&path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("PATH", std::env::join_paths(path_var).unwrap_or_default())
        .env("AZURE_CORE_NO_COLOR", "1")
        .env("AZURE_CORE_ONLY_SHOW_ERRORS", "1")
        .env("AWS_PAGER", "")
        .env("AWS_CLI_AUTO_PROMPT", "off")
        .env("CLOUDSDK_CORE_DISABLE_PROMPTS", "1");
    let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(SignInError::new(format!("{} did not start", cli.name()), e.to_string())),
        Err(_) => return Err(SignInError::new(format!("{} did not answer in {} s", cli.name(), TIMEOUT.as_secs()), format!("Run `{}` in a terminal to see what it waits for.", login_command(cli, auth)))),
    };
    if !out.status.success() {
        return Err(classify(cli, &String::from_utf8_lossy(&out.stderr), auth, args));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The command that signs the user in again, with the connection's tenant, profile or account.
pub fn login_command(cli: Cli, auth: &Auth) -> String {
    let given = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    match (cli, auth) {
        (Cli::Az, Auth::EntraId { tenant }) => match given(tenant) {
            Some(t) => format!("az login --tenant {t}"),
            None => "az login".into(),
        },
        (Cli::Aws, Auth::AwsIam { profile, .. }) => match given(profile) {
            Some(p) => format!("aws sso login --profile {p}"),
            None => "aws sso login".into(),
        },
        (Cli::Gcloud, Auth::GoogleIam { account }) => match given(account) {
            Some(a) => format!("gcloud auth login {a}"),
            None => "gcloud auth login".into(),
        },
        (cli, _) => format!("{} login", cli.program()),
    }
}

/// The CLI's complaint, turned into what to do. `args`: what it was asked.
fn classify(cli: Cli, stderr: &str, auth: &Auth, args: &[String]) -> SignInError {
    let low = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| low.contains(&n.to_ascii_lowercase()));
    let first = stderr.lines().map(str::trim).map(|l| l.trim_start_matches("ERROR:").trim_start_matches("ERROR").trim_start_matches(':').trim()).find(|l| !l.is_empty()).unwrap_or("").to_string();
    let first: String = first.chars().take(300).collect();
    let login = login_command(cli, auth);
    let signed_out = |detail: &str| SignInError::new(format!("Not signed in to {}", cli.cloud()), format!("Run `{login}` in a terminal, then try again.{detail}"));
    match cli {
        Cli::Az => {
            if has(&["AADSTS50076", "AADSTS50079", "AADSTS50078", "AADSTS53003", "AADSTS530003", "claims challenge", "interaction_required"]) {
                let scope = match args.iter().find_map(|a| a.strip_prefix("--resource=")) {
                    Some(r) => format!("{r}.default"),
                    None => "https://ossrdbms-aad.database.windows.net/.default".into(),
                };
                return SignInError::new("Azure asks to sign in again (MFA or Conditional Access)", format!("Run `{login} --scope {scope}` in a terminal, then try again."));
            }
            if has(&["az login", "AADSTS70043", "AADSTS700082", "AADSTS50173", "AADSTS50133", "AADSTS700024", "refresh token has expired", "No subscription found", "Please run 'az login'"]) {
                return signed_out("");
            }
            if has(&["AADSTS90002", "AADSTS900023"]) {
                return SignInError::new("Azure does not know this tenant", format!("Check the tenant in the connection's settings ({first})."));
            }
            if has(&["AADSTS65001", "AADSTS500011", "AADSTS650057", "consent"]) {
                return SignInError::new(
                    "Your Entra tenant gave no token for the database",
                    format!("An administrator may need to allow Azure CLI to sign in to Azure databases, or Microsoft Entra authentication is off for this server ({first})."),
                );
            }
        }
        Cli::Aws => {
            if has(&["could not be found"]) && has(&["profile"]) {
                let p = match auth {
                    Auth::AwsIam { profile: Some(p), .. } => p.trim().to_string(),
                    _ => "default".into(),
                };
                return SignInError::new(format!("No AWS profile {p:?}"), format!("Create it with `aws configure sso --profile {p}` (or fix the profile in the connection's settings)."));
            }
            if has(&["Unable to locate credentials"]) {
                return SignInError::new("Not signed in to AWS", format!("Run `{login}` (IAM Identity Center) or `aws configure` (access keys) in a terminal, then try again."));
            }
            if has(&["token has expired", "sso session", "sso token", "UnauthorizedSSOTokenError", "ExpiredToken", "refresh failed", "Error when retrieving token from sso", "reauthenticate"]) {
                return signed_out("");
            }
            if has(&["invalid choice"]) {
                return SignInError::new("This AWS CLI cannot make database tokens", format!("Update it ({}).", cli.install_url()));
            }
            if has(&["region"]) {
                return SignInError::new("No AWS region", format!("Set the region in the connection's settings ({first})."));
            }
        }
        Cli::Gcloud => {
            if has(&["invalid choice", "generate-login-token"]) && has(&["invalid choice"]) {
                return SignInError::new("This Google Cloud CLI cannot make database tokens", "Update it: `gcloud components update`.");
            }
            if has(&["You do not currently have an active account", "gcloud auth login", "Reauthentication", "invalid_grant", "problem refreshing your current auth tokens", "credentialed account"]) {
                return signed_out("");
            }
            if has(&["PERMISSION_DENIED", "permission"]) {
                return SignInError::new("Google Cloud refused a database token", format!("Check that the account may sign in to Cloud SQL ({first})."));
            }
        }
    }
    SignInError::new(format!("{} could not make a token", cli.name()), if first.is_empty() { format!("Run `{login}` in a terminal, then try again.") } else { first })
}

// ---- Parsing ---------------------------------------------------------------------------------

fn from_epoch(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}

fn bad_output(cli: Cli) -> SignInError {
    SignInError::new(format!("{} answered something that is not a token", cli.name()), format!("Update it ({}).", cli.install_url()))
}

/// A token is one word of printable ASCII.
fn plain_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic())
}

/// `az account get-access-token --output json`: `accessToken`, and `expires_on` (epoch seconds,
/// az 2.54+) or `expiresOn` (local time) before it.
fn parse_az(out: &str, now: SystemTime) -> Result<Token, SignInError> {
    let v: serde_json::Value = serde_json::from_str(out.trim()).map_err(|_| bad_output(Cli::Az))?;
    let secret = v.get("accessToken").and_then(|t| t.as_str()).filter(|t| plain_token(t)).ok_or_else(|| bad_output(Cli::Az))?.to_string();
    let epoch = match v.get("expires_on") {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    let local = || {
        use chrono::TimeZone as _;
        let s = v.get("expiresOn")?.as_str()?;
        let t = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S%.f").ok()?;
        Some(chrono::Local.from_local_datetime(&t).earliest()?.timestamp())
    };
    let expires = epoch.or_else(local).map(from_epoch).unwrap_or(now + UNTOLD);
    Ok(Token { secret, expires })
}

/// `aws rds generate-db-auth-token`: a presigned URL without its scheme, valid from
/// `X-Amz-Date` for `X-Amz-Expires` seconds.
fn parse_aws(out: &str, now: SystemTime) -> Result<Token, SignInError> {
    let secret = out.trim();
    if !plain_token(secret) || !secret.contains("Action=connect") || !secret.contains("X-Amz-Signature=") {
        return Err(bad_output(Cli::Aws));
    }
    let query = |k: &str| secret.split(['?', '&']).find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='));
    let signed = query("X-Amz-Date").and_then(|d| chrono::NaiveDateTime::parse_from_str(d, "%Y%m%dT%H%M%SZ").ok()).map(|t| from_epoch(t.and_utc().timestamp()));
    let lasts = query("X-Amz-Expires").and_then(|s| s.parse::<u64>().ok()).map(Duration::from_secs).unwrap_or(AWS_LIFETIME);
    let expires = signed.map(|t| t + lasts).unwrap_or(now + AWS_LIFETIME);
    Ok(Token { secret: secret.to_string(), expires })
}

/// `gcloud sql generate-login-token`: the token alone; or, under a `core/format` the user set,
/// `token: …` or JSON with a `token`.
fn parse_gcloud(out: &str, now: SystemTime) -> Result<Token, SignInError> {
    let text = out.trim();
    let secret = if text.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(text).ok().and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_string))
    } else if let Some(t) = text.strip_prefix("token:") {
        Some(t.trim().trim_matches(['\'', '"']).to_string())
    } else {
        Some(text.to_string())
    };
    let secret = secret.filter(|s| plain_token(s)).ok_or_else(|| bad_output(Cli::Gcloud))?;
    Ok(Token { secret, expires: now + UNTOLD })
}

fn parse(cli: Cli, out: &str, now: SystemTime) -> Result<Token, SignInError> {
    match cli {
        Cli::Az => parse_az(out, now),
        Cli::Aws => parse_aws(out, now),
        Cli::Gcloud => parse_gcloud(out, now),
    }
}

/// The signed-in user an Entra token names (its payload is plain base64url JSON).
fn jwt_identity(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let raw = data_encoding::BASE64URL_NOPAD.decode(payload.trim_end_matches('=').as_bytes()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let claim = |k: &str| v.get(k).and_then(|c| c.as_str()).map(str::to_string).filter(|s| !s.is_empty());
    claim("upn").or_else(|| claim("unique_name")).or_else(|| claim("preferred_username")).or_else(|| claim("email")).or_else(|| claim("appid").map(|a| format!("application {a}")))
}

/// `aws sts get-caller-identity --output json`: the ARN.
fn parse_sts(out: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    v.get("Arn").and_then(|a| a.as_str()).map(str::to_string)
}

// ---- The source ------------------------------------------------------------------------------

type Slot = Arc<tokio::sync::Mutex<Option<Token>>>;

/// Tokens by what was asked: one CLI run at a time per request, the others wait for its token.
#[derive(Default)]
pub struct Cache {
    slots: Mutex<HashMap<Request, Slot>>,
}

impl Cache {
    fn slot(&self, r: &Request) -> Slot {
        self.slots.lock().entry(r.clone()).or_default().clone()
    }
}

static SHARED: LazyLock<Arc<Cache>> = LazyLock::new(Default::default);

/// Mints tokens for connections: the process' one cache, the CLIs where they are installed.
#[derive(Clone)]
pub struct TokenSource {
    dirs: Arc<Vec<PathBuf>>,
    cache: Arc<Cache>,
}

impl Default for TokenSource {
    fn default() -> Self {
        TokenSource { dirs: Arc::new(default_dirs()), cache: SHARED.clone() }
    }
}

impl TokenSource {
    /// CLIs looked for in `dirs` only, with a cache of its own (tests).
    pub fn in_dirs(dirs: Vec<PathBuf>) -> TokenSource {
        TokenSource { dirs: Arc::new(dirs), cache: Default::default() }
    }

    /// A token for a new connection of `p`: the cached one while it lasts, else the CLI's.
    pub async fn token(&self, p: &ConnectionProfile) -> Result<String, SignInError> {
        guard(p)?;
        let r = request(p)?;
        self.fetch(&r, &p.auth, false).await
    }

    async fn fetch(&self, r: &Request, auth: &Auth, force: bool) -> Result<String, SignInError> {
        let slot = self.cache.slot(r);
        let mut held = slot.lock().await;
        if !force
            && let Some(t) = held.as_ref().filter(|t| t.usable(SystemTime::now()))
        {
            return Ok(t.secret.clone());
        }
        *held = None;
        let out = run(r.cli, &r.args, &self.dirs, auth).await?;
        let t = parse(r.cli, &out, SystemTime::now())?;
        let secret = t.secret.clone();
        if t.usable(SystemTime::now()) {
            *held = Some(t);
        }
        Ok(secret)
    }

    /// The connection dialog's check: a fresh token (the CLI run now) and who it signs in as.
    pub async fn check(&self, p: &ConnectionProfile) -> Result<SignedIn, SignInError> {
        guard(p)?;
        let r = request(p)?;
        let secret = self.fetch(&r, &p.auth, true).await?;
        let valid_for = {
            let slot = self.cache.slot(&r);
            let held = slot.lock().await;
            held.as_ref().and_then(|t| t.expires.duration_since(SystemTime::now()).ok()).unwrap_or_default()
        };
        let who = match &p.auth {
            Auth::EntraId { .. } => jwt_identity(&secret),
            Auth::AwsIam { profile, .. } => {
                let mut args: Vec<String> = ["sts", "get-caller-identity", "--output=json"].map(String::from).to_vec();
                args.extend(opt("profile", "profile", profile.as_deref())?);
                run(Cli::Aws, &args, &self.dirs, &p.auth).await.ok().and_then(|o| parse_sts(&o))
            }
            Auth::GoogleIam { account: Some(a) } if !a.trim().is_empty() => Some(a.trim().to_string()),
            Auth::GoogleIam { .. } => {
                let args: Vec<String> = ["config", "get-value", "account", "--quiet"].map(String::from).to_vec();
                run(Cli::Gcloud, &args, &self.dirs, &p.auth).await.ok().map(|o| o.trim().to_string()).filter(|o| plain_token(o))
            }
            Auth::Password => None,
        };
        Ok(SignedIn { who, valid_for })
    }
}

// ---- Refusals by the server ------------------------------------------------------------------

/// What the user's side most likely misses when the server refused a token, put on top of the
/// server's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hint(pub &'static str);

impl std::fmt::Display for Hint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

pub fn hint_of(e: &anyhow::Error) -> Option<&'static str> {
    e.downcast_ref::<Hint>().map(|h| h.0)
}

fn refusal(p: &ConnectionProfile) -> Option<&'static str> {
    Some(match (p.auth.kind(), p.engine) {
        (AuthKind::EntraId, Engine::Postgres) => "The server refused the Entra token: the user must be the exact name (case-sensitive) of a principal created with pgaadauth_create_principal, and the server must have Microsoft Entra authentication on.",
        (AuthKind::EntraId, Engine::MySql) => "The server refused the Entra token: the user must be a principal created with CREATE AADUSER, and the server must have Microsoft Entra authentication on.",
        (AuthKind::EntraId, _) => "The server refused the Entra token: the database needs a user created with CREATE USER [name] FROM EXTERNAL PROVIDER, and the server Microsoft Entra authentication on.",
        (AuthKind::AwsIam, Engine::MySql) => "The server refused the IAM token: IAM database authentication must be on for the instance, the user created WITH AWSAuthenticationPlugin AS 'RDS', and your IAM policy must allow rds-db:connect for this user.",
        (AuthKind::AwsIam, _) => "The server refused the IAM token: IAM database authentication must be on for the instance, the user granted rds_iam, and your IAM policy must allow rds-db:connect for this user.",
        (AuthKind::GoogleIam, Engine::MySql) => "The server refused the Google token: the instance needs cloudsql_iam_authentication on and the IAM user added to it; the user is the e-mail's part before @.",
        (AuthKind::GoogleIam, _) => "The server refused the Google token: the instance needs cloudsql.iam_authentication on and the IAM user added to it; the user is the full e-mail (a service account's without .gserviceaccount.com).",
        (AuthKind::Password, _) => return None,
    })
}

/// A login refused for a cloud token, with the likely reason on top; any other error unchanged.
pub(crate) fn explain(p: &ConnectionProfile, e: anyhow::Error) -> anyhow::Error {
    let Some(hint) = refusal(p) else { return e };
    if e.chain().any(|c| c.downcast_ref::<SignInError>().is_some()) {
        return e;
    }
    let code = crate::backend::server_error(&e).and_then(|i| i.code).unwrap_or_default();
    let msg = format!("{e:#}").to_ascii_lowercase();
    let refused = ["28P01", "28000", "1045", "18456"].contains(&code.as_str())
        || ["password authentication failed", "pam authentication failed", "access denied", "login failed", "token-identified principal"].iter().any(|w| msg.contains(w));
    if refused { e.context(Hint(hint)) } else { e }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn profile(engine: Engine, host: &str, auth: Auth) -> ConnectionProfile {
        let mut p = ConnectionProfile::new("cloud");
        (p.engine, p.host, p.port, p.user, p.auth) = (engine, host.into(), engine.default_port(), "ana@contoso.com".into(), auth);
        p
    }

    /// A JWT with `claims` as its payload (unsigned: only the payload is read).
    fn jwt(claims: &str) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.c2ln", data_encoding::BASE64URL_NOPAD.encode(claims.as_bytes()))
    }

    #[test]
    fn az_output_parses_with_either_expiry() {
        let token = jwt(r#"{"upn":"ana@contoso.com","tid":"t"}"#);
        let new = format!(r#"{{"accessToken":"{token}","expiresOn":"2026-10-28 13:00:00.000000","expires_on":1793192400,"subscription":"s","tenant":"t","tokenType":"Bearer"}}"#);
        let t = parse_az(&new, at(0)).unwrap();
        assert_eq!((t.secret.as_str(), t.expires), (token.as_str(), at(1793192400)));
        let old = format!(r#"{{"accessToken":"{token}","expiresOn":"2026-10-28 13:00:00.000000","tokenType":"Bearer"}}"#);
        let local = chrono::NaiveDateTime::parse_from_str("2026-10-28 13:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let want = { use chrono::TimeZone as _; chrono::Local.from_local_datetime(&local).earliest().unwrap().timestamp() };
        assert_eq!(parse_az(&old, at(0)).unwrap().expires, at(want as u64));
        let string_epoch = format!(r#"{{"accessToken":"{token}","expires_on":"1793192400"}}"#);
        assert_eq!(parse_az(&string_epoch, at(0)).unwrap().expires, at(1793192400));
        assert_eq!(parse_az(&format!(r#"{{"accessToken":"{token}"}}"#), at(100)).unwrap().expires, at(100) + UNTOLD);
        for bad in ["", "Please run 'az login'", r#"{"accessToken":""}"#, r#"{"accessToken":"a b"}"#, r#"{"token":"x"}"#] {
            assert!(parse_az(bad, at(0)).is_err(), "{bad}");
        }
        assert_eq!(jwt_identity(&token).as_deref(), Some("ana@contoso.com"));
        assert_eq!(jwt_identity(&jwt(r#"{"appid":"1234"}"#)).as_deref(), Some("application 1234"));
        assert_eq!(jwt_identity("opaque"), None);
    }

    #[test]
    fn aws_tokens_expire_when_their_signature_says() {
        let token = "db.abc123.eu-west-1.rds.amazonaws.com:5432/?Action=connect&DBUser=ana&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ASIAX%2F20261028%2Feu-west-1%2Frds-db%2Faws4_request&X-Amz-Date=20261028T120000Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Security-Token=IQo&X-Amz-Signature=0123abcd";
        let t = parse_aws(&format!("{token}\n"), at(0)).unwrap();
        let signed = chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp() as u64;
        assert_eq!((t.secret.as_str(), t.expires), (token, at(signed + 900)));
        let undated = "h:5432/?Action=connect&DBUser=ana&X-Amz-Signature=00";
        assert_eq!(parse_aws(undated, at(50)).unwrap().expires, at(50) + AWS_LIFETIME);
        for bad in ["", "Unable to locate credentials", "h:5432/?Action=connect", "a b?Action=connect&X-Amz-Signature=1"] {
            assert!(parse_aws(bad, at(0)).is_err(), "{bad}");
        }
        let sts = r#"{"UserId":"AROA:ana","Account":"123456789012","Arn":"arn:aws:sts::123456789012:assumed-role/AWSReservedSSO_Dev_1/ana@example.com"}"#;
        assert_eq!(parse_sts(sts).as_deref(), Some("arn:aws:sts::123456789012:assumed-role/AWSReservedSSO_Dev_1/ana@example.com"));
    }

    #[test]
    fn gcloud_tokens_in_every_format() {
        for out in ["ya29.a0AfB_byC\n", "token: ya29.a0AfB_byC\n", r#"{"token": "ya29.a0AfB_byC"}"#] {
            let t = parse_gcloud(out, at(10)).unwrap();
            assert_eq!((t.secret.as_str(), t.expires), ("ya29.a0AfB_byC", at(10) + UNTOLD), "{out}");
        }
        for bad in ["", "two words", "{}"] {
            assert!(parse_gcloud(bad, at(0)).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_are_reused_until_minutes_before_they_expire() {
        let now = at(10_000);
        let t = |left: u64| Token { secret: "x".into(), expires: now + Duration::from_secs(left) };
        assert!(t(3600).usable(now));
        assert!(t(MARGIN.as_secs() + 1).usable(now));
        assert!(!t(MARGIN.as_secs()).usable(now));
        assert!(!t(60).usable(now));
        assert!(!Token { secret: "x".into(), expires: at(1) }.usable(now), "expired");
    }

    #[test]
    fn requests_name_the_right_audience_and_only_plain_values() {
        let pg = profile(Engine::Postgres, "srv.postgres.database.azure.com", Auth::EntraId { tenant: Some("contoso.onmicrosoft.com".into()) });
        let r = request(&pg).unwrap();
        assert_eq!((r.cli, r.args.join(" ")), (Cli::Az, "account get-access-token --output=json --resource-type=oss-rdbms --tenant=contoso.onmicrosoft.com".to_string()));
        let ms = profile(Engine::SqlServer, "srv.database.usgovcloudapi.net", Auth::EntraId { tenant: None });
        assert!(request(&ms).unwrap().args.contains(&"--resource=https://database.usgovcloudapi.net/".to_string()));
        let mut rds = profile(Engine::MySql, "db.abc.eu-west-1.rds.amazonaws.com", Auth::AwsIam { profile: Some("dev".into()), region: None });
        rds.user = "app_iam".into();
        assert_eq!(
            request(&rds).unwrap().args.join(" "),
            "rds generate-db-auth-token --output=text --hostname=db.abc.eu-west-1.rds.amazonaws.com --port=3306 --username=app_iam --region=eu-west-1 --profile=dev"
        );
        rds.host = "db.internal".into();
        assert_eq!(request(&rds).unwrap_err().title, "No AWS region");
        rds.auth = Auth::AwsIam { profile: Some("--endpoint-url=http://evil".into()), region: Some("eu-west-1".into()) };
        assert!(request(&rds).is_err(), "a flag smuggled as a profile");
        rds.auth = Auth::AwsIam { profile: Some("dev; rm -rf ~".into()), region: Some("eu-west-1".into()) };
        assert!(request(&rds).is_err());
        let g = profile(Engine::Postgres, "10.0.0.3", Auth::GoogleIam { account: Some("ana@example.com".into()) });
        assert_eq!(request(&g).unwrap().args.join(" "), "sql generate-login-token --quiet --account=ana@example.com");
        assert_eq!(aws_region_of("x.cluster-ro-abc.us-gov-west-1.rds.amazonaws.com").as_deref(), Some("us-gov-west-1"));
        assert_eq!(aws_region_of("proxy.proxy-abc.ap-southeast-2.rds.amazonaws.com").as_deref(), Some("ap-southeast-2"));
        assert_eq!(aws_region_of("rds.example.com"), None);
    }

    #[test]
    fn hosts_of_each_cloud() {
        for h in ["srv.database.windows.net", "srv.postgres.database.azure.com", "SRV.MySQL.Database.Azure.com.", "mi.abc123.database.windows.net", "srv.privatelink.database.windows.net", "srv.postgres.database.chinacloudapi.cn"] {
            assert!(host_allowed(AuthKind::EntraId, h), "{h}");
        }
        for h in ["evil.example", "database.windows.net", "srv.database.windows.net.evil.example", "evil.example/.database.windows.net", "a@srv.database.windows.net", "10.0.0.1", "srv..database.windows.net"] {
            assert!(!host_allowed(AuthKind::EntraId, h), "{h}");
        }
        assert!(host_allowed(AuthKind::AwsIam, "db.abc.us-east-1.rds.amazonaws.com"));
        assert!(!host_allowed(AuthKind::AwsIam, "db.abc.us-east-1.rds.amazonaws.com.evil.example"));
        assert!(host_allowed(AuthKind::GoogleIam, "abc.us-central1.sql.goog"));
        assert!(!host_allowed(AuthKind::GoogleIam, "34.1.2.3"), "a plain IP is nobody's in particular");
    }

    #[test]
    fn tokens_go_only_where_the_user_chose() {
        let mut p = profile(Engine::Postgres, "evil.example", Auth::EntraId { tenant: None });
        assert!(guard(&p).is_ok(), "set up by the user in Dataman");
        p.from_project = true;
        assert!(guard(&p).unwrap_err().body.contains("open it in Edit Connection"));
        p.host = "srv.postgres.database.azure.com".into();
        assert!(guard(&p).is_ok());
        p.ssh = Some(Default::default());
        assert!(guard(&p).is_err(), "through the project's tunnel");
        let mut g = profile(Engine::MySql, "34.1.2.3", Auth::GoogleIam { account: None });
        assert!(guard(&g).is_ok());
        g.agent_made = true;
        assert!(guard(&g).is_err());
        let mut aws = profile(Engine::Postgres, "db.abc.us-east-1.rds.amazonaws.com", Auth::AwsIam { profile: None, region: None });
        (aws.agent_made, aws.ssh) = (true, Some(Default::default()));
        assert!(guard(&aws).is_ok(), "an RDS token is signed for its host");
        let mongo = profile(Engine::MongoDb, "h", Auth::EntraId { tenant: None });
        assert!(guard(&mongo).is_err(), "not for this engine yet");
    }

    #[test]
    fn failures_say_what_to_run() {
        let entra = Auth::EntraId { tenant: Some("contoso.onmicrosoft.com".into()) };
        let e = classify(Cli::Az, "ERROR: Please run 'az login' to setup account.\n", &entra, &[]);
        assert_eq!(e.title, "Not signed in to Azure");
        assert!(e.body.contains("`az login --tenant contoso.onmicrosoft.com`"), "{e}");
        let e = classify(Cli::Az, "ERROR: AADSTS50076: Due to a configuration change made by your administrator, you must use multi-factor authentication.", &Auth::EntraId { tenant: None }, &[]);
        assert!(e.body.contains("az login --scope https://ossrdbms-aad.database.windows.net/.default"), "{e}");
        let e = classify(Cli::Az, "ERROR: AADSTS65001: The user or administrator has not consented to use the application", &Auth::EntraId { tenant: None }, &[]);
        assert!(e.title.contains("no token"), "{e}");
        let aws = Auth::AwsIam { profile: Some("dev".into()), region: None };
        let e = classify(Cli::Aws, "\nUnable to locate credentials. You can configure credentials by running \"aws configure\".\n", &aws, &[]);
        assert!(e.body.contains("`aws sso login --profile dev`"), "{e}");
        let e = classify(Cli::Aws, "\nError when retrieving token from sso: Token has expired and refresh failed\n", &aws, &[]);
        assert_eq!((e.title.as_str(), e.body.contains("aws sso login --profile dev")), ("Not signed in to AWS", true));
        let e = classify(Cli::Aws, "\nThe config profile (dev) could not be found\n", &aws, &[]);
        assert!(e.body.contains("aws configure sso --profile dev"), "{e}");
        let g = Auth::GoogleIam { account: None };
        let e = classify(Cli::Gcloud, "ERROR: (gcloud.sql.generate-login-token) You do not currently have an active account selected.\nPlease run:\n\n  $ gcloud auth login\n", &g, &[]);
        assert!(e.body.contains("`gcloud auth login`"), "{e}");
        let e = classify(Cli::Gcloud, "ERROR: (gcloud.sql) Invalid choice: 'generate-login-token'.", &g, &[]);
        assert!(e.body.contains("gcloud components update"), "{e}");
        let e = classify(Cli::Gcloud, "ERROR: something odd happened\n", &g, &[]);
        assert_eq!(e.body, "something odd happened", "{e}");
    }

    /// A fake CLI: a shell script in `dir`.
    fn fake(dir: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let f = dir.join(name);
        std::fs::write(&f, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn runs(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("runs")).map(|s| s.lines().count()).unwrap_or(0)
    }

    #[tokio::test]
    async fn one_cli_run_while_the_token_lasts() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let far = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
        let token = jwt(r#"{"upn":"ana@contoso.com"}"#);
        fake(d, "az", &format!("echo \"$@\" >> '{}/runs'\necho '{{\"accessToken\":\"{token}\",\"expires_on\":{far}}}'", d.display()));
        let src = TokenSource::in_dirs(vec![d.to_path_buf()]);
        let p = profile(Engine::Postgres, "srv.postgres.database.azure.com", Auth::EntraId { tenant: None });
        let all = futures_util::future::join_all((0..5).map(|_| src.token(&p))).await;
        assert!(all.iter().all(|t| t.as_deref() == Ok(token.as_str())));
        assert_eq!(runs(d), 1, "five tabs at once, one az");
        assert_eq!(std::fs::read_to_string(d.join("runs")).unwrap().trim(), "account get-access-token --output=json --resource-type=oss-rdbms");
        let signed = src.check(&p).await.unwrap();
        assert_eq!(signed.who.as_deref(), Some("ana@contoso.com"));
        assert!(signed.valid_for > Duration::from_secs(3500), "{signed:?}");
        assert_eq!(runs(d), 2, "the check asks the CLI again");

        // A token about to expire is never reused: every dial asks again.
        let near = far - 3600 + 120;
        fake(d, "az", &format!("echo run >> '{}/runs'\necho '{{\"accessToken\":\"{token}\",\"expires_on\":{near}}}'", d.display()));
        let src = TokenSource::in_dirs(vec![d.to_path_buf()]);
        src.token(&p).await.unwrap();
        src.token(&p).await.unwrap();
        assert_eq!(runs(d), 4);
    }

    #[tokio::test]
    async fn a_missing_or_signed_out_cli_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let src = TokenSource::in_dirs(vec![d.to_path_buf()]);
        let p = profile(Engine::Postgres, "db.abc.eu-west-1.rds.amazonaws.com", Auth::AwsIam { profile: Some("dev".into()), region: None });
        let e = src.token(&p).await.unwrap_err();
        assert!(e.title.contains("AWS CLI (aws) not found") && e.body.contains("getting-started-install"), "{e}");
        fake(d, "aws", "echo 'Unable to locate credentials. You can configure credentials by running \"aws configure\".' >&2\nexit 255");
        let e = src.token(&p).await.unwrap_err();
        assert!(e.body.contains("aws sso login --profile dev"), "{e}");
        fake(d, "aws", "echo 'db.abc.eu-west-1.rds.amazonaws.com:5432/?Action=connect&DBUser=ana&X-Amz-Signature=ff'");
        assert!(src.token(&p).await.unwrap().ends_with("X-Amz-Signature=ff"));
    }

    #[test]
    fn a_refused_token_gets_a_hint() {
        let p = profile(Engine::Postgres, "h", Auth::AwsIam { profile: None, region: None });
        let e = explain(&p, anyhow::anyhow!("db error: FATAL: PAM authentication failed for user \"ana\""));
        assert!(hint_of(&e).is_some_and(|h| h.contains("rds_iam")), "{e:#}");
        assert!(hint_of(&explain(&p, anyhow::anyhow!("connection refused"))).is_none());
        let mut pw = p.clone();
        pw.auth = Auth::Password;
        assert!(hint_of(&explain(&pw, anyhow::anyhow!("password authentication failed"))).is_none());
    }
}
