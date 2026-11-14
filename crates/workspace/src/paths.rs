//! Dotted table paths as people and the AI write them: `sales.orders`,
//! `main.sales.orders.amount`, with any name quoted (`"…"`, `` `…` ``,
//! `[…]`; `""` inside `"…"` is one quote). A quoted name may contain dots,
//! spaces, `&` or commas: `sales."orders.2024"` is the table `orders.2024`
//! of `sales`, not the table `2024` of `sales.orders`.

/// The names of a path, quotes read: `sales."orders.2024"` →
/// `["sales", "orders.2024"]`. Unquoted names are trimmed; empty ones dropped.
pub fn path_parts(path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = path.chars().peekable();
    let end = |cur: &mut String, quoted: &mut bool, parts: &mut Vec<String>| {
        let name = if *quoted { std::mem::take(cur) } else { cur.trim().to_string() };
        if !name.is_empty() {
            parts.push(name);
        }
        cur.clear();
        *quoted = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                quoted = true;
                while let Some(d) = chars.next() {
                    if d == close {
                        if close != ']' && chars.peek() == Some(&close) {
                            chars.next();
                            cur.push(close);
                            continue;
                        }
                        break;
                    }
                    cur.push(d);
                }
            }
            '.' => end(&mut cur, &mut quoted, &mut parts),
            c => cur.push(c),
        }
    }
    end(&mut cur, &mut quoted, &mut parts);
    parts
}

/// `(schema, name)` of a table reference: the last name is the table, the
/// ones before it the schema id (`main.sales`); no schema for a bare name.
pub fn split_reference(reference: &str) -> (Option<String>, String) {
    let mut parts = path_parts(reference);
    let name = parts.pop().unwrap_or_default();
    ((!parts.is_empty()).then(|| parts.join(".")), name)
}

/// A name quoted when a path (or a note target) would read it otherwise: a
/// dot, a space, `&`, `,`, a quote, or the word `and`/`or`.
pub fn quote_name(name: &str) -> String {
    let low = name.to_ascii_lowercase();
    if name.is_empty() || name.chars().any(|c| c.is_whitespace() || matches!(c, '.' | '&' | ',' | '"' | '`' | '[' | ']')) || low == "and" || low == "or" {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

/// Path text of `parts` (each quoted when needed).
pub fn path_text<S: AsRef<str>>(parts: &[S]) -> String {
    parts.iter().map(|p| quote_name(p.as_ref())).collect::<Vec<_>>().join(".")
}

/// The path of a table (`schema` is a schema id: its dots are levels),
/// with a column when given: what [`path_parts`] reads back.
pub fn table_path(schema: &str, name: &str, column: Option<&str>) -> String {
    let mut parts: Vec<&str> = schema.split('.').filter(|s| !s.is_empty()).collect();
    parts.push(name);
    parts.extend(column);
    path_text(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_quoted_names() {
        assert_eq!(path_parts("sales.orders"), ["sales", "orders"]);
        assert_eq!(path_parts(r#"sales."orders.2024""#), ["sales", "orders.2024"]);
        assert_eq!(path_parts(r#"[my db].`my schema`."a ""b"" c".col"#), ["my db", "my schema", "a \"b\" c", "col"]);
        assert_eq!(path_parts(" sales . orders "), ["sales", "orders"]);
        assert_eq!(split_reference(r#"main.sales."orders.2024""#), (Some("main.sales".into()), "orders.2024".into()));
        assert_eq!(split_reference("orders"), (None, "orders".into()));
        // Written, then read back as the same names.
        for (schema, name) in [("sales", "orders"), ("sales", "orders.2024"), ("main.sales", "R&D, old"), ("s", "and"), ("s", "say \"hi\"")] {
            let p = table_path(schema, name, None);
            assert_eq!(split_reference(&p), (Some(schema.into()), name.into()), "{p}");
        }
        assert_eq!(table_path("sales", "orders.2024", Some("amount")), r#"sales."orders.2024".amount"#);
        assert_eq!(table_path("sales", "orders", None), "sales.orders", "plain names stay plain");
    }
}
