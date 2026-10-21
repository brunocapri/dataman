//! What agents may see: sensitive data types recognized inside any value an agent receives,
//! per-connection rules (what the agent sees, what is visible to agents, sensitive columns,
//! what agents may do), and database errors cleaned of the values they quote.
//!
//! Everything here narrows what agents get; none of it touches the user's own tabs.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mask {
    #[default]
    Hidden,
    Partial,
    Hash,
}

impl Mask {
    pub const ALL: [Mask; 3] = [Mask::Hidden, Mask::Partial, Mask::Hash];

    pub fn label(self) -> &'static str {
        match self {
            Mask::Hidden => "Hidden",
            Mask::Partial => "Partial",
            Mask::Hash => "Hash",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Mask::Hidden => "hidden",
            Mask::Partial => "partial",
            Mask::Hash => "hash",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Validator {
    #[default]
    None,
    Luhn,
    Cpf,
    Cnpj,
    Iban,
    Entropy,
}

impl Validator {
    pub const ALL: [Validator; 6] = [Validator::None, Validator::Luhn, Validator::Cpf, Validator::Cnpj, Validator::Iban, Validator::Entropy];

    pub fn label(self) -> &'static str {
        match self {
            Validator::None => "None",
            Validator::Luhn => "Luhn",
            Validator::Cpf => "CPF",
            Validator::Cnpj => "CNPJ",
            Validator::Iban => "IBAN",
            Validator::Entropy => "Looks random",
        }
    }

    pub fn accepts(self, s: &str) -> bool {
        match self {
            Validator::None => true,
            Validator::Luhn => luhn(s),
            Validator::Cpf => cpf(s),
            Validator::Cnpj => cnpj(s),
            Validator::Iban => iban(s),
            Validator::Entropy => entropy(s) >= 3.5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Partial {
    /// Every letter and digit but the last `n` becomes •; separators stay.
    KeepLast(u8),
    /// `•••• 4242`.
    Card,
    /// `a•••@acme.com`.
    Email,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Norm {
    Digits,
    Alnum,
    Lower,
    Exact,
}

pub struct Builtin {
    pub id: &'static str,
    pub name: &'static str,
    /// How it is found (the `?` tooltip).
    pub rule: &'static str,
    pub example: &'static str,
    pub on: bool,
    pub mask: Mask,
    pattern: &'static str,
    validator: Validator,
    partial: Partial,
    norm: Norm,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "cpf",
        name: "CPF",
        rule: "11 digits with both check digits valid, not all the same digit.",
        example: "123.456.789-09 → •••.•••.•89-09",
        on: true,
        mask: Mask::Partial,
        pattern: r"\b\d{3}\.?\d{3}\.?\d{3}-?\d{2}\b",
        validator: Validator::Cpf,
        partial: Partial::KeepLast(4),
        norm: Norm::Digits,
    },
    Builtin {
        id: "cnpj",
        name: "CNPJ",
        rule: "14 positions with valid check digits, the alphanumeric CNPJ included.",
        example: "12.ABC.345/01DE-35 → ••.•••.•••/••DE-35",
        on: true,
        mask: Mask::Partial,
        pattern: r"\b[0-9A-Z]{2}\.?[0-9A-Z]{3}\.?[0-9A-Z]{3}/?[0-9A-Z]{4}-?\d{2}\b",
        validator: Validator::Cnpj,
        partial: Partial::KeepLast(4),
        norm: Norm::Alnum,
    },
    Builtin {
        id: "card",
        name: "Card number",
        rule: "13–19 digits passing Luhn, with a known issuer prefix.",
        example: "4242 4242 4242 4242 → •••• 4242",
        on: true,
        mask: Mask::Partial,
        pattern: r"\b(?:\d[ -]?){12,18}\d\b",
        validator: Validator::Luhn,
        partial: Partial::Card,
        norm: Norm::Digits,
    },
    Builtin {
        id: "email",
        name: "E-mail",
        rule: "A practical RFC 5322 address.",
        example: "ana@acme.com → a•••@acme.com",
        on: true,
        mask: Mask::Partial,
        pattern: r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}\b",
        validator: Validator::None,
        partial: Partial::Email,
        norm: Norm::Lower,
    },
    Builtin {
        id: "phone",
        name: "Phone",
        rule: "Brazilian mobile and landline numbers, with or without +55.",
        example: "(81) 98765-4321 → (••) •••••-4321",
        on: true,
        mask: Mask::Partial,
        pattern: r"(?:\+55[ ]?)?(?:\(\d{2}\)[ ]?|\b\d{2}[ ])9?\d{4}[- ]\d{4}\b",
        validator: Validator::None,
        partial: Partial::KeepLast(4),
        norm: Norm::Digits,
    },
    Builtin {
        id: "jwt",
        name: "JWT",
        rule: "eyJ….eyJ…. whose header decodes to JSON with alg.",
        example: "eyJhbGciOiJIUzI1NiIs… → ‹hidden›",
        on: true,
        mask: Mask::Hidden,
        pattern: r"\beyJ[A-Za-z0-9_\-]{5,}\.eyJ[A-Za-z0-9_\-]{5,}\.[A-Za-z0-9_\-]*",
        validator: Validator::None,
        partial: Partial::KeepLast(4),
        norm: Norm::Exact,
    },
    Builtin {
        id: "api-key",
        name: "API keys",
        rule: "Known prefixes: AKIA, ghp_, xoxb-, sk_live_, sk-ant-, AIza, glpat-…",
        example: "sk_live_51Hx… → ‹hidden›",
        on: true,
        mask: Mask::Hidden,
        pattern: r"\b(?:AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{30,}|xox[abposr]-[A-Za-z0-9\-]{10,}|[sr]k_(?:live|test)_[A-Za-z0-9]{10,}|sk-ant-[A-Za-z0-9_\-]{10,}|sk-(?:proj-)?[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_\-]{35}|glpat-[A-Za-z0-9_\-]{20})",
        validator: Validator::None,
        partial: Partial::KeepLast(4),
        norm: Norm::Exact,
    },
    Builtin {
        id: "private-key",
        name: "Private keys",
        rule: "From -----BEGIN … PRIVATE KEY----- to its END line.",
        example: "the whole block → ‹hidden›",
        on: true,
        mask: Mask::Hidden,
        pattern: r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
        validator: Validator::None,
        partial: Partial::KeepLast(0),
        norm: Norm::Exact,
    },
    Builtin {
        id: "password-hash",
        name: "Password hash",
        rule: "bcrypt, argon2, scrypt, PBKDF2 and crypt(3) hashes ($2b$…, $argon2id$…).",
        example: "$2b$12$Kix… → ‹hidden›",
        on: true,
        mask: Mask::Hidden,
        pattern: r"\$(?:2[abxy]|argon2(?:id|i|d)|scrypt|pbkdf2[-_a-z0-9]*|[156])\$[^\s'\x22]{8,}",
        validator: Validator::None,
        partial: Partial::KeepLast(0),
        norm: Norm::Exact,
    },
    Builtin {
        id: "ip",
        name: "IP address",
        rule: "IPv4 and IPv6.",
        example: "10.0.3.17 → ‹h:Q4M8…›",
        on: false,
        mask: Mask::Hash,
        pattern: r"\b(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\b|\b(?:[0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}\b|\b(?:[0-9A-Fa-f]{1,4}:){1,6}:(?:[0-9A-Fa-f]{1,4}:){0,5}[0-9A-Fa-f]{1,4}\b",
        validator: Validator::None,
        partial: Partial::KeepLast(4),
        norm: Norm::Lower,
    },
];

pub fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.id == id)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
}

fn four() -> u8 {
    4
}

fn is_four(n: &u8) -> bool {
    *n == 4
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomType {
    pub id: String,
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub mask: Mask,
    /// Partial only: the characters left at the end.
    #[serde(default = "four", skip_serializing_if = "is_four")]
    pub keep: u8,
    #[serde(default, skip_serializing_if = "is_none_validator")]
    pub validator: Validator,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_case: bool,
    #[serde(default = "yes")]
    pub on: bool,
}

fn is_none_validator(v: &Validator) -> bool {
    *v == Validator::None
}

impl CustomType {
    pub fn new(name: &str, pattern: &str, mask: Mask) -> CustomType {
        let id = format!("t_{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        CustomType { id, name: name.trim().to_string(), pattern: pattern.to_string(), mask, keep: 4, validator: Validator::None, ignore_case: false, on: true }
    }
}

/// The sensitive data types, for every connection: only the differences from the defaults are
/// written (`[agent_types]` in settings.toml).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtin: BTreeMap<String, BuiltinOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomType>,
}

pub const MAX_CUSTOM: usize = 50;
pub const MAX_PATTERN: usize = 500;

/// One row of the Sensitive data list, as it applies (global, or with a connection's switches).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRow {
    pub id: String,
    pub name: String,
    pub rule: String,
    pub example: String,
    pub on: bool,
    pub mask: Mask,
    pub custom: bool,
    /// The connection's switch differs from the global one.
    pub changed_here: bool,
    /// The pattern does not compile: reads are refused where it is on.
    pub broken: Option<String>,
}

impl TypeSettings {
    pub fn is_unchanged(&self) -> bool {
        *self == TypeSettings::default()
    }

    pub fn builtin_on(&self, b: &Builtin) -> bool {
        self.builtin.get(b.id).and_then(|o| o.on).unwrap_or(b.on)
    }

    pub fn builtin_mask(&self, b: &Builtin) -> Mask {
        self.builtin.get(b.id).and_then(|o| o.mask).unwrap_or(b.mask)
    }

    pub fn set_builtin(&mut self, id: &str, on: Option<bool>, mask: Option<Mask>) {
        let Some(b) = builtin(id) else { return };
        let mut o = self.builtin.remove(id).unwrap_or_default();
        if let Some(on) = on {
            o.on = (on != b.on).then_some(on);
        }
        if let Some(m) = mask {
            o.mask = (m != b.mask).then_some(m);
        }
        if o != BuiltinOverride::default() {
            self.builtin.insert(id.to_string(), o);
        }
    }

    pub fn global_on(&self, id: &str) -> Option<bool> {
        if let Some(b) = builtin(id) {
            return Some(self.builtin_on(b));
        }
        self.custom.iter().find(|c| c.id == id).map(|c| c.on)
    }

    /// The list as it applies: `overrides` are a connection's switches (empty for the global list).
    pub fn rows(&self, overrides: &BTreeMap<String, bool>) -> Vec<TypeRow> {
        let mut rows: Vec<TypeRow> = BUILTINS
            .iter()
            .map(|b| {
                let global = self.builtin_on(b);
                let on = overrides.get(b.id).copied().unwrap_or(global);
                TypeRow { id: b.id.into(), name: b.name.into(), rule: b.rule.into(), example: b.example.into(), on, mask: self.builtin_mask(b), custom: false, changed_here: on != global, broken: None }
            })
            .collect();
        for c in &self.custom {
            let on = overrides.get(&c.id).copied().unwrap_or(c.on);
            let mut rule = format!("Custom: {}", c.pattern);
            if c.validator != Validator::None {
                rule.push_str(&format!(" · {}", c.validator.label()));
            }
            let broken = check_pattern(&c.pattern).err();
            rows.push(TypeRow { id: c.id.clone(), name: c.name.clone(), rule, example: String::new(), on, mask: c.mask, custom: true, changed_here: on != c.on, broken });
        }
        rows
    }
}

/// Why a custom pattern cannot be used, in the editor's words.
pub fn check_pattern(p: &str) -> Result<(), String> {
    if p.chars().count() > MAX_PATTERN {
        return Err("Pattern is too large.".into());
    }
    if p.trim().is_empty() {
        return Err("This pattern matches empty text, so it would hide everything.".into());
    }
    if ["(?=", "(?!", "(?<=", "(?<!"].iter().any(|l| p.contains(l)) {
        return Err("Look-around isn't supported.".into());
    }
    let b = p.as_bytes();
    for i in 0..b.len().saturating_sub(1) {
        let escaped = i > 0 && b[i - 1] == b'\\' && (i < 2 || b[i - 2] != b'\\');
        if b[i] == b'\\' && !escaped && (b[i + 1].is_ascii_digit() && b[i + 1] != b'0' || b[i + 1] == b'k') {
            return Err("Back-references aren't supported.".into());
        }
    }
    let re = compile(p, false).map_err(|e| match e {
        regex::Error::CompiledTooBig(_) => "Pattern is too large.".to_string(),
        e => {
            let s = e.to_string();
            s.lines().last().unwrap_or("Invalid pattern.").trim().trim_start_matches("error: ").to_string()
        }
    })?;
    if re.is_match("") {
        return Err("This pattern matches empty text, so it would hide everything.".into());
    }
    Ok(())
}

fn compile(p: &str, ignore_case: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(p).case_insensitive(ignore_case).size_limit(256 * 1024).dfa_size_limit(1024 * 1024).build()
}

/// The editor's test: each line with its matches (byte ranges) and what the agent would get.
pub struct Tested {
    pub lines: Vec<(String, Vec<std::ops::Range<usize>>, Option<String>)>,
    pub matches: usize,
    /// Matches most lines: maybe too broad.
    pub broad: bool,
}

pub fn test_pattern(t: &CustomType, lines: &[&str], key: &HashKey) -> Result<Tested, String> {
    check_pattern(&t.pattern)?;
    let kind = Kind::custom(t).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut matches = 0;
    let mut hit_lines = 0;
    for l in lines.iter().filter(|l| !l.trim().is_empty()) {
        let spans = kind.spans(l);
        if !spans.is_empty() {
            hit_lines += 1;
        }
        matches += spans.len();
        let masked = (!spans.is_empty()).then(|| {
            let mut s = String::new();
            let mut at = 0;
            for r in &spans {
                s.push_str(&l[at..r.start]);
                s.push_str(&kind.mask_text(&l[r.clone()], key));
                at = r.end;
            }
            s.push_str(&l[at..]);
            s
        });
        out.push((l.to_string(), spans, masked));
    }
    let n = out.len();
    Ok(Tested { lines: out, matches, broad: n >= 2 && hit_lines * 2 > n })
}

/// The key of Hash masks (HMAC-SHA256), fetched once and only when a Hash mask is needed.
#[derive(Clone)]
pub struct HashKey {
    key: Arc<OnceLock<ring::hmac::Key>>,
    load: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl HashKey {
    pub fn new(load: impl Fn() -> Vec<u8> + Send + Sync + 'static) -> HashKey {
        HashKey { key: Arc::new(OnceLock::new()), load: Arc::new(load) }
    }

    /// Kept in the Keychain (or the platform's keyring) as `agents-mask-key`, made on first use.
    pub fn stored(secrets: Arc<dyn crate::SecretStore>) -> HashKey {
        HashKey::new(move || {
            const ACCOUNT: &str = "agents-mask-key";
            if let Ok(Some(k)) = secrets.get(ACCOUNT)
                && let Ok(b) = data_encoding::BASE64.decode(k.as_bytes())
                && b.len() == 32
            {
                return b;
            }
            let mut b = [0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random");
            if let Err(e) = secrets.set(ACCOUNT, &data_encoding::BASE64.encode(&b)) {
                log::warn!("mask key not saved: {e:#}");
            }
            b.to_vec()
        })
    }

    pub fn fixed(bytes: &[u8]) -> HashKey {
        let b = bytes.to_vec();
        HashKey::new(move || b.clone())
    }

    pub fn hash(&self, normalized: &str) -> String {
        let key = self.key.get_or_init(|| ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &(self.load)()));
        let tag = ring::hmac::sign(key, normalized.as_bytes());
        let b32 = data_encoding::BASE32_NOPAD.encode(tag.as_ref());
        format!("‹h:{}›", &b32[..10])
    }
}

pub const HIDDEN: &str = "‹hidden›";

struct Kind {
    id: String,
    name: String,
    re: Regex,
    group_v: bool,
    validator: Validator,
    mask: Mask,
    partial: Partial,
    norm: Norm,
    token: String,
}

impl Kind {
    fn builtin(b: &Builtin, mask: Mask) -> Kind {
        Kind {
            id: b.id.into(),
            name: b.name.into(),
            re: compile(b.pattern, false).expect("built-in pattern"),
            group_v: false,
            validator: b.validator,
            mask,
            partial: b.partial,
            norm: b.norm,
            token: HIDDEN.into(),
        }
    }

    fn custom(c: &CustomType) -> Result<Kind, regex::Error> {
        let re = compile(&c.pattern, c.ignore_case)?;
        let group_v = re.capture_names().any(|n| n == Some("v"));
        Ok(Kind {
            id: c.id.clone(),
            name: c.name.clone(),
            re,
            group_v,
            validator: c.validator,
            mask: c.mask,
            partial: Partial::KeepLast(c.keep.min(6)),
            norm: Norm::Exact,
            token: format!("‹{}›", c.name.trim().to_lowercase()),
        })
    }

    fn spans(&self, s: &str) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        for c in self.re.captures_iter(s) {
            let m = if self.group_v { c.name("v") } else { c.get(0) };
            let Some(m) = m.filter(|m| !m.is_empty()) else { continue };
            if self.validator.accepts(m.as_str()) && (self.id != "card" || card_issuer(m.as_str())) {
                out.push(m.range());
            }
        }
        out
    }

    fn mask_text(&self, v: &str, key: &HashKey) -> String {
        match self.mask {
            Mask::Hidden => self.token.clone(),
            Mask::Hash => key.hash(&normalize(v, self.norm)),
            Mask::Partial => partial(v, self.partial),
        }
    }
}

fn normalize(v: &str, n: Norm) -> String {
    match n {
        Norm::Digits => v.chars().filter(char::is_ascii_digit).collect(),
        Norm::Alnum => v.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect(),
        Norm::Lower => v.trim().to_lowercase(),
        Norm::Exact => v.to_string(),
    }
}

fn partial(v: &str, p: Partial) -> String {
    match p {
        Partial::Email => match v.split_once('@') {
            Some((user, domain)) => format!("{}•••@{domain}", user.chars().next().unwrap_or('•')),
            None => partial(v, Partial::KeepLast(4)),
        },
        Partial::Card => {
            let digits: Vec<char> = v.chars().filter(char::is_ascii_digit).collect();
            let last: String = digits[digits.len().saturating_sub(4)..].iter().collect();
            format!("•••• {last}")
        }
        Partial::KeepLast(n) => {
            let total = v.chars().filter(|c| c.is_alphanumeric()).count();
            // Short values would show most of themselves.
            let keep = if total < 2 * n as usize { 0 } else { n as usize };
            let mut seen = 0;
            v.chars()
                .map(|c| {
                    if !c.is_alphanumeric() {
                        return c;
                    }
                    seen += 1;
                    if seen > total - keep { c } else { '•' }
                })
                .collect()
        }
    }
}

/// A sensitive column's mask, for a whole value.
pub fn mask_value(how: Mask, v: &str, key: &HashKey) -> String {
    match how {
        Mask::Hidden => HIDDEN.into(),
        Mask::Hash => key.hash(&normalize(v, Norm::Exact)),
        Mask::Partial if v.contains('@') => partial(v, Partial::Email),
        Mask::Partial => partial(v, Partial::KeepLast(4)),
    }
}

/// Values masked in one reply, by type name.
pub type Counts = BTreeMap<String, u64>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished;

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the privacy check could not finish in time; ask for fewer rows or columns")
    }
}

/// The switched-on types, compiled once per settings change.
pub struct Types {
    set: Option<RegexSet>,
    kinds: Vec<Kind>,
    /// Switched-on custom types whose pattern does not compile.
    pub broken: Vec<String>,
}

impl Types {
    pub fn new(settings: &TypeSettings, overrides: &BTreeMap<String, bool>) -> Types {
        let on = |id: &str, global: bool| overrides.get(id).copied().unwrap_or(global);
        let mut kinds = Vec::new();
        let mut broken = Vec::new();
        for b in BUILTINS {
            if on(b.id, settings.builtin_on(b)) {
                kinds.push(Kind::builtin(b, settings.builtin_mask(b)));
            }
        }
        for c in settings.custom.iter().take(MAX_CUSTOM) {
            if !on(&c.id, c.on) {
                continue;
            }
            match check_pattern(&c.pattern).ok().and_then(|_| Kind::custom(c).ok()) {
                Some(k) => kinds.push(k),
                None => broken.push(c.name.clone()),
            }
        }
        let set = RegexSetBuilder::new(kinds.iter().map(|k| k.re.as_str()))
            .case_insensitive(false)
            .size_limit(8 * 1024 * 1024)
            .dfa_size_limit(8 * 1024 * 1024)
            .build()
            .ok();
        Types { set, kinds, broken }
    }

    pub fn none() -> Types {
        Types { set: None, kinds: Vec::new(), broken: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn scrub<'a>(&self, s: &'a str, key: &HashKey, counts: &mut Counts) -> Cow<'a, str> {
        if self.kinds.is_empty() || s.len() < 5 {
            return Cow::Borrowed(s);
        }
        let candidates: Vec<usize> = match &self.set {
            Some(set) => set.matches(s).into_iter().collect(),
            None => (0..self.kinds.len()).collect(),
        };
        if candidates.is_empty() {
            return Cow::Borrowed(s);
        }
        let mut spans: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
        for k in candidates {
            spans.extend(self.kinds[k].spans(s).into_iter().map(|r| (r, k)));
        }
        if spans.is_empty() {
            return Cow::Borrowed(s);
        }
        // Earliest first, then longest; overlapping later spans lose.
        spans.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)));
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        for (r, k) in spans {
            if r.start < at {
                continue;
            }
            let kind = &self.kinds[k];
            out.push_str(&s[at..r.start]);
            out.push_str(&kind.mask_text(&s[r.clone()], key));
            *counts.entry(kind.name.clone()).or_default() += 1;
            at = r.end;
        }
        out.push_str(&s[at..]);
        Cow::Owned(out)
    }

    /// Every string and object key, in place. Past `deadline` the whole reply is refused.
    pub fn scrub_json(&self, v: &mut Value, key: &HashKey, counts: &mut Counts, deadline: Instant) -> Result<(), Unfinished> {
        if self.kinds.is_empty() {
            return Ok(());
        }
        let mut n = 0u32;
        self.walk(v, key, counts, deadline, &mut n)
    }

    fn walk(&self, v: &mut Value, key: &HashKey, counts: &mut Counts, deadline: Instant, n: &mut u32) -> Result<(), Unfinished> {
        *n += 1;
        if n.is_multiple_of(64) && Instant::now() > deadline {
            return Err(Unfinished);
        }
        match v {
            Value::String(s) => {
                if let Cow::Owned(o) = self.scrub(s, key, counts) {
                    *s = o;
                }
            }
            // A card or a CPF kept in a numeric column comes back as a number.
            Value::Number(n) => {
                if let Cow::Owned(o) = self.scrub(&n.to_string(), key, counts) {
                    *v = Value::String(o);
                }
            }
            Value::Array(a) => {
                for x in a {
                    self.walk(x, key, counts, deadline, n)?;
                }
            }
            Value::Object(m) => {
                let renamed: Vec<(String, String)> = m
                    .keys()
                    .filter_map(|k| match self.scrub(k, key, counts) {
                        Cow::Owned(clean) => Some((k.clone(), clean)),
                        Cow::Borrowed(_) => None,
                    })
                    .collect();
                for (k, clean) in renamed {
                    if let Some(x) = m.remove(&k) {
                        m.insert(clean, x);
                    }
                }
                for x in m.values_mut() {
                    self.walk(x, key, counts, deadline, n)?;
                }
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(Unfinished);
        }
        Ok(())
    }
}

/// Per reply. Debug builds (tests) match many times slower than the app.
pub const BUDGET: std::time::Duration = std::time::Duration::from_millis(if cfg!(debug_assertions) { 1500 } else { 100 });

/// Values quoted by database errors become ‹value›; codes, constraint and column names, the
/// position and the hint stay.
pub fn clean_error(msg: &str) -> Cow<'_, str> {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            // PostgreSQL
            (r"(Key \([^)]*\))=\((?s:.*?)\)( already exists| is not present| conflicts|\.|$)", "$1=(‹value›)$2"),
            (r"(Failing row contains )\((?s:.*?)\)(\.|$)", "$1(‹value›)$2"),
            (r#"(invalid input (?:syntax|value) for [^:]*: )"(?:[^"]|"")*""#, "$1\"‹value›\""),
            (r#"(value too long for type [^:\n]*: )"(?:[^"]|"")*""#, "$1\"‹value›\""),
            (r#"(out of range for type [^:\n]*: )"(?:[^"]|"")*""#, "$1\"‹value›\""),
            (r#"(value )"(?:[^"]|"")*"( is out of range for type)"#, "$1\"‹value›\"$2"),
            (r#"(malformed [a-z]+ literal: )"(?:[^"]|"")*""#, "$1\"‹value›\""),
            (r#"(invalid value )"(?:[^"]|"")*"( for ")"#, "$1\"‹value›\"$2"),
            (r#"(field value out of range: )"(?:[^"]|"")*""#, "$1\"‹value›\""),
            // MySQL
            (r"(Duplicate entry )'(?:[^'\\]|\\.|'')*'( for key)", "$1'‹value›'$2"),
            (r"(Incorrect [a-z ]+ value: )'(?:[^'\\]|\\.|'')*'", "$1'‹value›'"),
            (r"(Truncated incorrect [A-Za-z ]+ value: )'(?:[^'\\]|\\.|'')*'", "$1'‹value›'"),
            // SQL Server
            (r"(The duplicate key value is )\((?s:.*?)\)(\.|$)", "$1(‹value›)$2"),
            (r"(Conversion failed when converting (?:the )?(?:[a-z]+ )?value )'(?:[^']|'')*'", "$1'‹value›'"),
            (r"(Error converting data type [^\n]*? )'(?:[^']|'')*'", "$1'‹value›'"),
            // MongoDB
            (r"(dup key: )\{(?s:.*?)\}", "$1{ ‹value› }"),
            // Elasticsearch / OpenSearch
            (r"(Preview of field's value: )'(?:[^'\\]|\\.)*'", "$1'‹value›'"),
            (r#"(failed to parse (?:date )?field \[[^\]]*\][^\n]*?input )'(?:[^'\\]|\\.)*'"#, "$1'‹value›'"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).expect("error rule"), r))
        .collect()
    });
    let mut out = Cow::Borrowed(msg);
    for (re, rep) in rules {
        if re.is_match(&out) {
            out = Cow::Owned(re.replace_all(&out, *rep).into_owned());
        }
    }
    out
}

// ---- What the agent sees ----

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentView {
    #[default]
    Data,
    Masked,
    Structure,
}

impl AgentView {
    pub const ALL: [AgentView; 3] = [AgentView::Data, AgentView::Masked, AgentView::Structure];

    pub fn label(self) -> &'static str {
        match self {
            AgentView::Data => "Data",
            AgentView::Masked => "Masked data",
            AgentView::Structure => "Structure only",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            AgentView::Data => "data",
            AgentView::Masked => "masked",
            AgentView::Structure => "structure",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisibleMode {
    #[default]
    Except,
    Only,
}

impl VisibleMode {
    pub fn label(self) -> &'static str {
        match self {
            VisibleMode::Except => "Everything except",
            VisibleMode::Only => "Only these",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemKind {
    Table,
    Schema,
    Column,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Table => "table",
            ItemKind::Schema => "schema",
            ItemKind::Column => "column",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleItem {
    pub name: String,
    pub kind: ItemKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visible {
    #[serde(default)]
    pub mode: VisibleMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<VisibleItem>,
}

/// `*` matches any run of characters; case is ignored.
pub fn glob(pattern: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.to_lowercase().chars().collect(), s.to_lowercase().chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Splits `a.b` names: SQL and MongoDB have a schema or database before the dot; Elasticsearch
/// and Redis names are flat (a Redis key pattern may hold dots).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    Qualified,
    Flat,
}

impl Names {
    pub fn of(engine: crate::Engine) -> Names {
        if matches!(engine, crate::Engine::Elasticsearch | crate::Engine::OpenSearch | crate::Engine::Redis) { Names::Flat } else { Names::Qualified }
    }
}

fn table_parts(name: &str, names: Names) -> (Option<&str>, &str) {
    match (names, name.split_once('.')) {
        (Names::Qualified, Some((s, t))) => (Some(s), t),
        _ => (None, name),
    }
}

/// `table.column`, `schema.table.column`, or for flat names everything before the last dot.
fn column_parts(name: &str, names: Names) -> (Option<&str>, &str, &str) {
    let (rest, col) = name.rsplit_once('.').unwrap_or(("*", name));
    match names {
        Names::Qualified => {
            let (s, t) = table_parts(rest, names);
            (s, t, col)
        }
        Names::Flat => (None, rest, col),
    }
}

fn name_eq(pattern: Option<&str>, actual: Option<&str>) -> bool {
    match (pattern, actual) {
        (None, _) => true,
        (Some(p), Some(a)) => glob(p, a),
        (Some(p), None) => p == "*",
    }
}

impl Visible {
    pub fn is_open(&self) -> bool {
        self.mode == VisibleMode::Except && self.items.is_empty()
    }

    fn schema_listed(&self, schema: Option<&str>) -> bool {
        self.items.iter().any(|i| i.kind == ItemKind::Schema && schema.is_some_and(|s| glob(i.name.trim_end_matches(".*"), s)))
    }

    fn table_listed(&self, schema: Option<&str>, table: &str, names: Names) -> bool {
        self.items.iter().any(|i| {
            i.kind == ItemKind::Table && {
                let (s, t) = table_parts(&i.name, names);
                name_eq(s, schema) && glob(t, table)
            }
        })
    }

    pub fn table_visible(&self, schema: Option<&str>, table: &str, names: Names) -> bool {
        let listed = self.schema_listed(schema) || self.table_listed(schema, table, names);
        match self.mode {
            VisibleMode::Except => !listed,
            VisibleMode::Only => listed,
        }
    }

    /// A schema (or MongoDB database) shows when anything in it can be visible.
    pub fn schema_visible(&self, schema: &str, names: Names) -> bool {
        match self.mode {
            VisibleMode::Except => !self.schema_listed(Some(schema)),
            VisibleMode::Only => {
                self.schema_listed(Some(schema))
                    || self.items.iter().any(|i| i.kind == ItemKind::Table && table_parts(&i.name, names).0.is_none_or(|s| glob(s, schema)))
            }
        }
    }

    pub fn column_hidden(&self, schema: Option<&str>, table: &str, column: &str, names: Names) -> bool {
        self.items.iter().any(|i| {
            i.kind == ItemKind::Column && {
                let (s, t, c) = column_parts(&i.name, names);
                name_eq(s, schema) && glob(t, table) && (glob(c, column) || column.starts_with(&format!("{c}.")))
            }
        })
    }

    /// The column entries that can apply to a table (patterns, as written).
    pub fn hidden_column_patterns(&self, schema: Option<&str>, table: &str, names: Names) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.kind == ItemKind::Column)
            .filter_map(|i| {
                let (s, t, c) = column_parts(&i.name, names);
                (name_eq(s, schema) && glob(t, table)).then(|| c.to_string())
            })
            .collect()
    }

    pub fn has_columns(&self) -> bool {
        self.items.iter().any(|i| i.kind == ItemKind::Column)
    }

    /// What a typed entry is: a known schema before `.*` or alone is a schema, `schema.table` a
    /// table, anything else with a dot a column.
    pub fn kind_of(text: &str, schemas: &[String], names: Names) -> ItemKind {
        let t = text.trim();
        if let Some(s) = t.strip_suffix(".*")
            && !s.contains('.')
        {
            return ItemKind::Schema;
        }
        if schemas.iter().any(|s| s.eq_ignore_ascii_case(t)) {
            return ItemKind::Schema;
        }
        match (names, t.matches('.').count()) {
            (_, 0) => ItemKind::Table,
            (Names::Flat, _) => ItemKind::Column,
            (Names::Qualified, 1) => {
                let first = t.split('.').next().unwrap_or("");
                if schemas.iter().any(|s| s.eq_ignore_ascii_case(first)) { ItemKind::Table } else { ItemKind::Column }
            }
            _ => ItemKind::Column,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensitiveCol {
    /// `table.column`, `schema.table.column` or a `*pattern*` for any column name.
    pub col: String,
    #[serde(default)]
    pub how: Mask,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub on: bool,
}

pub fn sensitive_match(entry: &str, schema: Option<&str>, table: &str, column: &str, names: Names) -> bool {
    if !entry.contains('.') {
        return glob(entry, column);
    }
    let (s, t, c) = column_parts(entry, names);
    name_eq(s, schema) && glob(t, table) && glob(c, column)
}

/// Column names that usually hold personal data (English and Portuguese), for Scan again.
pub fn sensitive_name(column: &str) -> Option<Mask> {
    static RE: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    let (hidden, hash, partial) = RE.get_or_init(|| {
        let r = |p: &str| RegexBuilder::new(p).case_insensitive(true).build().expect("name rule");
        (
            r(r"(^|_)(password|passwd|senha|secret|segredo|token|api_?key|private_?key|mfa|otp|salt)(_|$|hash)"),
            r(r"(^|_)(ssn|cpf|cnpj|rg|passport|passaporte|tax_?id|nif|iban|cartao|card_?number|pan)(_|$)"),
            r(r"(^|_)(e_?mail|phone|telefone|celular|mobile|birth|nascimento|birthday|salario|salary|endereco|address|street|rua|cep|zip)(_|$)"),
        )
    });
    if hidden.is_match(column) {
        Some(Mask::Hidden)
    } else if hash.is_match(column) {
        Some(Mask::Hash)
    } else if partial.is_match(column) {
        Some(Mask::Partial)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadFrom {
    Primary,
    SecondaryPreferred,
    Secondary,
}

impl ReadFrom {
    pub const ALL: [ReadFrom; 3] = [ReadFrom::Primary, ReadFrom::SecondaryPreferred, ReadFrom::Secondary];

    pub fn label(self) -> &'static str {
        match self {
            ReadFrom::Primary => "Primary",
            ReadFrom::SecondaryPreferred => "Prefer secondary",
            ReadFrom::Secondary => "Secondary",
        }
    }

    pub fn mongo(self) -> &'static str {
        match self {
            ReadFrom::Primary => "primary",
            ReadFrom::SecondaryPreferred => "secondaryPreferred",
            ReadFrom::Secondary => "secondary",
        }
    }
}

/// Redis' ACL categories that agents may use (`@dangerous` never runs for agents).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisCats {
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub read: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub write: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub keyspace: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub scripting: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub pubsub: bool,
}

impl Default for RedisCats {
    fn default() -> Self {
        RedisCats { read: true, write: true, keyspace: true, scripting: true, pubsub: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedisCat {
    Read,
    Write,
    Keyspace,
    Scripting,
    Dangerous,
    Pubsub,
}

impl RedisCat {
    pub const ALL: [RedisCat; 6] = [RedisCat::Read, RedisCat::Write, RedisCat::Keyspace, RedisCat::Scripting, RedisCat::Dangerous, RedisCat::Pubsub];

    pub fn label(self) -> &'static str {
        match self {
            RedisCat::Read => "@read",
            RedisCat::Write => "@write",
            RedisCat::Keyspace => "@keyspace",
            RedisCat::Scripting => "@scripting",
            RedisCat::Dangerous => "@dangerous",
            RedisCat::Pubsub => "@pubsub",
        }
    }

    pub fn examples(self) -> &'static str {
        match self {
            RedisCat::Read => "GET, HGETALL, LRANGE, SCAN",
            RedisCat::Write => "SET, HSET, LPUSH, EXPIRE",
            RedisCat::Keyspace => "DEL, UNLINK, RENAME",
            RedisCat::Scripting => "EVAL, EVALSHA, FUNCTION CALL",
            RedisCat::Dangerous => "FLUSHALL, FLUSHDB, CONFIG, DEBUG, KEYS",
            RedisCat::Pubsub => "PUBLISH, SUBSCRIBE",
        }
    }

    /// Dataman's own list, after Redis' `COMMAND INFO` categories. Read-only admin commands
    /// Redis tags `@dangerous` (INFO, ROLE, CLIENT LIST, SLOWLOG GET) count as reads.
    pub fn of(command: &str) -> RedisCat {
        let c = command.to_ascii_uppercase();
        let first = c.split(' ').next().unwrap_or("");
        match first {
            "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" | "SCRIPT" | "FUNCTION" => RedisCat::Scripting,
            "PUBLISH" | "SPUBLISH" | "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE" | "PUBSUB" => RedisCat::Pubsub,
            "DEL" | "UNLINK" | "RENAME" | "RENAMENX" | "COPY" | "MOVE" | "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" | "PERSIST" | "TOUCH" | "OBJECT" | "DUMP" | "RESTORE" | "MIGRATE" => {
                RedisCat::Keyspace
            }
            "KEYS" | "FLUSHALL" | "FLUSHDB" | "CONFIG" | "DEBUG" | "SHUTDOWN" | "SAVE" | "BGSAVE" | "BGREWRITEAOF" | "REPLICAOF" | "SLAVEOF" | "MONITOR" | "SYNC" | "PSYNC" | "ACL" | "MODULE" | "FAILOVER" | "CLUSTER" | "SWAPDB" | "LASTSAVE" | "LATENCY" | "MEMORY" | "SORT" => {
                match c.as_str() {
                    "CONFIG GET" | "CLUSTER INFO" | "CLUSTER NODES" | "ACL WHOAMI" | "LATENCY LATEST" | "MEMORY USAGE" | "MEMORY STATS" => RedisCat::Read,
                    _ => RedisCat::Dangerous,
                }
            }
            _ if crate::nosql::redis::is_read_command(&c) => RedisCat::Read,
            _ => RedisCat::Write,
        }
    }
}

impl RedisCats {
    pub fn allows(&self, c: RedisCat) -> bool {
        match c {
            RedisCat::Read => self.read,
            RedisCat::Write => self.write,
            RedisCat::Keyspace => self.keyspace,
            RedisCat::Scripting => self.scripting,
            RedisCat::Pubsub => self.pubsub,
            RedisCat::Dangerous => false,
        }
    }

    pub fn set(&mut self, c: RedisCat, on: bool) {
        match c {
            RedisCat::Read => self.read = on,
            RedisCat::Write => self.write = on,
            RedisCat::Keyspace => self.keyspace = on,
            RedisCat::Scripting => self.scripting = on,
            RedisCat::Pubsub => self.pubsub = on,
            RedisCat::Dangerous => {}
        }
    }
}

/// A connection's rules for agents: Privacy and Permissions in the Agents area. No MCP tool
/// writes them (`update_connection` keeps them as they are).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRules {
    #[serde(default, skip_serializing_if = "is_data")]
    pub view: AgentView,
    #[serde(default, skip_serializing_if = "Visible::is_open")]
    pub visible: Visible,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive: Vec<SensitiveCol>,
    /// Sensitive data types switched differently here than in All connections.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<String, bool>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub structure: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub code: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub activity: bool,
    #[serde(default, skip_serializing_if = "is_default_redis")]
    pub redis: RedisCats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_from: Option<ReadFrom>,
}

fn is_data(v: &AgentView) -> bool {
    *v == AgentView::Data
}

fn is_default_redis(r: &RedisCats) -> bool {
    *r == RedisCats::default()
}

impl Default for AgentRules {
    fn default() -> Self {
        AgentRules {
            view: AgentView::Data,
            visible: Visible::default(),
            sensitive: Vec::new(),
            types: BTreeMap::new(),
            structure: true,
            code: true,
            activity: true,
            redis: RedisCats::default(),
            time_limit_s: None,
            read_rows: None,
            read_bytes: None,
            read_from: None,
        }
    }
}

pub const TIME_LIMITS: [u64; 4] = [10, 30, 120, 300];
pub const READ_ROWS: [u64; 3] = [20, 100, 500];
pub const READ_BYTES: [u64; 2] = [64 * 1024, 16 * 1024];
pub const WRITE_ROWS: [u64; 4] = [10, 100, 1000, 10_000];

impl AgentRules {
    pub fn is_default(&self) -> bool {
        *self == AgentRules::default()
    }

    /// Something is kept from agents: they see less than the user does.
    pub fn restricted(&self) -> bool {
        self.view != AgentView::Data || !self.visible.is_open()
    }

    /// Server activity carries other sessions' statements and values: off while data is hidden or masked.
    pub fn activity_on(&self) -> bool {
        self.activity && !self.restricted()
    }

    pub fn activity_forced_off(&self) -> bool {
        self.restricted()
    }

    pub fn sensitive_mask(&self, schema: Option<&str>, table: &str, column: &str, names: Names) -> Option<Mask> {
        if self.view != AgentView::Masked {
            return None;
        }
        self.sensitive.iter().filter(|s| s.on).find(|s| sensitive_match(&s.col, schema, table, column, names)).map(|s| s.how)
    }

    pub fn time_limit(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.time_limit_s.unwrap_or(300).clamp(1, 300))
    }

    /// The rules with each generated (computed) column protected as the strongest of the
    /// columns it is computed from: hidden if any is hidden, else masked with the strongest
    /// mask among them. Followed through generated columns computed from generated columns.
    pub fn with_generated(&self, cols: &[Generated], names: Names) -> AgentRules {
        let mut r = self.clone();
        let rank = |m: Option<Mask>| match m {
            None => 0,
            Some(Mask::Partial) => 1,
            Some(Mask::Hash) => 2,
            Some(Mask::Hidden) => 3,
        };
        loop {
            let mut changed = false;
            for g in cols {
                let s = Some(g.schema.as_str());
                if r.visible.column_hidden(s, &g.table, &g.column, names) {
                    continue;
                }
                let name = format!("{}.{}.{}", g.schema, g.table, g.column);
                if g.from.iter().any(|d| r.visible.column_hidden(s, &g.table, d, names)) {
                    r.visible.items.push(VisibleItem { name, kind: ItemKind::Column });
                    changed = true;
                    continue;
                }
                let strongest = g.from.iter().map(|d| r.sensitive_mask(s, &g.table, d, names)).max_by_key(|m| rank(*m)).flatten();
                if rank(strongest) > rank(r.sensitive_mask(s, &g.table, &g.column, names))
                    && let Some(how) = strongest
                {
                    // First: the first entry that matches a column is its mask.
                    r.sensitive.insert(0, SensitiveCol { col: name, how, on: true });
                    changed = true;
                }
            }
            if !changed {
                return r;
            }
        }
    }
}

/// A generated (computed) column and the columns of its table its expression reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub from: Vec<String>,
}

// ---- Validators ----

fn digits(s: &str) -> Vec<u32> {
    s.chars().filter_map(|c| c.to_digit(10)).collect()
}

pub fn luhn(s: &str) -> bool {
    let d = digits(s);
    if !(12..=19).contains(&d.len()) {
        return false;
    }
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 { y - 9 } else { y }
            } else {
                x
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn card_issuer(s: &str) -> bool {
    let d: String = s.chars().filter(char::is_ascii_digit).collect();
    let p = |n: usize| d.get(..n).and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
    let len = d.len();
    (d.starts_with('4') && matches!(len, 13 | 16 | 19))
        || ((51..=55).contains(&p(2)) || (2221..=2720).contains(&p(4))) && len == 16
        || matches!(p(2), 34 | 37) && len == 15
        || (d.starts_with("6011") || d.starts_with("65") || (644..=649).contains(&p(3))) && len >= 16
        || (3528..=3589).contains(&p(4)) && len >= 16
        || matches!(p(2), 36 | 38 | 39) && len >= 14
        || (300..=305).contains(&p(3)) && len >= 14
        || (d.starts_with("50") || (56..=69).contains(&p(2))) && (12..=19).contains(&len)
}

pub fn cpf(s: &str) -> bool {
    let d = digits(s);
    if d.len() != 11 || d.iter().all(|x| *x == d[0]) {
        return false;
    }
    let check = |n: usize| {
        let sum: u32 = (0..n).map(|i| d[i] * (n as u32 + 1 - i as u32)).sum();
        let r = (sum * 10) % 11;
        if r == 10 { 0 } else { r }
    };
    check(9) == d[9] && check(10) == d[10]
}

pub fn cnpj(s: &str) -> bool {
    let v: Vec<u32> = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase() as u32 - 48).collect();
    if v.len() != 14 || v[12] > 9 || v[13] > 9 || v.iter().all(|x| *x == v[0]) {
        return false;
    }
    let check = |n: usize| {
        let weights: Vec<u32> = if n == 12 { vec![5, 4, 3, 2, 9, 8, 7, 6, 5, 4, 3, 2] } else { vec![6, 5, 4, 3, 2, 9, 8, 7, 6, 5, 4, 3, 2] };
        let sum: u32 = (0..n).map(|i| v[i] * weights[i]).sum();
        let r = sum % 11;
        if r < 2 { 0 } else { 11 - r }
    };
    check(12) == v[12] && check(13) == v[13]
}

pub fn iban(s: &str) -> bool {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    if !(15..=34).contains(&c.len()) || !c[..2].chars().all(|x| x.is_ascii_alphabetic()) {
        return false;
    }
    let moved = format!("{}{}", &c[4..], &c[..4]);
    let mut rem: u64 = 0;
    for ch in moved.chars() {
        let n = ch.to_digit(36).unwrap_or(0) as u64;
        rem = if n > 9 { (rem * 100 + n) % 97 } else { (rem * 10 + n) % 97 };
    }
    rem == 1
}

pub fn entropy(s: &str) -> f64 {
    let n = s.chars().count();
    if n == 0 {
        return 0.0;
    }
    let mut freq: BTreeMap<char, usize> = BTreeMap::new();
    for c in s.chars() {
        *freq.entry(c).or_default() += 1;
    }
    freq.values().map(|&k| {
        let p = k as f64 / n as f64;
        -p * p.log2()
    })
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> HashKey {
        HashKey::fixed(b"test-key")
    }

    fn scrub(t: &Types, s: &str) -> (String, Counts) {
        let mut c = Counts::new();
        (t.scrub(s, &key(), &mut c).into_owned(), c)
    }

    #[test]
    fn validators() {
        assert!(cpf("123.456.789-09") && cpf("12345678909") && !cpf("123.456.789-00") && !cpf("111.111.111-11"));
        assert!(cnpj("11.222.333/0001-81") && !cnpj("11.222.333/0001-80"));
        assert!(cnpj("12.ABC.345/01DE-35"), "alphanumeric CNPJ");
        assert!(luhn("4242 4242 4242 4242") && !luhn("4242 4242 4242 4241"));
        assert!(iban("GB82 WEST 1234 5698 7654 32") && !iban("GB82 WEST 1234 5698 7654 33"));
        assert!(entropy("aZ8kQ2pL9xW4mN7r") > 3.5 && entropy("aaaaaaaa") < 1.0);
    }

    #[test]
    fn builtin_types_mask_in_any_value() {
        let t = Types::new(&TypeSettings::default(), &BTreeMap::new());
        let (s, c) = scrub(&t, "cliente 123.456.789-09 pagou com 4242 4242 4242 4242, ana@acme.com, (81) 98765-4321");
        assert_eq!(s, "cliente •••.•••.•89-09 pagou com •••• 4242, a•••@acme.com, (••) •••••-4321");
        assert_eq!((c["CPF"], c["Card number"], c["E-mail"], c["Phone"]), (1, 1, 1, 1));
        assert_eq!(scrub(&t, "12.ABC.345/01DE-35").0, "••.•••.•••/••DE-35");
        assert_eq!(scrub(&t, "token sk_live_51HxABCDEFGHIJKLMN ok").0, "token ‹hidden› ok");
        assert_eq!(scrub(&t, "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc").0, "Bearer ‹hidden›");
        assert_eq!(scrub(&t, "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----!").0, "‹hidden›!");
        assert_eq!(scrub(&t, "$2b$12$KixQ0aaaaaaaaaaaaaaaaa").0, "‹hidden›");
        assert_eq!(scrub(&t, "order 12345678901 x 4242424242424241").0, "order 12345678901 x 4242424242424241", "invalid check digits stay");
        assert_eq!(scrub(&t, "10.0.3.17").0, "10.0.3.17", "IP is off by default");
    }

    #[test]
    fn switches_masks_and_custom_types() {
        let mut s = TypeSettings::default();
        s.set_builtin("ip", Some(true), None);
        s.set_builtin("email", None, Some(Mask::Hash));
        assert_eq!(s.builtin.len(), 2);
        s.set_builtin("email", None, Some(Mask::Partial));
        assert_eq!(s.builtin.len(), 1, "back to the default is no override");
        let mut c = CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hash);
        c.id = "t_1".into();
        s.custom.push(c);
        let mut v = CustomType::new("Senha", r"senha=(?P<v>\S+)", Mask::Hidden);
        v.id = "t_2".into();
        s.custom.push(v);
        let t = Types::new(&s, &BTreeMap::new());
        let (a, counts) = scrub(&t, "merge CUST-000017 into CUST-000930; senha=hunter2 host 10.0.3.17");
        assert!(a.starts_with("merge ‹h:") && a.contains(" into ‹h:") && a.contains("senha=‹senha›") && !a.contains("10.0.3.17"), "{a}");
        assert_eq!(counts["Customer code"], 2);
        assert_eq!(scrub(&t, "CUST-000017").0, scrub(&t, "CUST-000017").0, "hashes are deterministic");
        assert_ne!(scrub(&t, "CUST-000017").0, scrub(&t, "CUST-000018").0);
        let off: BTreeMap<String, bool> = [("t_1".to_string(), false), ("phone".to_string(), false)].into();
        let t = Types::new(&s, &off);
        assert_eq!(scrub(&t, "CUST-000017 (81) 98765-4321").0, "CUST-000017 (81) 98765-4321");
        let rows = s.rows(&off);
        assert!(rows.iter().find(|r| r.id == "t_1").is_some_and(|r| !r.on && r.changed_here && r.custom));
        assert!(rows.iter().find(|r| r.id == "cpf").is_some_and(|r| r.on && !r.changed_here));
    }

    #[test]
    fn json_keys_and_values_and_budget() {
        let t = Types::new(&TypeSettings::default(), &BTreeMap::new());
        let mut v = serde_json::json!({"rows": [["ana@acme.com", 1]], "ana@acme.com": {"cpf": "123.456.789-09"}});
        let mut c = Counts::new();
        t.scrub_json(&mut v, &key(), &mut c, Instant::now() + BUDGET).unwrap();
        let text = v.to_string();
        assert!(!text.contains("ana@acme.com") && !text.contains("123.456.789-09"), "{text}");
        assert_eq!(c["E-mail"], 2);
        let mut big = serde_json::json!((0..5000).map(|i| format!("ana{i}@acme.com")).collect::<Vec<_>>());
        assert_eq!(t.scrub_json(&mut big, &key(), &mut c, Instant::now()), Err(Unfinished));
    }

    #[test]
    #[ignore]
    fn scrub_speed() {
        let t = Types::new(&TypeSettings::default(), &BTreeMap::new());
        let mut v = serde_json::json!({"rows": (0..200).map(|i| vec![serde_json::json!("x".repeat(2000)), serde_json::json!(format!("ana{i}@acme.com 123.456.789-09"))]).collect::<Vec<_>>()});
        let t0 = Instant::now();
        t.scrub_json(&mut v, &key(), &mut Counts::new(), Instant::now() + std::time::Duration::from_secs(60)).unwrap();
        eprintln!("scrub 400 KB: {:?}", t0.elapsed());
    }

    #[test]
    fn custom_patterns_are_checked() {
        assert_eq!(check_pattern(r"(?=x)abc").unwrap_err(), "Look-around isn't supported.");
        assert_eq!(check_pattern(r"(a)\1").unwrap_err(), "Back-references aren't supported.");
        assert_eq!(check_pattern(r"a*").unwrap_err(), "This pattern matches empty text, so it would hide everything.");
        assert_eq!(check_pattern(&"a".repeat(501)).unwrap_err(), "Pattern is too large.");
        assert!(check_pattern(r"\bCUST-[0-9]{6}\b").is_ok());
        assert!(check_pattern(r"a\\1").is_ok(), "an escaped backslash is not a back-reference");
        assert!(check_pattern(r"(").is_err());
        let c = CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hash);
        let t = test_pattern(&c, &["Pedido de CUST-004211 enviado", "merge CUST-000017 into CUST-000930", "ref CUST-42", "xCUST-123456"], &key()).unwrap();
        assert_eq!(t.matches, 3);
        assert!(t.lines[2].2.is_none() && t.lines[3].2.is_none());
        assert!(!t.broad, "half the lines is not most");
        let wide = CustomType::new("Digits", r"[0-9]+", Mask::Hidden);
        assert!(test_pattern(&wide, &["a1", "b2", "c"], &key()).unwrap().broad);
        let mut s = TypeSettings::default();
        let mut bad = CustomType::new("Broken", "(", Mask::Hidden);
        bad.id = "t_x".into();
        s.custom.push(bad);
        assert_eq!(Types::new(&s, &BTreeMap::new()).broken, vec!["Broken".to_string()]);
        assert!(s.rows(&BTreeMap::new()).last().unwrap().broken.is_some());
    }

    #[test]
    fn errors_lose_their_values() {
        let cases = [
            ("duplicate key value violates unique constraint \"users_email_key\"\nKey (email)=(ana@acme.com) already exists.", "Key (email)=(‹value›) already exists."),
            ("new row for relation \"t\" violates check constraint \"c\"\nFailing row contains (1, ana, 123).", "Failing row contains (‹value›)."),
            ("invalid input syntax for type integer: \"12a\"", "invalid input syntax for type integer: \"‹value›\""),
            ("Duplicate entry 'ana@acme.com' for key 'users.email'", "Duplicate entry '‹value›' for key 'users.email'"),
            ("Cannot insert duplicate key row in object 'dbo.users' with unique index 'ix'. The duplicate key value is (ana@acme.com).", "The duplicate key value is (‹value›)."),
            ("Conversion failed when converting the varchar value 'abc' to data type int.", "Conversion failed when converting the varchar value '‹value›' to data type int."),
            ("E11000 duplicate key error collection: shop.users index: email_1 dup key: { email: \"ana@acme.com\" }", "dup key: { ‹value› }"),
            ("failed to parse field [age] of type [long] in document with id '1'. Preview of field's value: 'abc'", "Preview of field's value: '‹value›'"),
        ];
        for (raw, want) in cases {
            let c = clean_error(raw);
            assert!(c.contains(want), "{raw}\n→ {c}");
            assert!(!c.contains("ana@acme.com") && !c.contains("'abc'") && !c.contains("12a\""), "{c}");
        }
        assert_eq!(clean_error("relation \"x\" does not exist"), "relation \"x\" does not exist");
    }

    #[test]
    fn visibility() {
        let q = Names::Qualified;
        let v = Visible {
            mode: VisibleMode::Except,
            items: vec![
                VisibleItem { name: "public.payments".into(), kind: ItemKind::Table },
                VisibleItem { name: "audit.*".into(), kind: ItemKind::Schema },
                VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column },
            ],
        };
        assert!(!v.table_visible(Some("public"), "payments", q) && !v.table_visible(Some("PUBLIC"), "Payments", q));
        assert!(v.table_visible(Some("other"), "payments", q) && v.table_visible(Some("public"), "users", q));
        assert!(!v.table_visible(Some("audit"), "log", q) && !v.schema_visible("audit", q) && v.schema_visible("public", q));
        assert!(v.column_hidden(Some("public"), "users", "password_hash", q) && !v.column_hidden(Some("public"), "users", "email", q));
        assert_eq!(v.hidden_column_patterns(Some("public"), "users", q), vec!["password_hash".to_string()]);
        let only = Visible { mode: VisibleMode::Only, items: vec![VisibleItem { name: "public.orders".into(), kind: ItemKind::Table }, VisibleItem { name: "orders.notes".into(), kind: ItemKind::Column }] };
        assert!(only.table_visible(Some("public"), "orders", q) && !only.table_visible(Some("public"), "users", q));
        assert!(only.schema_visible("public", q) && !only.schema_visible("audit", q));
        assert!(only.column_hidden(Some("public"), "orders", "notes", q));
        let f = Names::Flat;
        let r = Visible { mode: VisibleMode::Except, items: vec![VisibleItem { name: "session:*".into(), kind: ItemKind::Table }, VisibleItem { name: "user:*.password".into(), kind: ItemKind::Column }] };
        assert!(!r.table_visible(None, "session:9", f) && r.table_visible(None, "user:1", f));
        assert!(r.column_hidden(None, "user:1", "password", f));
        let schemas = vec!["public".to_string(), "audit".to_string()];
        assert_eq!(Visible::kind_of("public.payments", &schemas, q), ItemKind::Table);
        assert_eq!(Visible::kind_of("audit.*", &schemas, q), ItemKind::Schema);
        assert_eq!(Visible::kind_of("users.password_hash", &schemas, q), ItemKind::Column);
        assert_eq!(Visible::kind_of("payments", &schemas, q), ItemKind::Table);
        assert_eq!(Visible::kind_of("user:*.password", &[], f), ItemKind::Column);
        assert!(glob("payments-*", "payments-2026") && !glob("payments-*", "orders") && glob("*", ""));
    }

    #[test]
    fn rules_roundtrip_and_defaults() {
        let r = AgentRules::default();
        assert_eq!(toml::to_string(&r).unwrap().trim(), "", "defaults write nothing");
        let mut r = AgentRules { view: AgentView::Masked, structure: false, time_limit_s: Some(30), read_from: Some(ReadFrom::SecondaryPreferred), ..Default::default() };
        r.sensitive.push(SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true });
        r.visible.items.push(VisibleItem { name: "public.payments".into(), kind: ItemKind::Table });
        r.redis.scripting = false;
        let text = toml::to_string(&r).unwrap();
        let back: AgentRules = toml::from_str(&text).unwrap();
        assert_eq!(back, r, "{text}");
        assert!(back.restricted() && !back.activity_on() && back.activity);
        assert_eq!(back.sensitive_mask(Some("public"), "users", "EMAIL", Names::Qualified), Some(Mask::Partial));
        assert_eq!(AgentRules { view: AgentView::Data, ..back.clone() }.sensitive_mask(None, "users", "email", Names::Qualified), None, "columns mask in Masked data only");
        assert_eq!(sensitive_name("password_hash"), Some(Mask::Hidden));
        assert_eq!(sensitive_name("cpf"), Some(Mask::Hash));
        assert_eq!(sensitive_name("email"), Some(Mask::Partial));
        assert_eq!(sensitive_name("created_at"), None);
        assert_eq!(RedisCat::of("KEYS"), RedisCat::Dangerous);
        assert_eq!(RedisCat::of("CONFIG GET"), RedisCat::Read);
        assert_eq!(RedisCat::of("HGETALL"), RedisCat::Read);
        assert_eq!(RedisCat::of("SET"), RedisCat::Write);
        assert_eq!(RedisCat::of("DEL"), RedisCat::Keyspace);
        assert_eq!(RedisCat::of("EVAL"), RedisCat::Scripting);
        assert!(!RedisCats::default().allows(RedisCat::Dangerous));
    }

    #[test]
    fn partial_and_column_masks() {
        assert_eq!(mask_value(Mask::Partial, "ana@acme.com", &key()), "a•••@acme.com");
        assert_eq!(mask_value(Mask::Partial, "98765-4321", &key()), "•••••-4321");
        assert_eq!(mask_value(Mask::Partial, "abc", &key()), "•••");
        assert_eq!(mask_value(Mask::Hidden, "x", &key()), HIDDEN);
        assert!(mask_value(Mask::Hash, "x", &key()).starts_with("‹h:"));
    }
}
