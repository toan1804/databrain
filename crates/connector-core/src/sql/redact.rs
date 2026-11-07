/// Words followed by a secret (optionally after `=` or `(`): `PASSWORD 'x'`,
/// `PASSWORD = 'x'`, `PASSWORD('x')`, `SECRET 'x'`, `AWS_SECRET_KEY = 'x'`…
const SECRET_WORDS: &[&str] = &[
    "password",
    "secret",
    "secret_key",
    "aws_secret_key",
    "aws_secret_access_key",
    "aws_token",
    "session_token",
    "account_key",
    "azure_sas_token",
    "sas_token",
    "master_key",
    "client_secret",
    "access_token",
    "refresh_token",
    "private_key",
    "private_key_passphrase",
    "connection_string",
    "token",
];

/// `sql` with passwords and keys replaced by `***`, for what DataBrain keeps
/// (query history, job run logs, the AI audit log):
/// `CREATE USER u IDENTIFIED BY pw`, `IDENTIFIED WITH plugin BY 'pw'`,
/// `ALTER ROLE r PASSWORD 'pw'`, `WITH PASSWORD = 'pw'`, `SET PASSWORD =
/// PASSWORD('pw')`, `CREATE SECRET (… SECRET 'k')`, `CREDENTIALS = (AWS_SECRET_KEY = 'k')`.
/// Only string literals (and the bare word after `IDENTIFIED BY`) change.
pub fn redact_secrets(sql: &str) -> String {
    let b = sql.as_bytes();
    let n = b.len();
    let word_at = |i: usize| -> Option<(usize, usize)> {
        if i >= n || !(b[i].is_ascii_alphabetic() || b[i] == b'_') || (i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            return None;
        }
        let mut j = i;
        while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        Some((i, j))
    };
    let skip_ws = |mut i: usize| {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    // End of the quoted literal starting at `i` (doubled quotes escape).
    let literal_end = |i: usize| -> usize {
        let q = b[i];
        let mut j = i + 1;
        while j < n {
            if b[j] == b'\\' && q == b'\'' && j + 1 < n {
                j += 2;
                continue;
            }
            if b[j] == q {
                if j + 1 < n && b[j + 1] == q {
                    j += 2;
                    continue;
                }
                return j + 1;
            }
            j += 1;
        }
        n
    };
    let lower = |s: usize, e: usize| sql[s..e].to_ascii_lowercase();
    // Spans (start, end, quote) to replace.
    let mut cut: Vec<(usize, usize, Option<u8>)> = Vec::new();
    let mut i = 0;
    while i < n {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                i = literal_end(i);
                continue;
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = sql[i + 2..].find("*/").map(|k| i + 4 + k).unwrap_or(n);
                continue;
            }
            _ => {}
        }
        let Some((s, e)) = word_at(i) else {
            i += 1;
            continue;
        };
        let w = lower(s, e);
        let mut value_at = None;
        let mut bare_ok = false;
        if w == "identified" {
            // IDENTIFIED BY x | IDENTIFIED WITH plugin (BY|AS) x
            let mut j = skip_ws(e);
            if let Some((s2, e2)) = word_at(j) {
                match lower(s2, e2).as_str() {
                    "by" => {
                        value_at = Some(skip_ws(e2));
                        bare_ok = true;
                    }
                    "with" => {
                        j = skip_ws(e2);
                        if let Some((_, e3)) = word_at(j).or_else(|| (j < n && matches!(b[j], b'\'' | b'"' | b'`')).then(|| (j, literal_end(j)))) {
                            j = skip_ws(e3);
                            if let Some((s4, e4)) = word_at(j)
                                && matches!(lower(s4, e4).as_str(), "by" | "as")
                            {
                                value_at = Some(skip_ws(e4));
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else if SECRET_WORDS.contains(&w.as_str()) {
            let mut j = skip_ws(e);
            if j < n && b[j] == b'=' {
                j = skip_ws(j + 1);
            }
            // SET PASSWORD = PASSWORD('x')
            if let Some((s2, e2)) = word_at(j).filter(|(s2, e2)| lower(*s2, *e2) == "password") {
                let _ = s2;
                j = skip_ws(e2);
            }
            if j < n && b[j] == b'(' {
                j = skip_ws(j + 1);
            }
            value_at = Some(j);
        }
        if let Some(v) = value_at.filter(|v| *v < n) {
            if matches!(b[v], b'\'' | b'"') {
                let end = literal_end(v);
                cut.push((v, end, Some(b[v])));
                i = end;
                continue;
            }
            if bare_ok && let Some((s2, e2)) = word_at(v) {
                cut.push((s2, e2, None));
                i = e2;
                continue;
            }
        }
        i = e;
    }
    if cut.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (s, e, q) in cut {
        out.push_str(&sql[last..s]);
        match q {
            Some(q) => {
                out.push(q as char);
                out.push_str("***");
                out.push(q as char);
            }
            None => out.push_str("***"),
        }
        last = e;
    }
    out.push_str(&sql[last..]);
    out
}

/// `text` with the password of every `scheme://user:password@host` in it
/// replaced by `***` (connection strings pasted into a question, `ATTACH
/// 'postgres://u:pw@h'`). The user stays; a URL without a password is kept.
pub fn redact_url_passwords(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let start = i + 3;
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // The authority: up to the path, query, fragment or the end of the URL in the text.
        let end = rest.find(|c: char| matches!(c, '/' | '?' | '#' | '\'' | '"' | '`' | '<' | '>' | '(' | ')') || c.is_whitespace()).unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@')
            && let Some(colon) = rest[..at].find(':')
        {
            out.push_str(&rest[..colon + 1]);
            out.push_str("***");
            rest = &rest[at..];
            continue;
        }
    }
    out.push_str(rest);
    out
}

/// What DataBrain keeps of text that may quote SQL or connection strings
/// (query history, job logs, AI transcripts and audit): [`redact_secrets`],
/// [`redact_url_passwords`] and [`redact_key_values`].
/// Text in double quotes is redacted too: error messages quote the SQL they
/// failed on that way (`syntax error at or near "PASSWORD 'pw'"`).
pub fn redact_text(text: &str) -> String {
    let sql = redact_secrets(text);
    let quoted = sql.split('"').enumerate().map(|(i, part)| if i % 2 == 1 { redact_secrets(part) } else { part.to_string() }).collect::<Vec<_>>().join("\"");
    redact_key_values(&redact_url_passwords(&quoted))
}

/// Is `name` (lower case, without a leading `@`) the name of a secret?
/// `password`, `pwd`, `master_password`, `rmtpassword`, `client_secret`,
/// `api_key`… A suffix is enough: drivers and procedures prefix them.
fn secret_name(name: &str) -> bool {
    const ENDS: &[&str] =
        &["password", "passwd", "pwd", "secret", "secret_key", "secretkey", "access_key", "accesskey", "api_key", "apikey", "_token", "passphrase"];
    SECRET_WORDS.contains(&name) || ENDS.iter().any(|e| name.ends_with(e))
}

/// `text` with the value of every `name = value` / `name: value` whose name
/// is a secret's ([`secret_name`]) replaced by `***`:
/// `MASTER_PASSWORD='pw'`, `@rmtpassword = N'pw'`, `{user: "u", pwd: "pw"}`,
/// `"password": "pw"`, and inside strings, where connection strings put
/// bare values: `'Driver=…;UID=u;PWD=pw;'`, `'host=h password=pw'`,
/// `'jdbc:…;password=pw'`, `?user=u&password=pw`. Outside strings and
/// comments only quoted values change, so `where password = hash` stays.
pub fn redact_key_values(text: &str) -> String {
    let b = text.as_bytes();
    let n = b.len();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let skip_ws = |mut i: usize| {
        while i < n && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        i
    };
    // End (exclusive, after the closing quote) of a quoted value at `i`.
    let quoted_end = |i: usize| -> usize {
        let q = b[i];
        let mut j = i + 1;
        while j < n {
            if b[j] == q {
                if j + 1 < n && b[j + 1] == q {
                    j += 2;
                    continue;
                }
                return j + 1;
            }
            if b[j] == b'\\' && j + 1 < n {
                j += 2;
                continue;
            }
            j += 1;
        }
        n
    };
    let mut cut: Vec<(usize, usize)> = Vec::new();
    // The quote of the SQL string we are in, or b'-' / b'*' in a comment.
    let mut inside: Option<u8> = None;
    let mut i = 0;
    while i < n {
        let c = b[i];
        match inside {
            None if c == b'-' && i + 1 < n && b[i + 1] == b'-' => {
                inside = Some(b'-');
                i += 2;
                continue;
            }
            None if c == b'/' && i + 1 < n && b[i + 1] == b'*' => {
                inside = Some(b'*');
                i += 2;
                continue;
            }
            None if c == b'\'' => {
                inside = Some(c);
                i += 1;
                continue;
            }
            Some(b'-') if c == b'\n' => inside = None,
            Some(b'*') if c == b'*' && i + 1 < n && b[i + 1] == b'/' => {
                inside = None;
                i += 2;
                continue;
            }
            Some(q) if q == c && !matches!(q, b'-' | b'*') => {
                if i + 1 < n && b[i + 1] == q {
                    i += 2;
                    continue;
                }
                inside = None;
                i += 1;
                continue;
            }
            _ => {}
        }
        // A name: letters after a non-identifier character (`@` allowed in front).
        if !(c.is_ascii_alphabetic() || c == b'_') || (i > 0 && ident(b[i - 1])) {
            i += 1;
            continue;
        }
        let mut e = i;
        while e < n && ident(b[e]) {
            e += 1;
        }
        let name = text[i..e].to_ascii_lowercase();
        if !secret_name(&name) {
            i = e;
            continue;
        }
        // `"password": …` / `'pwd': …`: the name's own closing quote.
        let mut j = e;
        let mut name_quote = None;
        if j < n && matches!(b[j], b'"' | b'\'') && i > 0 && b[i - 1] == b[j] {
            name_quote = Some(b[j]);
            j += 1;
        }
        j = skip_ws(j);
        // `=`, `:`, `:=` or `=>` (named arguments), not `==` or `::` (a cast).
        let sep = j < n
            && match b[j] {
                b'=' => !(j + 1 < n && b[j + 1] == b'='),
                b':' => !(j + 1 < n && b[j + 1] == b':'),
                _ => false,
            };
        if !sep {
            i = e;
            continue;
        }
        j += if j + 1 < n && matches!((b[j], b[j + 1]), (b':', b'=') | (b'=', b'>')) { 2 } else { 1 };
        j = skip_ws(j);
        // N'…' (SQL Server), E'…' (Postgres).
        if j + 1 < n && matches!(b[j], b'N' | b'n' | b'E' | b'e') && b[j + 1] == b'\'' {
            j += 1;
        }
        if j >= n {
            break;
        }
        // The quote that closes the string we're in ends a bare value there.
        let in_string = match inside {
            Some(q) if name_quote == Some(q) => None,
            other => other,
        };
        if matches!(b[j], b'\'' | b'"' | b'`') && Some(b[j]) != in_string {
            let end = quoted_end(j);
            // Unterminated: to the end.
            let close = if end == n && (end == j + 1 || b[n - 1] != b[j]) { n } else { end - 1 };
            if close > j + 1 {
                cut.push((j + 1, close));
            }
            i = end;
            continue;
        }
        if in_string.is_none() {
            i = e;
            continue;
        }
        // A bare value in a string or comment: to the next separator (`{…}` whole).
        let closing = in_string.filter(|q| !matches!(q, b'-' | b'*'));
        let end = if b[j] == b'{' {
            text[j..].find('}').map(|k| j + k + 1).unwrap_or(n)
        } else {
            let mut k = j;
            while k < n && !matches!(b[k], b';' | b',' | b'&' | b')' | b'}' | b' ' | b'\t' | b'\r' | b'\n') && Some(b[k]) != closing {
                k += 1;
            }
            k
        };
        if end > j && &text[j..end] != "***" {
            cut.push((j, end));
        }
        i = end;
    }
    if cut.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (s, e) in cut {
        if s < last {
            continue;
        }
        out.push_str(&text[last..s]);
        out.push_str("***");
        last = e;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn redacts_passwords_and_keys_in_sql() {
        for (sql, want) in [
            ("CREATE USER u IDENTIFIED BY hunter2", "CREATE USER u IDENTIFIED BY ***"),
            ("create user u identified by 'p@ss'", "create user u identified by '***'"),
            ("CREATE USER u IDENTIFIED WITH mysql_native_password BY 'pw'", "CREATE USER u IDENTIFIED WITH mysql_native_password BY '***'"),
            ("ALTER USER u IDENTIFIED WITH 'caching_sha2_password' AS 'hash'", "ALTER USER u IDENTIFIED WITH 'caching_sha2_password' AS '***'"),
            ("ALTER ROLE r WITH PASSWORD 'pw'", "ALTER ROLE r WITH PASSWORD '***'"),
            ("CREATE LOGIN l WITH PASSWORD = 'pw'", "CREATE LOGIN l WITH PASSWORD = '***'"),
            ("SET PASSWORD = PASSWORD('pw')", "SET PASSWORD = PASSWORD('***')"),
            ("CREATE SECRET s (TYPE s3, KEY_ID 'id', SECRET 'k')", "CREATE SECRET s (TYPE s3, KEY_ID 'id', SECRET '***')"),
            (
                "COPY INTO t FROM 's3://b' CREDENTIALS = (AWS_KEY_ID = 'id' AWS_SECRET_KEY = 'k')",
                "COPY INTO t FROM 's3://b' CREDENTIALS = (AWS_KEY_ID = 'id' AWS_SECRET_KEY = '***')",
            ),
            ("ALTER USER u SET RSA_PUBLIC_KEY = 'pub', PRIVATE_KEY = \"k\"", "ALTER USER u SET RSA_PUBLIC_KEY = 'pub', PRIVATE_KEY = \"***\""),
            ("select 'it''s' as password", "select 'it''s' as password"),
        ] {
            assert_eq!(redact_secrets(sql), want, "{sql}");
        }
        // Words inside literals and comments, and other literals, are left alone.
        let keep = "select 'password ''x''' from t -- password 'y'\n/* identified by z */ where name = 'secret'";
        assert_eq!(redact_secrets(keep), keep);
        assert_eq!(redact_secrets("select token from sessions"), "select token from sessions", "a column named like a secret");
        assert_eq!(redact_secrets("pw = 'x'"), "pw = 'x'");
        // Unterminated literal: redacted to the end.
        assert_eq!(redact_secrets("alter role r password 'unfinished"), "alter role r password '***'");
    }

    /// What query history and job logs keep goes through [`redact_text`]: the
    /// forms the SQL-only pass missed.
    #[test]
    fn redacts_key_value_passwords() {
        for (text, want) in [
            ("ATTACH 'postgres://u:pw@h/db' AS p (TYPE postgres)", "ATTACH 'postgres://u:***@h/db' AS p (TYPE postgres)"),
            (
                "CHANGE MASTER TO MASTER_HOST='h', MASTER_USER='r', MASTER_PASSWORD='pw'",
                "CHANGE MASTER TO MASTER_HOST='h', MASTER_USER='r', MASTER_PASSWORD='***'",
            ),
            ("CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = 'pw'", "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = '***'"),
            ("db.createUser({user: \"app\", pwd: \"pw\", roles: []})", "db.createUser({user: \"app\", pwd: \"***\", roles: []})"),
            (
                "EXEC sp_addlinkedsrvlogin @rmtsrvname = 'S', @useself = 'false', @rmtuser = 'u', @rmtpassword = N'pw'",
                "EXEC sp_addlinkedsrvlogin @rmtsrvname = 'S', @useself = 'false', @rmtuser = 'u', @rmtpassword = N'***'",
            ),
            (
                "select * from openrowset('MSDASQL', 'Driver={SQL Server};Server=h;UID=u;PWD=pw;', 'select 1')",
                "select * from openrowset('MSDASQL', 'Driver={SQL Server};Server=h;UID=u;PWD=***;', 'select 1')",
            ),
            ("select dblink_connect('host=h user=u password=pw dbname=d')", "select dblink_connect('host=h user=u password=*** dbname=d')"),
            ("ATTACH 'host=h password=pw' AS pg (TYPE postgres)", "ATTACH 'host=h password=***' AS pg (TYPE postgres)"),
            ("select * from t where u = 'jdbc:sqlserver://h;user=u;password=pw'", "select * from t where u = 'jdbc:sqlserver://h;user=u;password=***'"),
            ("load 'https://h/x?user=u&password=pw&x=1'", "load 'https://h/x?user=u&password=***&x=1'"),
            ("select '{\"password\": \"pw\", \"user\": \"u\"}'::json", "select '{\"password\": \"***\", \"user\": \"u\"}'::json"),
            ("select connect(password => 'pw')", "select connect(password => '***')"),
            ("-- api_key=abc123 for staging\nselect 1", "-- api_key=*** for staging\nselect 1"),
            (
                "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (client_secret 'k')",
                "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (client_secret '***')",
            ),
        ] {
            assert_eq!(redact_text(text), want, "{text}");
        }
        // Not secrets: columns compared to columns, casts, other names and values.
        for keep in [
            "select * from users where password = password_hash",
            "select password::text from t",
            "select 'host=h user=u dbname=d'",
            "select password_hash, passwordless from t where pwd_changed = true",
            "select 'a' == 'b'",
        ] {
            assert_eq!(redact_text(keep), keep);
        }
        // Idempotent.
        let once = redact_text("ATTACH 'host=h password=pw' AS pg; CHANGE MASTER TO MASTER_PASSWORD='x'");
        assert_eq!(redact_text(&once), once);
    }

    #[test]
    fn redacts_url_passwords_in_text() {
        for (text, want) in [
            ("postgres://ann:s3cret@db.example.com:5432/app", "postgres://ann:***@db.example.com:5432/app"),
            (
                "connect to mongodb+srv://u:p%40ss@cluster0.x.net/?retryWrites=true please",
                "connect to mongodb+srv://u:***@cluster0.x.net/?retryWrites=true please",
            ),
            ("ATTACH 'mysql://root:pw@localhost/shop' AS m", "ATTACH 'mysql://root:***@localhost/shop' AS m"),
            ("two: a://x:1@h and b://y:2@k", "two: a://x:***@h and b://y:***@k"),
            ("user:p@ss with @ in it: s://u:p@ss@host/db", "user:p@ss with @ in it: s://u:***@host/db"),
        ] {
            assert_eq!(redact_url_passwords(text), want, "{text}");
        }
        for keep in ["https://example.com:8443/a@b", "s3://bucket/key", "neo4j://user@host", "no url here: a:b@c", "x://"] {
            assert_eq!(redact_url_passwords(keep), keep);
        }
        assert_eq!(redact_text("create user u identified by 'pw'; -- from postgres://a:b@h"), "create user u identified by '***'; -- from postgres://a:***@h");
        assert_eq!(redact_text("syntax error at or near \"PASSWORD 'pw'\""), "syntax error at or near \"PASSWORD '***'\"");
        assert_eq!(redact_text("alter role r password \"pw\""), "alter role r password \"***\"");
        let plain = "column \"a\" of \"t\" says \"hello\"";
        assert_eq!(redact_text(plain), plain);
    }
}
