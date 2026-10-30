//! The script as the review shows it: a long statement broken into its clauses, a command's JSON
//! indented where it doesn't fit on a line. Only for reading: what runs is the same statements.

use serde_json::Value;

use crate::config::Engine;

/// Past this width a statement is broken up.
const WIDTH: usize = 80;
const IN: &str = "  ";

/// One step of the plan, laid out for its engine.
pub(crate) fn step(engine: Option<Engine>, text: &str) -> String {
    match engine {
        Some(Engine::MongoDb) => lines(text, mongo),
        Some(Engine::Elasticsearch | Engine::OpenSearch) => search(text),
        Some(Engine::Redis) => lines(text, redis),
        Some(Engine::SqlServer) => lines(text, |l| sql(l, true)),
        Some(_) => lines(text, |l| sql(l, false)),
        None => text.to_string(),
    }
}

fn lines(text: &str, f: impl Fn(&str) -> String) -> String {
    text.lines().map(f).collect::<Vec<_>>().join("\n")
}

fn width(s: &str) -> usize {
    s.chars().count()
}

// ---- JSON (MongoDB's commands, the search engines' bodies) ----

/// `v` starting at column `col` of a line indented by `indent`: whole when it fits, else one
/// member a line.
fn json(v: &Value, col: usize, indent: usize, out: &mut String) {
    let flat = flat(v);
    let (open, close, items): (char, char, Vec<(Option<&String>, &Value)>) = match v {
        Value::Object(m) if !m.is_empty() => ('{', '}', m.iter().map(|(k, v)| (Some(k), v)).collect()),
        Value::Array(a) if !a.is_empty() => ('[', ']', a.iter().map(|v| (None, v)).collect()),
        _ => return out.push_str(&flat),
    };
    if col + width(&flat) <= WIDTH {
        return out.push_str(&flat);
    }
    let pad = " ".repeat(indent + IN.len());
    out.push(open);
    // A list of names or numbers fills its lines.
    if let Value::Array(a) = v
        && a.iter().all(|v| !v.is_object() && !v.is_array())
    {
        let mut line = pad.clone();
        for (k, item) in a.iter().enumerate() {
            let item = format!("{item}{}", if k + 1 < a.len() { "," } else { "" });
            if line.len() > pad.len() && width(&line) + 1 + width(&item) > WIDTH {
                out.push('\n');
                out.push_str(line.trim_end());
                line = pad.clone();
            }
            line.push_str(&item);
            line.push(' ');
        }
        out.push('\n');
        out.push_str(line.trim_end());
        out.push('\n');
        out.push_str(&" ".repeat(indent));
        out.push(close);
        return;
    }
    for (k, (key, val)) in items.iter().enumerate() {
        out.push('\n');
        out.push_str(&pad);
        let key = key.map(|k| format!("{}: ", Value::String(k.clone()))).unwrap_or_default();
        out.push_str(&key);
        json(val, pad.len() + width(&key), pad.len(), out);
        if k + 1 < items.len() {
            out.push(',');
        }
    }
    out.push('\n');
    out.push_str(&" ".repeat(indent));
    out.push(close);
}

fn flat(v: &Value) -> String {
    match v {
        Value::Object(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", Value::String(k.clone()), flat(v))).collect::<Vec<_>>().join(", ")),
        Value::Array(a) => format!("[{}]", a.iter().map(flat).collect::<Vec<_>>().join(", ")),
        v => v.to_string(),
    }
}

/// `db.getSiblingDB("admin").runCommand({…})`: the command's document indented.
fn mongo(line: &str) -> String {
    let Some(at) = line.find("runCommand(").map(|i| i + "runCommand(".len()) else { return line.to_string() };
    let Some(body) = line[at..].strip_suffix(')') else { return line.to_string() };
    let Ok(v) = serde_json::from_str::<Value>(body) else { return line.to_string() };
    let mut out = line[..at].to_string();
    json(&v, width(&out), 0, &mut out);
    out.push(')');
    out
}

/// `PUT /_security/role/x` and its body: the body as wide as it fits.
fn search(text: &str) -> String {
    let Some((head, body)) = text.split_once('\n') else { return text.to_string() };
    let Ok(v) = serde_json::from_str::<Value>(body) else { return text.to_string() };
    let mut out = format!("{head}\n");
    json(&v, 0, 0, &mut out);
    out
}

// ---- Redis ----

/// `ACL SETUSER`: its rules a kind a line (the login, keys and channels, categories, commands,
/// each selector).
fn redis(line: &str) -> String {
    if width(line) <= WIDTH || !line.starts_with("ACL SETUSER ") {
        return line.to_string();
    }
    let words = words(line);
    let kind = |w: &str| match w {
        _ if w.starts_with('"') || w.starts_with('(') => 4,
        _ if w.starts_with("+@") || w.starts_with("-@") || w == "allcommands" || w == "nocommands" => 2,
        _ if w.starts_with('+') || w.starts_with('-') => 3,
        _ if w.starts_with(['~', '%', '&']) || w.starts_with("reset") && w != "reset" || w == "allkeys" || w == "allchannels" => 1,
        _ => 0,
    };
    let mut out: Vec<String> = Vec::new();
    let (mut cur, mut was) = (words[..3.min(words.len())].join(" "), 0);
    for w in words.iter().skip(3) {
        let k = kind(w);
        if k != was || k == 4 || width(&cur) + 1 + width(w) > WIDTH {
            out.push(std::mem::replace(&mut cur, format!("{IN}{w}")));
        } else {
            cur.push(' ');
            cur.push_str(w);
        }
        was = k;
    }
    out.push(cur);
    out.join("\n")
}

/// Space-separated words, a double-quoted one whole.
fn words(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open = false;
    for w in line.split(' ') {
        match out.last_mut() {
            Some(last) if open => {
                last.push(' ');
                last.push_str(w);
            }
            _ => out.push(w.to_string()),
        }
        let quotes = w.matches('"').count();
        if quotes % 2 == 1 {
            open = !open;
        }
    }
    out
}

// ---- SQL ----

fn sql(line: &str, ms: bool) -> String {
    if width(line) <= WIDTH || line.starts_with("--") {
        return line.to_string();
    }
    let top = outside(line, ms);
    let laid = if let Some(at) = first(line, &top, " AS SELECT ") {
        view(line, &top, at, ms)
    } else if line.starts_with("GRANT ") || line.starts_with("REVOKE ") {
        grant(line, &top)
    } else if line.starts_with("EXEC ") {
        exec(line, &top)
    } else if let Some(at) = first(line, &top, " WITH ").filter(|_| ms) {
        Some(format!("{}\n{IN}{}", &line[..at + " WITH".len()], pieces(line, &top, at + " WITH ".len(), line.len()).join(&format!(",\n{IN}"))))
    } else {
        clauses(line, &top)
    };
    laid.unwrap_or_else(|| line.to_string())
}

/// For each byte of `s`, whether it is outside quotes, brackets (SQL Server's) and parentheses.
fn outside(s: &str, brackets: bool) -> Vec<bool> {
    let mut out = vec![false; s.len()];
    let (mut depth, mut quote) = (0i32, None::<u8>);
    for (i, &c) in s.as_bytes().iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'[' if brackets => quote = Some(b']'),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => out[i] = depth == 0,
            },
        }
    }
    out
}

fn all(s: &str, top: &[bool], pat: &str) -> Vec<usize> {
    s.match_indices(pat).map(|(i, _)| i).filter(|&i| top[i..i + pat.len()].iter().all(|t| *t)).collect()
}

fn first(s: &str, top: &[bool], pat: &str) -> Option<usize> {
    all(s, top, pat).first().copied()
}

fn last(s: &str, top: &[bool], pat: &str) -> Option<usize> {
    all(s, top, pat).last().copied()
}

/// `s[from..to]` split at its top-level commas, each piece trimmed.
fn pieces(s: &str, top: &[bool], from: usize, to: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = from;
    for i in all(s, top, ",").into_iter().filter(|&i| i >= from && i < to) {
        out.push(s[start..i].trim().to_string());
        start = i + 1;
    }
    out.push(s[start..to].trim().to_string());
    out
}

/// `CREATE VIEW v AS SELECT a, b, CASE … END AS c FROM t`: a column a line, a long CASE a branch
/// a line.
fn view(s: &str, top: &[bool], at: usize, ms: bool) -> Option<String> {
    let cols = at + " AS SELECT ".len();
    let from = last(s, top, " FROM ").filter(|&f| f > cols)?;
    let mut out = format!("{} AS\nSELECT\n", &s[..at]);
    let cols = pieces(s, top, cols, from);
    for (k, c) in cols.iter().enumerate() {
        out.push_str(&column(c, ms));
        out.push_str(if k + 1 < cols.len() { ",\n" } else { "\n" });
    }
    out.push_str(&s[from + 1..]);
    Some(out)
}

fn column(c: &str, ms: bool) -> String {
    if width(IN) + width(c) <= WIDTH || !c.starts_with("CASE ") {
        return format!("{IN}{c}");
    }
    let top = outside(c, ms);
    let mut cuts: Vec<usize> = all(c, &top, " WHEN ").into_iter().chain(all(c, &top, " ELSE ")).chain(last(c, &top, " END")).collect();
    cuts.sort();
    let mut out = format!("{IN}CASE");
    let mut start = "CASE".len();
    for (k, &i) in cuts.iter().enumerate() {
        let part = c[start..i].trim();
        if k > 0 {
            // A long branch: its THEN under its WHEN.
            let then = part.find(" THEN ").filter(|_| width(IN) * 2 + width(part) > WIDTH);
            match then {
                Some(t) => out.push_str(&format!("\n{IN}{IN}{}\n{IN}{IN}{IN}{}", &part[..t], &part[t + 1..])),
                None => out.push_str(&format!("\n{IN}{IN}{part}")),
            }
        }
        start = i;
    }
    out.push_str(&format!("\n{IN}{}", c[start..].trim()));
    out
}

/// `GRANT a (…), b (…) ON t TO r`: the privileges a line when they don't fit, ON and TO each a
/// line.
fn grant(s: &str, top: &[bool]) -> Option<String> {
    let verb = s.find(' ')?;
    let to = last(s, top, " TO ").or_else(|| last(s, top, " FROM "))?;
    let on = first(s, top, " ON ").filter(|&o| o < to);
    let privs_end = on.unwrap_or(to);
    let mut out = if width(&s[..privs_end]) <= WIDTH {
        s[..privs_end].to_string()
    } else {
        format!("{}\n{IN}{}", &s[..verb], pieces(s, top, verb + 1, privs_end).join(&format!(",\n{IN}")))
    };
    if let Some(on) = on {
        out.push_str(&format!("\n{IN}{}", s[on + 1..to].trim()));
    }
    out.push_str(&format!("\n{IN}{}", s[to + 1..].trim()));
    Some(out)
}

/// `EXEC proc @a = …, @b = …`: an argument a line.
fn exec(s: &str, top: &[bool]) -> Option<String> {
    let at = first(s, top, " @")?;
    Some(format!("{}\n{IN}{}", &s[..at], pieces(s, top, at + 1, s.len()).join(&format!(",\n{IN}"))))
}

/// `CREATE ROLE`, `CREATE USER` and `ALTER USER`: their options a group a line.
fn clauses(s: &str, top: &[bool]) -> Option<String> {
    const AT: [&str; 10] =
        [" PASSWORD '", " PASSWORD REQUIRE ", " NOSUPERUSER ", " IDENTIFIED ", " WITH MAX_", " ACCOUNT ", " ATTRIBUTE ", " CONNECTION LIMIT ", " VALID UNTIL ", " DEFAULT ROLE "];
    let mut cuts: Vec<usize> = AT.iter().filter_map(|p| first(s, top, p)).collect();
    if cuts.is_empty() {
        return None;
    }
    cuts.sort();
    let mut out = String::new();
    let mut start = 0;
    for i in cuts {
        out.push_str(s[start..i].trim());
        out.push('\n');
        out.push_str(IN);
        start = i + 1;
    }
    out.push_str(&s[start..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_stay() {
        assert_eq!(step(Some(Engine::Postgres), "GRANT USAGE ON SCHEMA \"public\" TO \"a\";"), "GRANT USAGE ON SCHEMA \"public\" TO \"a\";");
        assert_eq!(step(Some(Engine::MongoDb), "db.getSiblingDB(\"admin\").runCommand({\"dropRole\":\"u\"})"), "db.getSiblingDB(\"admin\").runCommand({\"dropRole\": \"u\"})");
    }

    #[test]
    fn quotes_and_parens_hold_their_commas() {
        let s = "GRANT SELECT (\"a, b\", \"c\"), INSERT (\"created_at\", \"id\", \"name\"), UPDATE (\"created_at\", \"x\") ON \"public\".\"users, too\" TO \"dataman_agent_a1b2c3d4\";";
        assert_eq!(
            sql(s, false),
            "GRANT\n  SELECT (\"a, b\", \"c\"),\n  INSERT (\"created_at\", \"id\", \"name\"),\n  UPDATE (\"created_at\", \"x\")\n  ON \"public\".\"users, too\"\n  TO \"dataman_agent_a1b2c3d4\";"
        );
    }

    #[test]
    fn a_lone_selector_keeps_its_spaces() {
        let w = words("ACL SETUSER u on \"(%R~k:* -@all)\" +ping");
        assert_eq!(w, ["ACL", "SETUSER", "u", "on", "\"(%R~k:* -@all)\"", "+ping"]);
    }
}
