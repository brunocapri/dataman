use std::ops::Range;

use super::shell::ParseError;
use crate::guard::WriteClass;

const READS: &[&str] = &[
    "GET", "MGET", "GETRANGE", "STRLEN", "EXISTS", "TYPE", "TTL", "PTTL", "EXPIRETIME", "KEYS", "SCAN", "RANDOMKEY", "DBSIZE",
    "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS", "HLEN", "HEXISTS", "HSTRLEN", "HSCAN", "HRANDFIELD", "LRANGE", "LINDEX",
    "LLEN", "LPOS", "SMEMBERS", "SISMEMBER", "SMISMEMBER", "SCARD", "SSCAN", "SRANDMEMBER", "SINTER", "SUNION", "SDIFF",
    "SINTERCARD", "ZRANGE", "ZREVRANGE", "ZRANGEBYSCORE", "ZREVRANGEBYSCORE", "ZRANGEBYLEX", "ZSCORE", "ZMSCORE", "ZRANK",
    "ZREVRANK", "ZCARD", "ZCOUNT", "ZLEXCOUNT", "ZSCAN", "ZRANDMEMBER", "XRANGE", "XREVRANGE", "XLEN", "XINFO", "PING",
    "ECHO", "TIME", "INFO", "LASTSAVE", "ROLE", "OBJECT", "MEMORY", "DUMP", "BITCOUNT", "BITPOS", "GETBIT", "PFCOUNT",
    "GEOPOS", "GEODIST", "GEOHASH", "GEOSEARCH", "GEORADIUS_RO", "SLOWLOG", "LOLWUT", "COMMAND", "SELECT", "JSON.GET",
    "JSON.TYPE", "JSON.MGET", "JSON.OBJKEYS", "JSON.STRLEN", "JSON.ARRLEN", "FT.SEARCH", "FT.INFO", "FT._LIST",
];

const SAFE_WRITES: &[&str] = &[
    "SET", "SETEX", "PSETEX", "SETNX", "MSET", "MSETNX", "APPEND", "SETRANGE", "INCR", "INCRBY", "INCRBYFLOAT", "DECR",
    "DECRBY", "GETSET", "GETDEL", "GETEX", "DEL", "UNLINK", "EXPIRE", "PEXPIRE", "EXPIREAT", "PEXPIREAT", "PERSIST",
    "RENAME", "RENAMENX", "COPY", "HSET", "HSETNX", "HMSET", "HDEL", "HINCRBY", "HINCRBYFLOAT", "LPUSH", "RPUSH", "LPUSHX",
    "RPUSHX", "LPOP", "RPOP", "LSET", "LREM", "LINSERT", "LTRIM", "LMOVE", "RPOPLPUSH", "SADD", "SREM", "SPOP", "SMOVE",
    "SINTERSTORE", "SUNIONSTORE", "SDIFFSTORE", "ZADD", "ZREM", "ZINCRBY", "ZPOPMIN", "ZPOPMAX", "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE", "ZREMRANGEBYLEX", "ZRANGESTORE", "XADD", "XDEL", "XTRIM", "SETBIT", "PFADD", "PFMERGE", "GEOADD",
    "JSON.SET", "JSON.DEL", "JSON.ARRAPPEND", "JSON.NUMINCRBY", "PUBLISH",
];

const REFUSED: &[&str] = &[
    "SUBSCRIBE", "PSUBSCRIBE", "SSUBSCRIBE", "MONITOR", "MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH", "BLPOP", "BRPOP",
    "BLMOVE", "BRPOPLPUSH", "BZPOPMIN", "BZPOPMAX", "BLMPOP", "BZMPOP", "XREAD", "XREADGROUP", "WAIT", "AUTH", "HELLO",
    "QUIT", "RESET", "CLIENT", "SYNC", "PSYNC", "SHUTDOWN",
];

const CLIENT_READS: &[&str] = &["CLIENT LIST", "CLIENT INFO", "CLIENT GETNAME", "CLIENT ID"];

const OTHERS: &[&str] = &["FLUSHDB", "FLUSHALL", "CONFIG", "EVAL", "EVALSHA", "SCRIPT", "FUNCTION", "SAVE", "BGSAVE", "SWAPDB", "MOVE", "DEBUG", "CLUSTER", "ACL", "LATENCY"];

pub fn commands() -> impl Iterator<Item = &'static str> {
    READS.iter().chain(SAFE_WRITES).chain(OTHERS).chain(REFUSED).copied()
}

pub fn is_command(word: &str) -> bool {
    let up = word.to_ascii_uppercase();
    commands().any(|c| c == up)
}

pub fn name(args: &[String]) -> String {
    let first = args.first().map(|a| a.to_ascii_uppercase()).unwrap_or_default();
    match (first.as_str(), args.get(1)) {
        ("CONFIG" | "SCRIPT" | "FUNCTION" | "CLUSTER" | "ACL" | "LATENCY" | "MEMORY" | "OBJECT" | "XINFO" | "XGROUP" | "CLIENT" | "SLOWLOG" | "COMMAND", Some(sub)) => {
            format!("{first} {}", sub.to_ascii_uppercase())
        }
        _ => first,
    }
}

pub fn class(args: &[String]) -> WriteClass {
    let n = name(args);
    let first = n.split(' ').next().unwrap_or("");
    let read_sub = CLIENT_READS.contains(&n.as_str()) || matches!(n.as_str(), "CONFIG GET" | "SCRIPT EXISTS" | "FUNCTION LIST" | "CLUSTER INFO" | "CLUSTER NODES" | "ACL WHOAMI" | "ACL LIST" | "LATENCY LATEST" | "MEMORY USAGE" | "MEMORY STATS" | "SLOWLOG GET" | "SLOWLOG LEN");
    if read_sub || READS.contains(&first) && !n.contains(' ') || matches!(first, "OBJECT" | "XINFO" | "COMMAND") {
        WriteClass::Read
    } else if SAFE_WRITES.contains(&first) {
        WriteClass::SafeWrite
    } else {
        WriteClass::Destructive { reasons: vec![format!("runs {n}")] }
    }
}

pub fn refusal(args: &[String]) -> Option<String> {
    let n = name(args);
    let first = n.split(' ').next().unwrap_or("");
    (REFUSED.contains(&first) && !CLIENT_READS.contains(&n.as_str())).then(|| format!("{n} is not supported here: it blocks or changes the connection's state"))
}

pub fn split(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with('#') && !t.starts_with("//") {
            let lead = line.len() - line.trim_start().len();
            out.push(at + lead..at + lead + t.len());
        }
        at += line.len();
    }
    out
}

pub fn parse(text: &str) -> Result<Vec<String>, ParseError> {
    Ok(parse_bytes(text)?.into_iter().map(|a| String::from_utf8_lossy(&a).into_owned()).collect())
}

pub fn parse_bytes(text: &str) -> Result<Vec<Vec<u8>>, ParseError> {
    let lead = text.len() - text.trim_start().len();
    let mut args = Vec::new();
    let mut chars = text.char_indices().peekable();
    let push = |arg: &mut Vec<u8>, c: char| {
        let mut b = [0; 4];
        arg.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    };
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut arg: Vec<u8> = Vec::new();
        match c {
            '"' | '\'' => {
                chars.next();
                let mut closed = false;
                while let Some((_, x)) = chars.next() {
                    match x {
                        _ if x == c => {
                            closed = true;
                            break;
                        }
                        '\\' if c == '"' => match chars.next().map(|(_, e)| e) {
                            Some('n') => arg.push(b'\n'),
                            Some('t') => arg.push(b'\t'),
                            Some('r') => arg.push(b'\r'),
                            Some('a') => arg.push(7),
                            Some('b') => arg.push(8),
                            Some('x') => {
                                let h: String = (0..2).filter_map(|_| chars.next().map(|(_, h)| h)).collect();
                                let b = u8::from_str_radix(&h, 16).map_err(|_| ParseError { message: "\\x takes two hex digits".into(), at: i })?;
                                arg.push(b);
                            }
                            Some(o) => push(&mut arg, o),
                            None => {}
                        },
                        '\\' if chars.peek().is_some_and(|(_, n)| *n == '\'') => {
                            chars.next();
                            arg.push(b'\'');
                        }
                        x => push(&mut arg, x),
                    }
                }
                if !closed {
                    return Err(ParseError { message: "a quoted argument is not closed".into(), at: i });
                }
            }
            _ => {
                while let Some(&(_, x)) = chars.peek() {
                    if x.is_whitespace() {
                        break;
                    }
                    push(&mut arg, x);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    if args.is_empty() {
        return Err(ParseError { message: "empty command".into(), at: lead });
    }
    Ok(args)
}

pub fn escape_bytes(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    let mut out = String::new();
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out
}

pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\') {
        return arg.to_string();
    }
    let mut s = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

pub fn view_command(key: &str, kind: &str) -> String {
    let k = quote(key);
    match kind {
        "hash" => format!("HGETALL {k}"),
        "list" => format!("LRANGE {k} 0 -1"),
        "set" => format!("SMEMBERS {k}"),
        "zset" => format!("ZRANGE {k} 0 -1 WITHSCORES"),
        "stream" => format!("XRANGE {k} - + COUNT 100"),
        "ReJSON-RL" => format!("JSON.GET {k}"),
        _ => format!("GET {k}"),
    }
}

pub fn len_command(key: &str, kind: &str) -> String {
    let k = quote(key);
    match kind {
        "hash" => format!("HLEN {k}"),
        "list" => format!("LLEN {k}"),
        "set" => format!("SCARD {k}"),
        "zset" => format!("ZCARD {k}"),
        "stream" => format!("XLEN {k}"),
        _ => format!("STRLEN {k}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commands_as_redis_cli() {
        assert_eq!(parse(r#"SET "my key" 'it\'s' "a\tb\x41""#).unwrap(), ["SET", "my key", "it's", "a\tbA"]);
        assert_eq!(parse_bytes(r#"SET b "\x00\xffé""#).unwrap()[2], vec![0, 0xff, 0xc3, 0xa9]);
        let shown = escape_bytes(&[0, 0xff, b'a', b'"']);
        assert_eq!(shown, r#"\x00\xffa\""#);
        assert_eq!(parse_bytes(&format!("SET b \"{shown}\"")).unwrap()[2], vec![0, 0xff, b'a', b'"']);
        assert_eq!(escape_bytes("olá".as_bytes()), "olá");
        assert!(refusal(&parse("CLIENT LIST").unwrap()).is_none() && class(&parse("client list").unwrap()) == WriteClass::Read);
        assert!(refusal(&parse("CLIENT KILL ID 1").unwrap()).is_some());
        assert_eq!(parse("  hgetall   user:1 ").unwrap(), ["hgetall", "user:1"]);
        assert!(parse("GET \"open").unwrap_err().message.contains("closed"));
        let t = "# notes\nGET a\n\n  HGETALL b  \n// x\nDEL c";
        let parts: Vec<&str> = split(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(parts, ["GET a", "HGETALL b", "DEL c"]);
        assert_eq!(quote("a b\"c"), "\"a b\\\"c\"");
        assert_eq!(parse(&format!("SET {} x", quote("a b\"c"))).unwrap()[1], "a b\"c");
        assert_eq!(view_command("user:1", "zset"), "ZRANGE user:1 0 -1 WITHSCORES");
    }

    #[test]
    fn what_each_command_does() {
        let c = |s: &str| class(&parse(s).unwrap());
        assert_eq!(c("get a"), WriteClass::Read);
        assert_eq!(c("CONFIG GET maxmemory"), WriteClass::Read);
        assert_eq!(c("MEMORY USAGE a"), WriteClass::Read);
        assert_eq!(c("hset h f v"), WriteClass::SafeWrite);
        assert_eq!(c("DEL a b"), WriteClass::SafeWrite);
        assert!(c("FLUSHDB").is_destructive());
        assert!(c("CONFIG SET maxmemory 1").is_destructive());
        assert!(c("EVAL \"return 1\" 0").is_destructive());
        assert_eq!(c("SLOWLOG GET 10"), WriteClass::Read);
        assert_eq!(c("COMMAND DOCS get"), WriteClass::Read);
        assert!(c("SLOWLOG RESET").is_destructive());
        for cmd in ["FUNCTION LOAD x", "MODULE LOAD /tmp/x.so", "DEBUG SLEEP 0", "MIGRATE h 1 k 0 1", "REPLICAOF h 1", "SLAVEOF h 1", "FLUSHALL", "SCRIPT FLUSH", "EVALSHA x 0", "\"\\x46LUSHALL\"", "flushall", "ACL SETUSER x on", "LATENCY RESET", "MEMORY PURGE", "SORT k STORE d", "BGREWRITEAOF", "FCALL f 0"] {
            assert!(c(cmd).is_destructive(), "{cmd}");
        }
        assert!(refusal(&parse("SUBSCRIBE ch").unwrap()).is_some());
        assert!(refusal(&parse("GET a").unwrap()).is_none());
    }
}
