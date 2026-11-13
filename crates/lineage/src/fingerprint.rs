//! A stable fingerprint of a query's shape, to tell "the same query run
//! again" from a different query when learning from history.

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::dialect_for;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer};

/// Hex fingerprint of `sql` with literals and placeholders replaced by `?`
/// (a list of them, `IN (1, 2, 3)`, by one `?`), comments and whitespace
/// dropped and unquoted words lower-cased. The same query with other values
/// gets the same fingerprint. Stable across versions (FNV-1a, 64 bits).
pub fn fingerprint(sql: &str, kind: ConnectorKind) -> String {
    let dialect = dialect_for(kind);
    let mut parts: Vec<String> = Vec::new();
    match Tokenizer::new(&*dialect, sql).tokenize() {
        Ok(tokens) => {
            let tokens: Vec<Token> = tokens.into_iter().filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF)).collect();
            for (i, t) in tokens.iter().enumerate() {
                // A sign is part of the number after it (`a = -1` is `a = 1`)
                // unless it follows a value (`a - 1`).
                if matches!(t, Token::Minus | Token::Plus)
                    && matches!(tokens.get(i + 1), Some(Token::Number(..)))
                    && !after_value(i.checked_sub(1).map(|p| &tokens[p]))
                {
                    continue;
                }
                let p = match t.clone() {
                    Token::Whitespace(_) | Token::EOF => continue,
                    Token::Word(w) if w.quote_style.is_none() => w.value.to_lowercase(),
                    Token::Number(..)
                    | Token::Placeholder(_)
                    | Token::SingleQuotedString(_)
                    | Token::DoubleQuotedString(_)
                    | Token::TripleSingleQuotedString(_)
                    | Token::TripleDoubleQuotedString(_)
                    | Token::DollarQuotedString(_)
                    | Token::SingleQuotedByteStringLiteral(_)
                    | Token::DoubleQuotedByteStringLiteral(_)
                    | Token::TripleSingleQuotedByteStringLiteral(_)
                    | Token::TripleDoubleQuotedByteStringLiteral(_)
                    | Token::SingleQuotedRawStringLiteral(_)
                    | Token::DoubleQuotedRawStringLiteral(_)
                    | Token::TripleSingleQuotedRawStringLiteral(_)
                    | Token::TripleDoubleQuotedRawStringLiteral(_)
                    | Token::NationalStringLiteral(_)
                    | Token::QuoteDelimitedStringLiteral(_)
                    | Token::NationalQuoteDelimitedStringLiteral(_)
                    | Token::EscapedStringLiteral(_)
                    | Token::UnicodeStringLiteral(_)
                    | Token::HexStringLiteral(_) => "?".into(),
                    other => other.to_string(),
                };
                // `?, ?, ?` → `?`
                if p == "?" && parts.len() >= 2 && parts[parts.len() - 1] == "," && parts[parts.len() - 2] == "?" {
                    parts.pop();
                    continue;
                }
                parts.push(p);
                // `VALUES (?), (?)` → `VALUES (?)`: rows of literals, any number.
                if parts.len() >= 8 && parts[parts.len() - 8..] == ["values", "(", "?", ")", ",", "(", "?", ")"] {
                    parts.truncate(parts.len() - 4);
                }
            }
            // A trailing `;` doesn't make another query.
            while parts.last().is_some_and(|p| p == ";") {
                parts.pop();
            }
        }
        Err(_) => parts = sql.split_whitespace().map(str::to_lowercase).collect(),
    }
    format!("{:016x}", fnv1a(parts.join(" ").as_bytes()))
}

/// Whether `prev` ends a value, so a `-` after it subtracts.
fn after_value(prev: Option<&Token>) -> bool {
    match prev {
        Some(Token::Word(w)) => w.quote_style.is_some() || w.keyword == Keyword::NoKeyword,
        Some(Token::RParen | Token::RBracket | Token::Number(..) | Token::Placeholder(_) | Token::SingleQuotedString(_)) => true,
        _ => false,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
