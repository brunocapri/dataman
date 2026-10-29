//! The agent login's password and the forms each engine takes it in, so that the password itself
//! never travels in a statement a server could log: PostgreSQL's SCRAM verifier, MySQL's
//! caching_sha2_password hash, MariaDB's native hash, SQL Server's hashed password, Redis'
//! SHA-256 and the bcrypt hash of Elasticsearch and OpenSearch. MongoDB alone takes the
//! password (createUser hashes it on the server, which redacts it from its logs).

use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};

/// Letters and digits only: nothing to quote in a statement, a URL or a header.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub const PASSWORD_LEN: usize = 40;

/// `n` random bytes from the system's CSPRNG.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    SystemRandom::new().fill(&mut b).expect("the system's random source");
    b
}

/// A new password: 40 letters and digits from the system's CSPRNG (about 238 bits).
pub fn password() -> String {
    random_text(PASSWORD_LEN)
}

fn random_text(n: usize) -> String {
    let mut out = String::with_capacity(n);
    // Rejection sampling: 62 does not divide 256, the bytes past 247 would weigh more.
    while out.len() < n {
        for b in random_bytes(n * 2) {
            if (b as usize) < 248 && out.len() < n {
                out.push(ALPHABET[b as usize % 62] as char);
            }
        }
    }
    out
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, b).as_ref())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// PostgreSQL's SCRAM-SHA-256 verifier: stored as it is, so the password is not in the
/// statement (`log_statement = ddl` would log it).
pub fn pg_scram(password: &str) -> String {
    postgres_protocol::password::scram_sha_256(password.as_bytes())
}

/// Redis' ACL takes `#<sha256>`.
pub fn redis_sha(password: &str) -> String {
    sha256_hex(password.as_bytes())
}

/// SQL Server's hashed password (2012 and later): `0x0200`, a 4-byte salt, and SHA-512 of the
/// password in UTF-16LE followed by the salt.
pub fn mssql_hash(password: &str) -> String {
    mssql_hash_salted(password, &random_bytes(4))
}

pub(crate) fn mssql_hash_salted(password: &str, salt: &[u8]) -> String {
    let mut m: Vec<u8> = password.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    m.extend_from_slice(salt);
    let h = digest::digest(&digest::SHA512, &m);
    format!("0x0200{}{}", hex(salt), hex(h.as_ref())).to_uppercase().replacen("0X", "0x", 1)
}

/// MariaDB's `mysql_native_password` hash: `*` and SHA1(SHA1(password)) in upper hex.
pub fn mariadb_native(password: &str) -> String {
    let once = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, password.as_bytes());
    let twice = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, once.as_ref());
    format!("*{}", hex(twice.as_ref()).to_uppercase())
}

/// MySQL's `caching_sha2_password` hash, as `IDENTIFIED WITH caching_sha2_password AS '…'`
/// takes it: `$A$005$`, a 20-character salt and SHA-256 crypt of 5000 rounds.
pub fn mysql_caching_sha2(password: &str) -> String {
    mysql_caching_sha2_salted(password, &random_text(20))
}

pub(crate) fn mysql_caching_sha2_salted(password: &str, salt: &str) -> String {
    format!("$A$005${salt}{}", sha256_crypt(password.as_bytes(), salt.as_bytes(), 5000))
}

/// Elasticsearch's and OpenSearch's bcrypt (`$2a$`, cost 10).
pub fn bcrypt(password: &str) -> String {
    let salt: [u8; 16] = random_bytes(16).try_into().expect("16 bytes");
    bcrypt::hash_with_salt(password, 10, salt).expect("bcrypt of a short password").format_for_version(bcrypt::Version::TwoA)
}

fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    let mut c = digest::Context::new(&digest::SHA256);
    for p in parts {
        c.update(p);
    }
    c.finish().as_ref().to_vec()
}

/// Ulrich Drepper's SHA-256 crypt, without its 16-byte cap on the salt (MySQL's takes 20).
fn sha256_crypt(p: &[u8], s: &[u8], rounds: u32) -> String {
    let b = sha256(&[p, s, p]);
    let mut a = digest::Context::new(&digest::SHA256);
    a.update(p);
    a.update(s);
    let mut n = p.len();
    while n > 32 {
        a.update(&b);
        n -= 32;
    }
    a.update(&b[..n]);
    let mut n = p.len();
    while n > 0 {
        if n & 1 == 1 {
            a.update(&b);
        } else {
            a.update(p);
        }
        n >>= 1;
    }
    let a = a.finish().as_ref().to_vec();
    let dp = {
        let mut c = digest::Context::new(&digest::SHA256);
        for _ in 0..p.len() {
            c.update(p);
        }
        c.finish().as_ref().to_vec()
    };
    let p_seq: Vec<u8> = dp.iter().cycle().take(p.len()).copied().collect();
    let ds = {
        let mut c = digest::Context::new(&digest::SHA256);
        for _ in 0..(16 + a[0] as usize) {
            c.update(s);
        }
        c.finish().as_ref().to_vec()
    };
    let s_seq: Vec<u8> = ds.iter().cycle().take(s.len()).copied().collect();
    let mut c = a;
    for i in 0..rounds {
        let mut x = digest::Context::new(&digest::SHA256);
        if i & 1 == 1 {
            x.update(&p_seq);
        } else {
            x.update(&c);
        }
        if i % 3 != 0 {
            x.update(&s_seq);
        }
        if i % 7 != 0 {
            x.update(&p_seq);
        }
        if i & 1 == 1 {
            x.update(&c);
        } else {
            x.update(&p_seq);
        }
        c = x.finish().as_ref().to_vec();
    }
    const ITOA: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = String::new();
    let mut put = |b2: u8, b1: u8, b0: u8, n: usize| {
        let mut w = ((b2 as u32) << 16) | ((b1 as u32) << 8) | b0 as u32;
        for _ in 0..n {
            out.push(ITOA[(w & 0x3f) as usize] as char);
            w >>= 6;
        }
    };
    for (x, y, z) in [(0, 10, 20), (21, 1, 11), (12, 22, 2), (3, 13, 23), (24, 4, 14), (15, 25, 5), (6, 16, 26), (27, 7, 17), (18, 28, 8), (9, 19, 29)] {
        put(c[x], c[y], c[z], 4);
    }
    put(0, c[31], c[30], 3);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_long_random_and_plain() {
        let a = password();
        let b = password();
        assert_eq!(a.len(), PASSWORD_LEN);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}");
    }

    #[test]
    fn sha256_crypt_matches_the_reference() {
        // As `openssl passwd -5` gives them: the default 5000 rounds, and 10000.
        assert_eq!(sha256_crypt(b"Hello world!", b"saltstring", 5000), "5B8vYYiY.CVt1RlTTf8KbXBH3hsxY/GNooZaBBGWEc5");
        assert_eq!(sha256_crypt(b"pw", b"abcdefghijklmnop", 5000), "gu.71QnGu.GrGbdJF.mj20x/mWJF1VliZRmeYNpAtDB");
        // And with 10000 rounds (`$5$rounds=10000$saltstringsaltst`, the salt cut at 16).
        assert_eq!(sha256_crypt(b"Hello world!", b"saltstringsaltst", 10000), "3xv.VbSHBb41AL9AvLeujZkZRBAwqFMz2.opqey6IcA");
        let h = mysql_caching_sha2_salted("pw", "abcdefghijklmnopqrst");
        assert!(h.starts_with("$A$005$abcdefghijklmnopqrst") && h.len() == 7 + 20 + 43, "{h}");
    }

    #[test]
    fn engine_forms() {
        assert!(pg_scram("x").starts_with("SCRAM-SHA-256$4096:"));
        assert_eq!(mariadb_native("password"), "*2470C0C06DEE42FD1618BB99005ADCA2EC9D1E19");
        assert_eq!(redis_sha("foobar"), "c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2");
        let h = mssql_hash_salted("pw", &[1, 2, 3, 4]);
        assert!(h.starts_with("0x020001020304") && h.len() == 2 + 4 + 8 + 128, "{h}");
        let b = bcrypt("secret");
        assert!(b.starts_with("$2a$10$") && ::bcrypt::verify("secret", &b).unwrap());
    }
}
