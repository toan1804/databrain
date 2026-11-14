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
    "secret_access_key",
    "secret_string",
    "bearer_token",
    "encryption_key",
];

/// Functions and procedures that take a secret by position: the name
/// (lower case), the 0-based arguments that are secrets, and how many
/// arguments the call needs before they are (`s3(url)` and `s3(url,
/// format)` have none). Only quoted arguments change; a call with a named
/// argument (`@rmtpassword = …`) is left to [`redact_key_values`].
const POSITIONAL_SECRETS: &[(&str, &[usize], usize)] = &[
    // ClickHouse table functions: (url, key id, secret, …).
    ("s3", &[2], 3),
    ("gcs", &[2], 3),
    ("oss", &[2], 3),
    ("cosn", &[2], 3),
    ("s3cluster", &[3], 4),
    // (connection string, container, blob, account name, account key, …).
    ("azureblobstorage", &[4], 5),
    ("azureblobstoragecluster", &[5], 6),
    // (host, db, table, user, password, …).
    ("mysql", &[4], 5),
    ("postgresql", &[4], 5),
    ("mongodb", &[4], 5),
    // (addresses, db.table, user, password) or (addresses, db, table, user,
    // password): both places hidden when there are five.
    ("remote", &[3, 4], 4),
    ("remotesecure", &[3, 4], 4),
    // Postgres pgcrypto and MySQL: (data, key/password, …).
    ("pgp_sym_encrypt", &[1], 2),
    ("pgp_sym_encrypt_bytea", &[1], 2),
    ("pgp_sym_decrypt", &[1], 2),
    ("pgp_sym_decrypt_bytea", &[1], 2),
    ("aes_encrypt", &[1], 2),
    ("aes_decrypt", &[1], 2),
    // SQL Server procedures.
    ("sp_addlinkedsrvlogin", &[4], 5),
    ("sp_addlogin", &[1], 2),
    ("sp_password", &[0, 1], 2),
    ("sp_setapprole", &[1], 2),
];

/// `sql` with passwords and keys replaced by `***`, for what DataBrain keeps
/// (query history, job run logs, the AI audit log):
/// `CREATE USER u IDENTIFIED BY pw`, `IDENTIFIED WITH plugin BY 'pw'`,
/// `ALTER ROLE r PASSWORD 'pw'`, `WITH PASSWORD = 'pw'`, `SET PASSWORD =
/// PASSWORD('pw')`, `CREATE SECRET (… SECRET 'k')`, `CREDENTIALS = (AWS_SECRET_KEY = 'k')`.
/// Only string literals (and the bare word after `IDENTIFIED BY`) change.
pub fn redact_secrets(sql: &str) -> String {
    // A string ending in a backslash (`'C:\'`) ends there on most servers
    // but runs on where `\'` escapes (MySQL); `$$it's$$` is one string on
    // Postgres but a quote on others. A secret after one is found by one
    // reading only: all are read, and what any finds is hidden.
    let mut cut = Vec::new();
    for reading in Reading::ALL {
        cut.extend(secret_spans(sql, reading, 0));
    }
    if cut.is_empty() {
        return sql.to_string();
    }
    // Overlapping spans (found twice, or by the two readings with
    // different ends) become one that covers both.
    cut.sort_by_key(|c| (c.0, std::cmp::Reverse(c.1)));
    let mut merged: Vec<Span> = Vec::with_capacity(cut.len());
    for (s, e, q) in cut {
        match merged.last_mut() {
            Some(m) if s < m.1 => m.1 = m.1.max(e),
            _ => merged.push((s, e, q)),
        }
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (s, e, q) in merged {
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

/// A secret to hide: its start and end, and its quote (`'***'`) or none
/// (`***`, a bare word or the text of a dollar-quoted string).
type Span = (usize, usize, Option<u8>);

/// How quoted text is read: `\'` in `'…'` and `\"` in `"…"` as escaped
/// quotes or not (`backslash`: MySQL, Postgres `E'…'`), and `$$…$$` /
/// `$tag$…$tag$` as quoted text or not (`dollar`: Postgres, DuckDB, Snowflake).
#[derive(Debug, Clone, Copy)]
struct Reading {
    backslash: bool,
    dollar: bool,
    /// `[name]` as a quoted name (SQL Server, SQLite) and `q'[…]'` as
    /// quoted text (Oracle): `[it's]` and `q'[it's]'` open no string.
    brackets: bool,
    /// `#` starts a comment to the end of the line (MySQL, ClickHouse):
    /// `# don't` opens no string.
    hash: bool,
}

impl Reading {
    const fn new(backslash: bool, dollar: bool) -> Self {
        Self { backslash, dollar, brackets: false, hash: false }
    }

    /// Every reading. The last two are for the servers their syntax
    /// belongs to (neither reads dollar quotes; SQL Server and Oracle no
    /// backslash escapes, MySQL and ClickHouse do); they are extra
    /// readings, so `a[1]`, `#temp` and `a # b` are still read as on the
    /// other servers by the first four.
    const ALL: [Self; 6] = [
        Self::new(false, true),
        Self::new(true, true),
        Self::new(false, false),
        Self::new(true, false),
        Self { brackets: true, ..Self::new(false, false) },
        Self { hash: true, ..Self::new(true, false) },
    ];
}

/// What a token is, as redaction reads SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Spaces; `newline` when one of them ends a line.
    Space {
        newline: bool,
    },
    Comment,
    /// A name or keyword: ASCII letters, digits and `_`, not led by a digit.
    Word,
    /// Quoted text, `'…'`, `"…"` or `` `…` ``, with `E`/`N` before `'…'`.
    /// `open` is its opening quote (after the prefix).
    Quoted {
        quote: u8,
        open: usize,
    },
    /// `$$…$$` / `$tag$…$tag$`: where its text starts and ends.
    Dollar {
        body: usize,
        body_end: usize,
    },
    /// Anything else: one character, or a run of digits.
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// The tokens every secret rule reads, in one [`Reading`]: where comments
/// and quoted text start and end, and what is a word. Tokens are read on
/// demand from any token boundary, so a rule that skips a bare value
/// (`pw#1$x`) goes on from where it ends.
struct Lexer<'a> {
    sql: &'a str,
    b: &'a [u8],
    reading: Reading,
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str, reading: Reading) -> Self {
        Self { sql, b: sql.as_bytes(), reading }
    }

    fn len(&self) -> usize {
        self.b.len()
    }

    /// The token starting at byte `i` (`i` < the length).
    fn at(&self, i: usize) -> Token {
        let (b, n) = (self.b, self.b.len());
        let c = b[i];
        let tok = |kind, end| Token { kind, start: i, end };
        if c.is_ascii_whitespace() {
            let mut j = i;
            let mut newline = false;
            while j < n && b[j].is_ascii_whitespace() {
                newline |= b[j] == b'\n';
                j += 1;
            }
            return tok(Kind::Space { newline }, j);
        }
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        match c {
            b'-' if b.get(i + 1) == Some(&b'-') => tok(Kind::Comment, self.sql[i..].find('\n').map_or(n, |k| i + k)),
            b'/' if b.get(i + 1) == Some(&b'*') => tok(Kind::Comment, self.sql[i + 2..].find("*/").map_or(n, |k| i + 4 + k)),
            b'\'' | b'"' | b'`' => tok(Kind::Quoted { quote: c, open: i }, self.quoted_end(i, self.reading.backslash && c != b'`')),
            b'#' if self.reading.hash => tok(Kind::Comment, self.sql[i..].find('\n').map_or(n, |k| i + k)),
            // `[name]` on one line, `]]` an escaped bracket; an array
            // subscript or a `[` that never closes is one character.
            b'[' if self.reading.brackets => match self.bracket_end(i) {
                Some(end) => tok(Kind::Quoted { quote: b'[', open: i }, end),
                None => tok(Kind::Other, i + 1),
            },
            b'$' => match dollar_literal(self.sql, i).filter(|_| self.reading.dollar) {
                Some((body, body_end, end)) => tok(Kind::Dollar { body, body_end }, end),
                None => tok(Kind::Other, i + 1),
            },
            c if ident(c) => {
                let mut j = i;
                while j < n && ident(b[j]) {
                    j += 1;
                }
                if c.is_ascii_digit() {
                    return tok(Kind::Other, j);
                }
                // E'…' (Postgres, backslashes escape), N'…' (SQL Server).
                if j == i + 1 && matches!(c, b'E' | b'e' | b'N' | b'n') && b.get(j) == Some(&b'\'') {
                    return tok(Kind::Quoted { quote: b'\'', open: j }, self.quoted_end(j, self.reading.backslash));
                }
                // Oracle q'[…]', q'{…}', q'!…!' (and nq'…'): its text is read
                // like a dollar-quoted one.
                if self.reading.brackets
                    && b.get(j) == Some(&b'\'')
                    && matches!(self.sql[i..j].to_ascii_lowercase().as_str(), "q" | "nq")
                    && let Some((body, body_end, end)) = self.q_quote(j)
                {
                    return tok(Kind::Dollar { body, body_end }, end);
                }
                tok(Kind::Word, j)
            }
            // One character (a whole UTF-8 one, so slices stay on boundaries).
            _ => tok(Kind::Other, i + self.sql[i..].chars().next().map_or(1, char::len_utf8)),
        }
    }

    /// The end of the quoted text at `i`, after its closing quote (doubled
    /// quotes escape; `\` too when `escapes`); unterminated: the end.
    fn quoted_end(&self, i: usize, escapes: bool) -> usize {
        let (b, n) = (self.b, self.b.len());
        let q = b[i];
        let mut j = i + 1;
        while j < n {
            if escapes && b[j] == b'\\' && j + 1 < n {
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
    }

    /// The end of the bracketed name at `i` (`[`), after its `]`: on one
    /// line and at most 130 bytes (a name is at most 128 characters long).
    fn bracket_end(&self, i: usize) -> Option<usize> {
        let b = self.b;
        let mut j = i + 1;
        while j < b.len() && j < i + 130 && b[j] != b'\n' {
            if b[j] == b']' {
                if b.get(j + 1) == Some(&b']') {
                    j += 2;
                    continue;
                }
                return (j > i + 1).then_some(j + 1);
            }
            j += 1;
        }
        None
    }

    /// Oracle's `q'<d>…<d>'` whose `'` is at `i`: the start and end of its
    /// text and the end of the literal (unterminated: the end of the SQL).
    fn q_quote(&self, i: usize) -> Option<(usize, usize, usize)> {
        let open = self.sql[i + 1..].chars().next().filter(|c| !c.is_whitespace() && *c != '\'')?;
        let close = match open {
            '[' => ']',
            '{' => '}',
            '(' => ')',
            '<' => '>',
            c => c,
        };
        let body = i + 1 + open.len_utf8();
        let n = self.len();
        Some(match self.sql[body..].find(&format!("{close}'")) {
            Some(k) => (body, body + k, body + k + close.len_utf8() + 1),
            None => (body, n, n),
        })
    }

    /// The next token from `i` that isn't space or a comment.
    fn next(&self, mut i: usize) -> Option<Token> {
        while i < self.len() {
            let t = self.at(i);
            match t.kind {
                Kind::Space { .. } | Kind::Comment => i = t.end,
                _ => return Some(t),
            }
        }
        None
    }

    /// The word `t` in lower case; empty for other tokens.
    fn word(&self, t: Token) -> String {
        if t.kind == Kind::Word { self.sql[t.start..t.end].to_ascii_lowercase() } else { String::new() }
    }

    /// Is `t` the one-character token `c`?
    fn is(&self, t: Token, c: u8) -> bool {
        t.kind == Kind::Other && t.end == t.start + 1 && self.b[t.start] == c
    }

    /// The end of a bare value at `i`: up to a space, `;`, `,`, `)` or a quote.
    fn bare_end(&self, mut j: usize) -> usize {
        let b = self.b;
        while j < b.len() && !b[j].is_ascii_whitespace() && !matches!(b[j], b';' | b',' | b')' | b'\'' | b'"') {
            j += 1;
        }
        j
    }
}

/// How deep SQL inside quoted text is read (a body in a string in a body…).
const MAX_DEPTH: usize = 8;

/// Spans of the secrets in `sql`, in one reading of its quotes. `depth`:
/// how many quoted texts this SQL is inside.
fn secret_spans(sql: &str, reading: Reading, depth: usize) -> Vec<Span> {
    let lx = Lexer::new(sql, reading);
    let mut cut = positional_secrets(&lx);
    let mut i = 0;
    while i < lx.len() {
        let t = lx.at(i);
        i = t.end;
        match t.kind {
            // A string can hold SQL that runs (`EXECUTE 'alter role r
            // password ''pw'''`, `EXECUTE IMMEDIATE '…'`): read inside it too.
            Kind::Quoted { quote: b'\'', open } if depth < MAX_DEPTH => cut.extend(nested_spans(sql, open, t.end, reading, depth + 1)),
            // `$$Bob's table$$`: one literal, its quote is text. Its body is
            // read on its own (a function or `DO` body holds statements).
            Kind::Dollar { body, body_end } if depth < MAX_DEPTH => {
                cut.extend(secret_spans(&sql[body..body_end], reading, depth + 1).into_iter().map(|(s, e, q)| (s + body, e + body, q)));
            }
            Kind::Word => {
                if let Some(end) = secret_after(&lx, t, &mut cut) {
                    i = end;
                }
            }
            _ => {}
        }
    }
    cut
}

/// The rules for a secret after a word: `IDENTIFIED BY x`, `IDENTIFIED WITH
/// plugin BY|AS x`, `IDENTIFIED BY VALUES 'hash'`, `… REPLACE old`, and a
/// [`SECRET_WORDS`] word then `[FOR account =] [=] [PASSWORD] [(]` and the
/// value. The secret's span goes to `cut`; returns where reading goes on.
fn secret_after(lx: &Lexer, word: Token, cut: &mut Vec<Span>) -> Option<usize> {
    let w = lx.word(word);
    let identified = w == "identified";
    // The value's token, and whether it may be a bare word.
    let (value, bare) = if identified {
        let next = lx.next(word.end)?;
        match lx.word(next).as_str() {
            "by" => {
                let v = lx.next(next.end)?;
                // Oracle `IDENTIFIED BY VALUES 'hash'`: the hash, not the word.
                if lx.word(v) == "values" { (lx.next(v.end)?, false) } else { (v, true) }
            }
            "with" => {
                let plugin = lx.next(next.end).filter(|t| matches!(t.kind, Kind::Word | Kind::Quoted { .. }))?;
                let by = lx.next(plugin.end).filter(|t| matches!(lx.word(*t).as_str(), "by" | "as"))?;
                (lx.next(by.end)?, false)
            }
            _ => return None,
        }
    } else if SECRET_WORDS.contains(&w.as_str()) {
        let mut j = lx.next(word.end)?;
        // SET PASSWORD FOR 'bob'@'%' = 'x': the account, then `=` on the same line.
        if lx.word(j) == "for" {
            let mut k = j.end;
            // An account is a few tokens (`'bob'@'%'`): no further.
            let mut left = 32;
            j = loop {
                left -= 1;
                let t = (left > 0 && k < lx.len()).then(|| lx.at(k))?;
                if lx.is(t, b';') || t.kind == (Kind::Space { newline: true }) {
                    return None;
                }
                if lx.is(t, b'=') {
                    break t;
                }
                k = t.end;
            };
        }
        if lx.is(j, b'=') {
            j = lx.next(j.end)?;
        }
        // SET PASSWORD = PASSWORD('x')
        if lx.word(j) == "password" {
            j = lx.next(j.end)?;
        }
        if lx.is(j, b'(') {
            j = lx.next(j.end)?;
        }
        (j, false)
    } else {
        return None;
    };
    let end = take_value(lx, value, bare, cut)?;
    // Oracle `IDENTIFIED BY new REPLACE old`: the old password too.
    if identified
        && let Some(r) = lx.next(end).filter(|t| lx.word(*t) == "replace")
        && let Some(old) = lx.next(r.end)
        && let Some(end) = take_value(lx, old, true, cut)
    {
        return Some(end);
    }
    Some(end)
}

/// Hides the value at `v`: a quoted string (an unterminated one to the
/// end), the text of a dollar-quoted one, or (when `bare`) a bare word up
/// to a space or the end of the statement (Oracle allows `#` and `$` in
/// one: `pw#1$x`). Returns its end; `None` when `v` is none of these.
fn take_value(lx: &Lexer, v: Token, bare: bool, cut: &mut Vec<Span>) -> Option<usize> {
    match v.kind {
        Kind::Quoted { quote: quote @ (b'\'' | b'"'), open } => cut.push((open, v.end, Some(quote))),
        Kind::Dollar { body, body_end } => cut.push((body, body_end, None)),
        Kind::Word | Kind::Other if bare && (v.kind == Kind::Word || lx.b[v.start] >= 0x80 || lx.b[v.start].is_ascii_digit()) => {
            let end = lx.bare_end(v.start);
            cut.push((v.start, end, None));
            return Some(end);
        }
        _ => return None,
    }
    Some(v.end)
}

/// Secrets in the SQL a `'…'` literal holds (`sql[start..end]`, quotes
/// included): its text with `''` (and `\'` when `backslash`) read as one
/// quote, scanned like any SQL, and the spans mapped back to `sql`. A
/// secret that was a quoted literal inside becomes `''***''` in place, so
/// the outer string still ends where it did.
fn nested_spans(sql: &str, start: usize, end: usize, reading: Reading, depth: usize) -> Vec<Span> {
    let backslash = reading.backslash;
    let b = sql.as_bytes();
    let body_end = if end > start + 1 && end <= b.len() && b[end - 1] == b'\'' { end - 1 } else { end.min(b.len()) };
    // Only a string that reads as a statement: data that merely mentions a
    // password (`'password ''x'''`) stays as typed.
    let first = sql[start + 1..body_end].split(|c: char| !c.is_ascii_alphabetic()).find(|w| !w.is_empty()).unwrap_or("").to_ascii_lowercase();
    if body_end <= start + 1 || !matches!(first.as_str(), "create" | "alter" | "set" | "grant" | "begin" | "declare" | "do" | "exec" | "execute" | "call") {
        return Vec::new();
    }
    // The inner text, and for each of its bytes (and its end) the offset in `sql`.
    let mut inner = Vec::with_capacity(body_end - start);
    let mut at = Vec::with_capacity(body_end - start + 1);
    let mut j = start + 1;
    while j < body_end {
        at.push(j);
        if (b[j] == b'\'' || (backslash && b[j] == b'\\')) && j + 1 < body_end && b[j + 1] == b'\'' {
            inner.push(b'\'');
            j += 2;
        } else {
            inner.push(b[j]);
            j += 1;
        }
    }
    at.push(body_end);
    let Ok(inner) = String::from_utf8(inner) else { return Vec::new() };
    let ib = inner.as_bytes();
    secret_spans(&inner, reading, depth)
        .into_iter()
        .filter_map(|(s, e, q)| {
            // A quoted secret: its text between the quotes, unquoted (an
            // unterminated one runs to the end).
            let s = if q.is_some() { s + 1 } else { s };
            let e = if q.is_some_and(|q| e > s && ib.get(e - 1) == Some(&q)) { e - 1 } else { e };
            (e > s).then(|| (at[s], at[e], None))
        })
        .collect()
}

/// Quoted arguments of calls in [`POSITIONAL_SECRETS`] that hold a secret:
/// `s3(url, key, 'secret')`, `exec sp_addlinkedsrvlogin 'S', 'false', NULL,
/// 'u', 'pw'`. A function's arguments are in parentheses; a procedure's
/// follow a space and run to the end of the line or statement (they may
/// start on the next line). Dollar-quoted bodies are read by
/// [`secret_spans`] on their own.
fn positional_secrets(lx: &Lexer) -> Vec<Span> {
    /// A call being read: its secret arguments, how many it needs, and
    /// the arguments so far.
    struct Call {
        secret: &'static [usize],
        min: usize,
        parens: bool,
        args: Vec<(usize, usize)>,
        start: usize,
        depth: i32,
        named: bool,
    }
    let n = lx.len();
    let mut out = Vec::new();
    let mut close = |mut c: Call, end: usize| {
        c.args.push((c.start, end));
        // A call with a named argument (`@rmtpassword = …`) is left to [`redact_key_values`].
        if c.named || c.args.len() < c.min {
            return;
        }
        for &k in c.secret {
            let Some(&(s, e)) = c.args.get(k) else { continue };
            let mut a = s;
            while a < e && matches!(lx.at(a).kind, Kind::Space { .. }) {
                a = lx.at(a).end;
            }
            if a < e
                && let Kind::Quoted { quote: quote @ (b'\'' | b'"'), open } = lx.at(a).kind
            {
                out.push((open, lx.at(a).end, Some(quote)));
            }
        }
    };
    // The calls open at this point, the innermost last: a call in an
    // argument of another (`s3(url, id, aes_decrypt(c, 'key'))`) is read too.
    let mut open: Vec<Call> = Vec::new();
    let mut i = 0;
    while i < n {
        let t = lx.at(i);
        i = t.end;
        match t.kind {
            Kind::Word => {
                let name = lx.word(t);
                let Some(&(_, secret, min)) = POSITIONAL_SECRETS.iter().find(|(f, ..)| *f == name) else { continue };
                let mut j = t.end;
                let spaced = j < n && matches!(lx.at(j).kind, Kind::Space { .. });
                if spaced {
                    j = lx.at(j).end;
                }
                let parens = j < n && lx.is(lx.at(j), b'(');
                if !parens && (!spaced || !name.starts_with("sp_")) {
                    continue;
                }
                // A procedure is a statement of its own, never an argument.
                if !parens && open.iter().any(|c| c.parens) {
                    continue;
                }
                if parens {
                    j += 1;
                }
                open.push(Call { secret, min, parens, args: Vec::new(), start: j, depth: 0, named: false });
                i = j;
            }
            // A procedure's arguments end with the line, and what is open
            // in them with it (only procedures are below a procedure).
            Kind::Space { newline: true } if open.first().is_some_and(|c| !c.parens) => {
                while let Some(c) = open.pop() {
                    close(c, t.start);
                }
            }
            Kind::Other if lx.b[t.start] == b';' && open.first().is_some_and(|c| !c.parens) => {
                while let Some(c) = open.pop() {
                    close(c, t.start);
                }
            }
            Kind::Other => {
                let Some(c) = open.last_mut() else { continue };
                match lx.b[t.start] {
                    b'(' => c.depth += 1,
                    b')' if c.depth > 0 => c.depth -= 1,
                    b')' if c.parens => close(open.pop().expect("checked"), t.start),
                    b',' if c.depth == 0 => {
                        c.args.push((c.start, t.start));
                        c.start = t.end;
                    }
                    b'=' | b'@' if c.depth == 0 => c.named = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Calls that never close run to the end.
    while let Some(c) = open.pop() {
        close(c, n);
    }
    out
}

/// A dollar-quoted literal at `i` (`$$…$$`, `$tag$…$tag$`): the end of the
/// opening tag, the start of the closing tag (the end of the text when it
/// is missing) and the end of the literal.
fn dollar_literal(sql: &str, i: usize) -> Option<(usize, usize, usize)> {
    let b = sql.as_bytes();
    if b.get(i) != Some(&b'$') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    // `$1` is a parameter, not a tag.
    if b.get(j) != Some(&b'$') || b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let tag = &sql[i..=j];
    let body = j + 1;
    Some(match sql[body..].find(tag) {
        Some(k) => (body, body + k, body + k + tag.len()),
        None => (body, b.len(), b.len()),
    })
}

/// `text` with the password of every `scheme://user:password@host` in it
/// replaced by `***` (connection strings pasted into a question, `ATTACH
/// 'postgres://u:pw@h'`). The user stays; a URL without a password is kept.
/// The password runs from the first colon of the authority to the last `@`
/// of the URL, so one with `/`, `?`, `#` or `@` in it (`postgres://u:pa/ss@h/db`,
/// `u:/pw@h`), or a user name with `@` in it (`me@x.com:pw@h`), is hidden
/// whole. `host:port` (digits after the colon) is a port only when no `@`
/// follows it, or the `@` is in a query parameter (`h:5432/db?x=a@b`). A
/// password with a space in it (`u:pa ss@h`) runs to the next `@` on the line;
/// one with a quote or bracket in it (`u:pa)ss@h`) to the last `@` of the word.
/// Digits then `?` are a password when a host and path follow the `@`
/// (`u:1234?a=b@h/db`), a port and query otherwise (`h:5432?mail=a@b.com`).
/// When in doubt the text is hidden: `https://h:8443/a@b` loses `8443/a`.
pub fn redact_url_passwords(text: &str) -> String {
    let stop = |c: char| matches!(c, '\'' | '"' | '`' | '<' | '>' | '(' | ')');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let start = i + 3;
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // The URL as written in the text: up to a space, a quote or a bracket.
        let token = rest.find(|c: char| stop(c) || c.is_whitespace()).unwrap_or(rest.len());
        let Some(colon) = userinfo_colon(&rest[..token]) else { continue };
        let after = &rest[colon + 1..token];
        // JDBC URLs (`jdbc:sqlserver://host:1433;user=me@corp`) put `;key=value`
        // parameters after the port.
        let scheme = out[..out.len() - 3].rsplit(|c: char| c.is_whitespace() || stop(c)).next().unwrap_or("");
        let jdbc = scheme.to_ascii_lowercase().starts_with("jdbc:");
        // `host:5432`, `host:5432/db`, `host:5432?x`, JDBC `host:1433;db=x`,
        // or a port ending a sentence (`localhost:8080, or …`) when the next
        // word has no `@` (`u:12345, pass@h` is a password with a space).
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let tail = &after[digits..];
        let next_word_has_at = || rest[token..].split_whitespace().next().is_some_and(|w| w.contains('@'));
        let port = digits > 0
            && (tail.chars().next().is_none_or(|c| matches!(c, '/' | '?' | '#') || (jdbc && c == ';'))
                || (!tail.is_empty() && tail.chars().all(|c| matches!(c, '.' | ',' | ':' | ';' | '!')) && !next_word_has_at()));
        let params = if jdbc { &['?', ';'][..] } else { &['?'][..] };
        let at = match after.find('@') {
            // `host:5432/db?x=a@b`, `host:1433;user=me@corp`: the `@` is in a parameter.
            // But digits then `?` and a host and path after the `@`
            // (`u:1234?a=b@h/db`) read as a password: hidden.
            Some(first)
                if port
                    && after[..first].find(params).is_some_and(|q| after[q..first].contains('='))
                    && !(tail.starts_with('?') && after[first..].contains('/')) =>
            {
                None
            }
            // The password ends at the last `@`, except one in the query string
            // after a host and path (`u:pw@h/db?x=1@2` keeps its host); when
            // what follows isn't a host and path (`u:a@b?c@h`), the whole is hidden.
            Some(first) => {
                let cut = after[first..].find('?').map(|q| first + q).filter(|&q| {
                    let before = &after[..q];
                    before.rfind('@').is_some_and(|a| before[a..].contains('/'))
                });
                let scope = cut.map_or(after, |q| &after[..q]);
                scope.rfind('@').map(|a| colon + 1 + a)
            }
            // `u:pa)ss@h`, `'…://u:pa'ss@h/db'`, `u:1234)x@h`: the URL stopped
            // at a quote or bracket inside the password (a letter or digit
            // follows it, not a separator such as `','b@c'`); it runs to the
            // last `@` of the word.
            None if (!port || tail.is_empty()) && in_password(&rest[token..]) && {
                let w = word(&rest[token..]);
                !w.contains("://") && w.contains('@')
            } =>
            {
                word(&rest[token..]).rfind('@').map(|a| token + a)
            }
            // `u:pa ss@h`: a password with a space in it, up to the next `@`
            // on the line (the last `@` of that word), not into another URL.
            None if !port => {
                let line = rest.find(|c: char| stop(c) || matches!(c, '\n' | '\r')).unwrap_or(rest.len());
                let more = &rest[token..line];
                more.find('@').filter(|a| !more[..*a].contains("://")).and_then(|a| {
                    let word = more[a..].find(char::is_whitespace).map_or(more.len(), |w| a + w);
                    more[..word].rfind('@').map(|a| token + a)
                })
            }
            // `u:12345 pass@h/db`: digits, a space, then a word with `@` and
            // a host and path; `h:8080 bob@x.com` (no path) stays a port.
            None if tail.is_empty() && rest[token..].starts_with([' ', '\t']) => {
                let w = word(rest[token..].trim_start_matches([' ', '\t']));
                let start = rest.len() - rest[token..].trim_start_matches([' ', '\t']).len();
                // A host then a path (`h/db`), not `git@github.com:org/repo`.
                let host_path = |rest: &str| rest.find('/').is_some_and(|sl| !rest[..sl].contains(':'));
                w.rfind('@').filter(|&a| !w.contains("://") && host_path(&w[a..])).map(|a| start + a)
            }
            None => None,
        };
        if let Some(at) = at {
            out.push_str(&rest[..colon + 1]);
            out.push_str("***");
            rest = &rest[at..];
        }
    }
    out.push_str(rest);
    out
}

/// Text up to the next whitespace.
fn word(s: &str) -> &str {
    &s[..s.find(char::is_whitespace).unwrap_or(s.len())]
}

/// `s` starts with a quote or bracket that sits inside a password: a letter,
/// digit or other password character follows it (not a separator, the end
/// of the text, or another quote or bracket).
fn in_password(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('\'' | '"' | '`' | '<' | '>' | '(' | ')'))
        && c.next()
            .is_some_and(|n| !n.is_whitespace() && !matches!(n, ',' | ';' | ':' | '+' | '|' | '\'' | '"' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'))
}

/// The colon after the user name (or host) of a URL without its `scheme://`:
/// the first colon outside `[…]` (an IPv6 host), before any `/`, `?` or `#`
/// (`https://h/a:b@c` has its colon in the path). The user may contain `@`.
fn userinfo_colon(url: &str) -> Option<usize> {
    let mut chars = url.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            ':' => return Some(i),
            '/' | '?' | '#' => return None,
            '[' => {
                chars.by_ref().find(|(_, c)| *c == ']')?;
            }
            _ => {}
        }
    }
    None
}

/// What DataBrain keeps of text that may quote SQL or connection strings
/// (query history, job logs, AI transcripts and audit): [`redact_secrets`],
/// [`redact_url_passwords`] and [`redact_key_values`].
/// Text in double quotes is redacted too: error messages quote the SQL they
/// failed on that way (`syntax error at or near "PASSWORD 'pw'"`).
pub fn redact_text(text: &str) -> String {
    let sql = redact_secrets(text);
    let quoted = sql.split('"').enumerate().map(|(i, part)| if i % 2 == 1 { redact_secrets(part) } else { part.to_string() }).collect::<Vec<_>>().join("\"");
    redact_bearer(&redact_key_values(&redact_url_passwords(&quoted)))
}

/// `Bearer <token>` as a header value (HTTP headers in DuckDB secrets,
/// ClickHouse `url(…, headers(…))`): the token becomes `***`.
fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(p) = lower[from..].find("bearer ") {
        let at = from + p;
        from = at + 7;
        // A header value: right after a quote, `:` or `=` (`'Bearer x'`,
        // `Authorization: Bearer x`), not a column named bearer.
        let before = text[..at].trim_end().bytes().last();
        if !matches!(before, Some(b'\'' | b'"' | b':' | b'=')) {
            continue;
        }
        let start = at + 7;
        let mut end = start;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')) {
            end += 1;
        }
        if end > start && &text[start..end] != "***" {
            out.push_str(&text[last..start]);
            out.push_str("***");
            last = end;
            from = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Is `name` (lower case, without a leading `@`) the name of a secret?
/// `password`, `pwd`, `master_password`, `rmtpassword`, `client_secret`,
/// `api_key`… A suffix is enough: drivers and procedures prefix them.
fn secret_name(name: &str) -> bool {
    const ENDS: &[&str] = &[
        "password",
        "passwd",
        "pwd",
        "secret",
        "secret_key",
        "secretkey",
        "access_key",
        "accesskey",
        "api_key",
        "apikey",
        "_token",
        "passphrase",
        "accountkey",
        "privatekey",
        "private_key",
        "sharedaccesssignature",
        "authorization",
    ];
    // Azure SAS tokens (`…&sig=…`).
    SECRET_WORDS.contains(&name) || name == "sig" || ENDS.iter().any(|e| name.ends_with(e))
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
        // Outside strings a bare value is kept (`where password = hash`),
        // except where the text is a connection string, an environment
        // variable or a URL, not SQL: the pair right after `;`
        // (`Server=h;Password=pw;`), an upper-case prefixed name with no
        // spaces (`PGPASSWORD=pw`, `MYSQL_PWD=pw`), a query parameter
        // (`?access_token=t`). Always `name=value`, no space around `=`.
        let tight = e < n && b[e] == b'=' && j > e && j < n && !b[j].is_ascii_whitespace();
        let prev = text[..i].trim_end_matches([' ', '\t']).bytes().last();
        let bare_here = tight
            && (matches!(prev, Some(b';'))
                || (i > 0 && matches!(b[i - 1], b'?' | b'&'))
                || (text[i..e].bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
                    && !matches!(name.as_str(), "password" | "passwd" | "pwd" | "secret" | "token")));
        if in_string.is_none() && !bare_here {
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

    /// Secrets the final review found kept: after a string ending in a
    /// backslash, after an apostrophe in a dollar-quoted string, behind a
    /// comment, in HTTP headers and SAS tokens.
    #[test]
    fn redacts_secrets_the_final_review_found() {
        for (text, secret) in [
            ("insert into t values ('C:\\'); alter role r with password 'pw1'", "pw1"),
            ("select 'C:\\temp\\' as p; create user u identified by pw2", "pw2"),
            ("SELECT 'a\\'; CREATE USER u IDENTIFIED BY pw39", "pw39"),
            ("comment on table t is $$Bob's table$$; alter role r password 'pw4'", "pw4"),
            ("create user u identified by -- c\n 'pw66'", "pw66"),
            ("alter role r password /* c */ 'pw67'", "pw67"),
            ("create secret h (type http, extra_http_headers map {'Authorization': 'Bearer tok5'})", "tok5"),
            ("select * from url('http://x', headers('Authorization'='Bearer tok6'))", "tok6"),
            ("'BlobEndpoint=https://a;SharedAccessSignature=sv=2022&sig=SIG8'", "SIG8"),
            ("{\"privateKey\": \"pk9\"}", "pk9"),
            ("alter role r password 'a\\' password 'SECRET1'", "SECRET1"),
            ("create user u identified by 'x\\' ; alter user v identified by 'SECRET2'", "SECRET2"),
        ] {
            let out = redact_text(text);
            assert!(!out.contains(secret), "{text}\n→ {out}");
        }
        // Found by the 2026-11-13 review: settings and connection strings
        // outside quotes (AI text), URL tokens, `\"` in a double-quoted
        // password, and positional keys of encryption functions.
        for (text, secret) in [
            ("Use Server=db1;Database=app;User Id=sa;Password=pw10;", "pw10"),
            ("conn: Host=h; Port=5432; Pwd=pw11", "pw11"),
            ("run it with PGPASSWORD=pw12 psql -h db", "pw12"),
            ("export MYSQL_PWD=pw13 && mysql", "pw13"),
            ("AWS_SECRET_ACCESS_KEY=sk14 aws s3 ls", "sk14"),
            ("GET https://api.x/v1/data?access_token=tok15&page=2", "tok15"),
            ("see /cb?code=1&refresh_token=tok16", "tok16"),
            ("create user u identified by \"p\\\"w17\"", "w17"),
            ("select pgp_sym_encrypt(card, 'key18') from t", "key18"),
            ("select pgp_sym_decrypt(col::bytea, 'key19')", "key19"),
            ("insert into t values (aes_encrypt('data', 'key20'))", "key20"),
        ] {
            let out = redact_text(text);
            assert!(!out.contains(secret), "{text}\n→ {out}");
        }
        // Found by reading every secret rule from one tokenizer: positional
        // keys after a string the old per-rule scanners read differently
        // (`$$…'…$$`, `E'\''`, `"\""`, a doubled quote), and the MySQL
        // reading, where `$$` is not a quote.
        for (text, secret) in [
            ("comment on t is $$Bob's$$; select aes_encrypt('d', 'key21')", "key21"),
            ("select E'a\\'b', s3('u', 'k', 'sk22')", "sk22"),
            ("create user u identified by \"p\\\"w\"; select s3('u', 'k', 'sk23')", "sk23"),
            ("select 'it''s', mysql('h', 'd', 't', 'u', 'pw24')", "pw24"),
            ("select $$Bob's$$'; select s3('u', 'k', 'sk25')", "sk25"),
        ] {
            let out = redact_text(text);
            assert!(!out.contains(secret), "{text}\n→ {out}");
        }
        // `REPLACE(` after a password is a function call, not the old password.
        assert_eq!(redact_secrets("alter user u identified by pw replace(x, 'a', 'b')"), "alter user u identified by *** replace(x, 'a', 'b')");
        // SQL comparing a column stays readable, outside quotes.
        for sql in [
            "select * from users where password = hash",
            "select * from users where password=hash",
            "update u set PASSWORD=crypt(x) where id = 1",
            "select token, secret from t",
            "select aes_encrypt(x) from t",
        ] {
            assert_eq!(redact_text(sql), sql);
        }
        // Text that only looks like it stays.
        assert_eq!(redact_text("select 'C:\\' as p, 1"), "select 'C:\\' as p, 1");
        assert_eq!(redact_text("select bearer from t"), "select bearer from t");
    }

    /// Secrets in SQL that a dollar-quoted body or a string holds (function
    /// and DO bodies, EXECUTE / EXECUTE IMMEDIATE), and what stays as it was.
    #[test]
    fn redacts_secrets_in_bodies_and_strings() {
        for (text, secret) in [
            ("create procedure p() as $$ begin alter user u identified by 'PW3'; end $$", "PW3"),
            ("do $$ begin execute 'alter role r password ''PW1'''; end $$", "PW1"),
            ("select $a$ from t; alter role r password 'PW5'; select $a$ from t", "PW5"),
            ("begin execute immediate 'create user u identified by PW7'; end;", "PW7"),
            ("begin execute immediate 'alter user u identified by \"PW8\"'; end;", "PW8"),
            ("exec('create login l with password = ''PW9''')", "PW9"),
            ("do $f$ begin perform dblink_exec('alter role r password ''PW10'''); end $f$", "PW10"),
        ] {
            for out in [redact_secrets(text), redact_text(text)] {
                assert!(!out.contains(secret), "{text}\n→ {out}");
            }
        }
        // The outer string still ends where it did.
        assert_eq!(redact_secrets("do $$ begin execute 'alter role r password ''pw'''; end $$"), "do $$ begin execute 'alter role r password ''***'''; end $$");
        assert_eq!(redact_secrets("execute immediate 'create user u identified by pw'"), "execute immediate 'create user u identified by ***'");
        // Strings and bodies without secrets stay.
        for text in [
            "select 'my password is long' as hint, 'it''s' from t",
            "comment on table t is $$Bob's table$$",
            "insert into t values ('identified', 'password')",
            "create function f() returns int as $$ select 1 $$ language sql",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    #[test]
    fn redacts_secrets_the_review_found_missed() {
        for (sql, want) in [
            ("SET PASSWORD FOR 'bob'@'%' = 'newpass'", "SET PASSWORD FOR 'bob'@'%' = '***'"),
            ("set password for bob = password('np')", "set password for bob = password('***')"),
            ("ALTER USER u IDENTIFIED BY 'new' REPLACE 'old'", "ALTER USER u IDENTIFIED BY '***' REPLACE '***'"),
            ("ALTER USER u IDENTIFIED BY new_pw REPLACE old_pw", "ALTER USER u IDENTIFIED BY *** REPLACE ***"),
            ("CREATE USER u IDENTIFIED BY VALUES 'S:ABCD;T:EF'", "CREATE USER u IDENTIFIED BY VALUES '***'"),
            ("CREATE USER u IDENTIFIED BY pw#1$x;", "CREATE USER u IDENTIFIED BY ***;"),
            ("COPY t FROM 's3://b/k' ACCESS_KEY_ID 'AKIA' SECRET_ACCESS_KEY 'sk'", "COPY t FROM 's3://b/k' ACCESS_KEY_ID 'AKIA' SECRET_ACCESS_KEY '***'"),
            ("CREATE SECRET s TYPE = GENERIC_STRING SECRET_STRING = 'tok'", "CREATE SECRET s TYPE = GENERIC_STRING SECRET_STRING = '***'"),
            ("CREATE SECRET h (TYPE http, BEARER_TOKEN 'tok')", "CREATE SECRET h (TYPE http, BEARER_TOKEN '***')"),
            ("ATTACH 'x.db' AS x (ENCRYPTION_KEY 'pw')", "ATTACH 'x.db' AS x (ENCRYPTION_KEY '***')"),
            (
                "select * from azureBlobStorage('DefaultEndpointsProtocol=https;AccountName=a;AccountKey=k==;', 'c', 'b')",
                "select * from azureBlobStorage('DefaultEndpointsProtocol=https;AccountName=a;AccountKey=***;', 'c', 'b')",
            ),
            ("select * from s3('https://b.s3/k.csv', 'AKIA', 'sk', 'CSV')", "select * from s3('https://b.s3/k.csv', 'AKIA', '***', 'CSV')"),
            ("SELECT * FROM mysql('h:3306', 'db', 't', 'u', 'pw')", "SELECT * FROM mysql('h:3306', 'db', 't', 'u', '***')"),
            ("select * from postgresql('h', 'db', 't', 'u', 'pw', 'public')", "select * from postgresql('h', 'db', 't', 'u', '***', 'public')"),
            ("select * from remote('h', db.t, 'u', 'pw')", "select * from remote('h', db.t, 'u', '***')"),
            ("EXEC sp_addlinkedsrvlogin 'S','false',NULL,'u','pw'", "EXEC sp_addlinkedsrvlogin 'S','false',NULL,'u','***'"),
            ("exec sp_addlogin 'l', N'pw'", "exec sp_addlogin 'l', N'***'"),
            ("exec sp_password 'old', 'new', 'l'", "exec sp_password '***', '***', 'l'"),
            ("EXEC sp_setapprole 'r', 'pw'", "EXEC sp_setapprole 'r', '***'"),
            ("exec sp_addlinkedsrvlogin 'S','false',NULL,'u','pw' -- x=1", "exec sp_addlinkedsrvlogin 'S','false',NULL,'u','***' -- x=1"),
            ("exec sp_addlogin\n'l','pw'", "exec sp_addlogin\n'l','***'"),
            ("create user u identified by é#x", "create user u identified by ***"),
        ] {
            assert_eq!(redact_text(sql), want, "{sql}");
        }
        // Still kept: the same names with nothing secret in that place.
        for keep in [
            "select * from s3('https://b.s3/k.csv')",
            "select * from s3('https://b.s3/k.csv', 'CSV')",
            "select * from mysql_table where password_changed_at > now()",
            "select replace(name, 'a', 'b') from t",
            "exec sp_addlinkedsrvlogin @rmtsrvname = 'S', @useself = 'true'",
        ] {
            assert_eq!(redact_text(keep), keep);
        }
    }

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
        // Dollar quotes (Postgres).
        for (sql, want) in [
            ("ALTER ROLE r PASSWORD $$secret$$", "ALTER ROLE r PASSWORD $$***$$"),
            ("alter role r with password $pw$it's $$ here$pw$ valid until 'x'", "alter role r with password $pw$***$pw$ valid until 'x'"),
            ("create user u identified by $$pw$$", "create user u identified by $$***$$"),
            ("alter role r password $$unfinished", "alter role r password $$***"),
        ] {
            assert_eq!(redact_secrets(sql), want, "{sql}");
            assert_eq!(redact_secrets(want), want, "idempotent: {want}");
        }
        assert_eq!(redact_secrets("select $1 from t where password = $1"), "select $1 from t where password = $1", "a parameter, not a tag");
        // Escape strings (Postgres E'…' with `\'`), N'…', and doubled quotes.
        for (sql, want) in [
            ("ALTER USER u PASSWORD E'sec\\'ret'", "ALTER USER u PASSWORD E'***'"),
            ("CREATE ROLE r LOGIN PASSWORD e'a\\\\b\\'c' VALID UNTIL 'x'", "CREATE ROLE r LOGIN PASSWORD e'***' VALID UNTIL 'x'"),
            ("create login l with password = N'pw'", "create login l with password = N'***'"),
            ("ALTER ROLE r PASSWORD 'it''s' VALID UNTIL 'x'", "ALTER ROLE r PASSWORD '***' VALID UNTIL 'x'"),
            ("alter role r password E'unfinished\\'", "alter role r password E'***'"),
        ] {
            assert_eq!(redact_secrets(sql), want, "{sql}");
            assert_eq!(redact_text(sql), want, "{sql}");
            assert_eq!(redact_secrets(want), want, "idempotent: {want}");
        }
        // Not a password: other E'…' literals and words starting with E.
        for keep in ["select E'it\\'s' as password", "select password, e from t where e = E'x'", "ALTER ROLE r PASSWORD NULL"] {
            assert_eq!(redact_secrets(keep), keep);
        }
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
            // Ordinary text around a URL with a port is kept.
            ("see http://localhost:8080, or email me@x.com", "see http://localhost:8080, or email me@x.com"),
            ("open http://localhost:8080. mail bob@corp.com", "open http://localhost:8080. mail bob@corp.com"),
            ("jdbc:sqlserver://host:1433;databaseName=x;user=me@corp;password=pw", "jdbc:sqlserver://host:1433;databaseName=x;user=me@corp;password=pw"),
            // An `@` in the query string isn't the end of the password.
            ("postgres://u:pw@h/db?x=1@2", "postgres://u:***@h/db?x=1@2"),
            ("postgres://u:pa/ss@h/db?mail=a@b", "postgres://u:***@h/db?mail=a@b"),
            // …but a password with `@` then `?` is hidden whole.
            ("postgres://u:a@b?c@h/db", "postgres://u:***@h/db"),
            ("postgres://u:p@ss?w@h/db", "postgres://u:***@h/db"),
            // `;` ends a port only in JDBC URLs; digits then punctuation then a
            // word with `@` is a password with a space in it.
            ("postgres://u:1234;x=1@host/db", "postgres://u:***@host/db"),
            ("postgres://u:12345, pass@h", "postgres://u:***@h"),
            ("postgres://u:1234. abc@h/db", "postgres://u:***@h/db"),
            ("postgres://u:1234,@h", "postgres://u:***@h"),
            // Digits then `?` and a host and path after the `@`.
            ("postgres://u:1234?a=b@h/db", "postgres://u:***@h/db"),
            // A quote or bracket in the password.
            ("postgres://u:pa)ss@h/db", "postgres://u:***@h/db"),
            ("postgres://u:pa(ss@h/db", "postgres://u:***@h/db"),
            ("ATTACH 'postgres://u:pa'ss@h/db' AS p", "ATTACH 'postgres://u:***@h/db' AS p"),
            ("postgres://u:p\"w@h/db", "postgres://u:***@h/db"),
            ("postgres://u:a<b>c@h/db", "postgres://u:***@h/db"),
            // …also after digits, and digits then a space then a word with a host and path.
            ("postgres://u:1234)x@h/db", "postgres://u:***@h/db"),
            ("postgres://u:1234'x@h/db", "postgres://u:***@h/db"),
            ("postgres://u:12345 pass@h/db", "postgres://u:***@h/db"),
            (
                "connect to mongodb+srv://u:p%40ss@cluster0.x.net/?retryWrites=true please",
                "connect to mongodb+srv://u:***@cluster0.x.net/?retryWrites=true please",
            ),
            ("ATTACH 'mysql://root:pw@localhost/shop' AS m", "ATTACH 'mysql://root:***@localhost/shop' AS m"),
            ("two: a://x:1@h and b://y:2@k", "two: a://x:***@h and b://y:***@k"),
            ("user:p@ss with @ in it: s://u:p@ss@host/db", "user:p@ss with @ in it: s://u:***@host/db"),
            // `/`, `?` and `#` in the password: hidden whole, up to the last `@`.
            ("postgres://u:pa/ss@h/db", "postgres://u:***@h/db"),
            ("ATTACH 'postgres://u:a/b?c#d@h:5432/db' AS p", "ATTACH 'postgres://u:***@h:5432/db' AS p"),
            ("mysql://root:x/y@localhost", "mysql://root:***@localhost"),
            // A password starting with `/`, `?` or `#`, or digits then one of
            // them, is not a port.
            ("postgres://u:/secret@h/db", "postgres://u:***@h/db"),
            ("postgres://u:#secret@h/db", "postgres://u:***@h/db"),
            ("postgres://u:?secret@h/db", "postgres://u:***@h/db"),
            ("postgres://u:12/ab@h", "postgres://u:***@h"),
            ("postgres://u:12?ab@h/db", "postgres://u:***@h/db"),
            ("postgres://u:12#ab@h:5432/db", "postgres://u:***@h:5432/db"),
            ("postgres://u:12@h/db", "postgres://u:***@h/db"),
            // An email address as the user name.
            ("postgres://me@x.com:pa/ss@h/db", "postgres://me@x.com:***@h/db"),
            ("postgres://me@x.com:pw@h:5432/db", "postgres://me@x.com:***@h:5432/db"),
            // A space in the password.
            ("postgres://u:pa ss@h/db", "postgres://u:***@h/db"),
            ("ATTACH 'postgres://u:a b c@h/db' AS p", "ATTACH 'postgres://u:***@h/db' AS p"),
            ("postgres://u:pa ss@x@h/db then", "postgres://u:***@h/db then"),
            // IPv6 hosts and non-ASCII passwords.
            ("postgres://u:pw@[::1]:5432/db", "postgres://u:***@[::1]:5432/db"),
            ("postgres://ü:pässwörd@h/db", "postgres://ü:***@h/db"),
            ("postgres://u:mật khẩu@h/db", "postgres://u:***@h/db"),
            ("postgres://u:密码/é@h", "postgres://u:***@h"),
            // Ambiguous: a port then a path with `@` is hidden (it reads the
            // same as `u:12/ab@h`).
            ("https://example.com:8443/a@b", "https://example.com:***@b"),
        ] {
            assert_eq!(redact_url_passwords(text), want, "{text}");
            assert_eq!(redact_url_passwords(want), want, "idempotent: {want}");
        }
        for keep in [
            "postgres://u:***@h:5432/db",
            "postgres://u@h:5432/db",
            "postgres://h:5432/db",
            "postgres://h:5432/db?x=a@b",
            "postgres://u@h:5432/db?x=a@b",
            "postgres://[::1]:5432/db",
            "postgres://u@[::1]:5432/db?x=a@b",
            "postgres://[::1]/db",
            "s3://bucket/key",
            "neo4j://user@host",
            "no url here: a:b@c",
            "x://",
            "x://u:",
            "https://h/a:b@c",
            "http://h:80/?q=a@b",
            "see postgres://h:5432 or mail me@x.com",
            "two: a://x@h:1 and b://y@k:2/d",
            "x://h:é",
            "x://[::1",
            "postgres://h:5432?mail=a@b.com",
            "f('x://u:a', 'b@c')",
            "f('x://u:a','b@c')",
            "{\"url\":\"http://svc:abc\",\"owner\":\"me@x.com\"}",
            "('http://h:8080'),'me@x.com'",
            "see http://h:8080 bob@x.com",
            "ssh://host:22 git@github.com:org/repo.git",
            "\"http://svc:abc\"+\"me@x.com/a\"",
            "open http://localhost:3000 then mail me@x.com/notes",
            "('x://u:a') then b://c@d",
        ] {
            assert_eq!(redact_url_passwords(keep), keep);
        }
        assert_eq!(redact_url_passwords("postgres://u:secret@h:5432/db"), "postgres://u:***@h:5432/db");
        // A space-separated password does not reach into the next URL.
        assert_eq!(redact_url_passwords("a://u:x b://h/@c"), "a://u:x b://h/@c");
        // What history, job logs and the audit log keep.
        assert_eq!(
            redact_text("ATTACH 'postgres://u:pa/ss@h/db' AS p; ALTER ROLE r PASSWORD $$s$$"),
            "ATTACH 'postgres://u:***@h/db' AS p; ALTER ROLE r PASSWORD $$***$$"
        );
        assert_eq!(redact_text("create user u identified by 'pw'; -- from postgres://a:b@h"), "create user u identified by '***'; -- from postgres://a:***@h");
        assert_eq!(redact_text("syntax error at or near \"PASSWORD 'pw'\""), "syntax error at or near \"PASSWORD '***'\"");
        assert_eq!(redact_text("alter role r password \"pw\""), "alter role r password \"***\"");
        let plain = "column \"a\" of \"t\" says \"hello\"";
        assert_eq!(redact_text(plain), plain);
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    /// A call in another's argument, text after a `#` comment, a bracketed
    /// name or an Oracle `q'[…]'` holding a quote, and a password led by a
    /// digit: hidden; the same syntax without a secret: kept.
    #[test]
    fn secrets_after_other_servers_syntax() {
        for (sql, want) in [
            ("select * from s3('u', 'id', aes_decrypt(c, 'k1'))", "select * from s3('u', 'id', aes_decrypt(c, '***'))"),
            ("select pgp_sym_decrypt(pgp_sym_encrypt('x','k1'),'k2')", "select pgp_sym_decrypt(pgp_sym_encrypt('x','***'),'***')"),
            (
                "select mysql('h', 'd', 't', 'u', aes_decrypt(p, 'k1'), s3('a', 'b', 'k2'))",
                "select mysql('h', 'd', 't', 'u', aes_decrypt(p, '***'), s3('a', 'b', '***'))",
            ),
            ("exec sp_addlogin 'u', 'pw1'\nselect aes_encrypt(a, 'k1')", "exec sp_addlogin 'u', '***'\nselect aes_encrypt(a, '***')"),
            ("# don't\nCREATE USER u IDENTIFIED BY 'pw'", "# don't\nCREATE USER u IDENTIFIED BY '***'"),
            ("# don't\nselect aes_encrypt(a, 'k1')", "# don't\nselect aes_encrypt(a, '***')"),
            ("select q'[it's]', aes_encrypt(a,'k1') from dual", "select q'[it's]', aes_encrypt(a,'***') from dual"),
            ("select q'!it's!', aes_encrypt(a,'k1') from dual", "select q'!it's!', aes_encrypt(a,'***') from dual"),
            // (The readings where it is a plain string take `]` for part of the bare password.)
            ("execute immediate q'[alter user u identified by pw1]'", "execute immediate q'[alter user u identified by ***'"),
            ("execute immediate q'[alter user u identified by \"pw1\"]'", "execute immediate q'[alter user u identified by \"***\"]'"),
            ("select [it's], aes_encrypt(a,'k1')", "select [it's], aes_encrypt(a,'***')"),
            ("create user u identified by 1234abc", "create user u identified by ***"),
        ] {
            assert_eq!(redact_secrets(sql), want, "{sql}");
        }
        for keep in [
            "select a[1], b['k'] from t where c # d = 1",
            "select * from #temp where name = [it's] and q = 'x'",
            "select s3('u'), aes_encrypt(a, k), mysql('h', 'd', 't')",
            "# password for bob\nselect 1",
            "set password for bob, alice, carol, dave, erin, frank, grace, heidi, ivan = 1",
        ] {
            assert_eq!(redact_secrets(keep), keep);
        }
    }

    /// Text built to make a rule read on to the end each time stays quick.
    #[test]
    fn long_repeats_stay_linear() {
        let t = std::time::Instant::now();
        for (piece, n) in [("password for x ", 20_000), ("s3(", 60_000), ("'set ", 50_000), ("exec sp_password ", 20_000), ("q'[ [a ", 30_000)] {
            let _ = redact_secrets(&piece.repeat(n));
        }
        let tags: String = (0..5_000).map(|i| format!("$t{i}$ ")).collect();
        let _ = redact_secrets(&tags);
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "{:?}", t.elapsed());
    }
}
