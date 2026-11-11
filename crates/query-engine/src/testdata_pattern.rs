//! Column patterns for test data: a rule a column's values follow, set by
//! the user, suggested by the AI, or a default from the column's name
//! (`age` → `18..90`). Made-up values follow it, and AI values must match it.
//!
//! - Numbers: `lo..hi` (`18..90`, `0.5..99.99`; `18-90` for whole numbers).
//! - Dates and date-times: `1950-01-01..2005-12-31`, or years `1950..2005`;
//!   times `08:00..18:00`.
//! - Any column: `{a|b|c}`, one of these values.
//! - Text: a template. `#` a digit, `?` a letter, `%` a word that suits
//!   the column (a name, a city; `first.last` in an email column), `%d %m
//!   %y %Y %H %M %S` parts of one date, `{a|b}` one of, `\` takes the next
//!   character as it is; other characters stay as written: `%@gmail.com`,
//!   `INV-#####`, `+84 ### ### ###`, `%d/%m/%Y`.

use super::{CITIES, COUNTRIES, FIRST, Flavor, Gen, LAST, MICROS_PER_DAY, Rng, Value, WORDS, cell_value, days_from_civil, real_date};
use databrain_connector_core::value::{parse_date, parse_datetime, parse_time};

/// How to write patterns, for the dialog and the AI.
pub const PATTERN_HELP: &str = "Numbers: lo..hi (18..90, 0.5..99.99). Dates: 1950-01-01..2005-12-31 or 1950..2005; times 08:00..18:00. \
Any column: {a|b|c} for one of these values. Text: a template where # is a digit, ? a letter, % a word that suits the column \
(a name, a city, first.last in an email column), %d %m %y %Y %H %M %S parts of a date, {a|b} one of, \\ keeps the next character; \
other characters stay as written: %@gmail.com, INV-#####, +84 ### ### ###, %d/%m/%Y.";

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Ints(i64, i64),
    Numbers(f64, f64),
    /// Days since 1970.
    Dates(i64, i64),
    /// Microseconds since 1970.
    DateTimes(i64, i64),
    /// Seconds of the day.
    Times(i64, i64),
    OneOf(Vec<String>),
    Template(Vec<Tok>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Lit(char),
    Digit,
    Letter,
    Word,
    /// A part of a date: `d m y Y H M S`.
    Date(char),
    Alt(Vec<String>),
}

const DATE_PARTS: &str = "dmyYHMS";

impl Pattern {
    /// `text` as a pattern for a column made by `base`.
    pub fn parse(base: &Gen, text: &str) -> Result<Pattern, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("empty".into());
        }
        if let Some(inner) = t.strip_prefix('{').and_then(|r| r.strip_suffix('}'))
            && !inner.contains(['{', '}'])
        {
            let options: Vec<String> = inner.split('|').map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect();
            if options.is_empty() {
                return Err("{a|b|c} needs at least one value".into());
            }
            if let Some(bad) = options.iter().find(|o| super::sample_value(base, o).is_none()) {
                return Err(format!("\"{bad}\" doesn't fit the column"));
            }
            return Ok(Pattern::OneOf(options));
        }
        let range = |t: &str| -> Option<(String, String)> {
            let (a, b) = t.split_once("..")?;
            Some((a.trim().to_string(), b.trim().to_string()))
        };
        let ordered = |p: Pattern, ok: bool| if ok { Ok(p) } else { Err("the first value is larger than the second".to_string()) };
        match base {
            Gen::Int { .. } => {
                let (a, b) = range(t)
                    .or_else(|| {
                        let (a, b) = t.split_once('-')?;
                        (!a.is_empty()).then(|| (a.to_string(), b.to_string()))
                    })
                    .ok_or("write whole numbers as lo..hi, e.g. 18..90")?;
                let (lo, hi) = (
                    a.parse::<i64>().map_err(|_| format!("\"{a}\" isn't a whole number"))?,
                    b.parse::<i64>().map_err(|_| format!("\"{b}\" isn't a whole number"))?,
                );
                ordered(Pattern::Ints(lo, hi), lo <= hi)
            }
            Gen::Float | Gen::Decimal { .. } => {
                let (a, b) = range(t).ok_or("write numbers as lo..hi, e.g. 1..1000 or 0.5..99.99")?;
                let num = |s: &str| s.parse::<f64>().ok().filter(|f| f.is_finite()).ok_or(format!("\"{s}\" isn't a number"));
                let (lo, hi) = (num(&a)?, num(&b)?);
                ordered(Pattern::Numbers(lo, hi), lo <= hi)
            }
            Gen::Date | Gen::Timestamp | Gen::TimestampTz => {
                let (a, b) = range(t).ok_or("write dates as 1950-01-01..2005-12-31, or years as 1950..2005")?;
                let day = |s: &str, end: bool| -> Result<i64, String> {
                    if s.len() == 4
                        && let Ok(y) = s.parse::<i64>()
                    {
                        return Ok(if end { days_from_civil(y + 1, 1, 1) - 1 } else { days_from_civil(y, 1, 1) });
                    }
                    let d = s.get(..10).filter(|d| real_date(d)).and_then(parse_date).ok_or(format!("\"{s}\" isn't a date (YYYY-MM-DD)"))?;
                    Ok(i64::from(d))
                };
                if matches!(base, Gen::Date) {
                    let (lo, hi) = (day(&a, false)?, day(&b, true)?);
                    return ordered(Pattern::Dates(lo, hi), lo <= hi);
                }
                let micros = |s: &str, end: bool| -> Result<i64, String> {
                    if s.len() > 10 {
                        return parse_datetime(s).ok_or(format!("\"{s}\" isn't a date-time (YYYY-MM-DD HH:MM:SS)"));
                    }
                    let d = day(s, end)?;
                    Ok(if end { (d + 1) * MICROS_PER_DAY - 1_000_000 } else { d * MICROS_PER_DAY })
                };
                let (lo, hi) = (micros(&a, false)?, micros(&b, true)?);
                ordered(Pattern::DateTimes(lo, hi), lo <= hi)
            }
            Gen::Time => {
                let (a, b) = range(t).ok_or("write times as 08:00..18:00")?;
                let secs = |s: &str| {
                    let s = if s.len() == 5 { format!("{s}:00") } else { s.to_string() };
                    parse_time(&s).map(|m| m / 1_000_000).ok_or(format!("\"{s}\" isn't a time (HH:MM)"))
                };
                let (lo, hi) = (secs(&a)?, secs(&b)?);
                ordered(Pattern::Times(lo, hi), lo <= hi)
            }
            Gen::Text { max, .. } => {
                let toks = parse_template(t)?;
                let shortest: usize = toks.iter().map(min_len).sum();
                if let Some(m) = max
                    && shortest > *m
                {
                    return Err(format!("its values are at least {shortest} characters, and the column holds {m}"));
                }
                Ok(Pattern::Template(toks))
            }
            Gen::Choice(_) => Err("this column has its own values: use {a|b} to pick among them".into()),
            Gen::Patterned { base, .. } => Pattern::parse(base, text),
            Gen::Bool | Gen::Uuid | Gen::Json | Gen::Inet | Gen::Binary { .. } => Err("only {a|b|c} works for this type".into()),
        }
    }

    /// A value following the pattern.
    pub fn generate(&self, base: &Gen, flavor: Flavor, max: Option<usize>, rng: &mut Rng) -> Value {
        match self {
            Pattern::Ints(lo, hi) => Value::Int(rng.range(*lo, *hi)),
            Pattern::Numbers(lo, hi) => {
                let f = lo + (hi - lo) * (rng.below(1_000_001) as f64 / 1_000_000.0);
                match base {
                    Gen::Decimal { scale, .. } => Value::Text(format!("{f:.prec$}", prec = *scale as usize)),
                    _ => Value::Float((f * 100.0).round() / 100.0),
                }
            }
            Pattern::Dates(lo, hi) => Value::Date(rng.range(*lo, *hi) as i32),
            Pattern::DateTimes(lo, hi) => {
                let secs = rng.range(lo.div_euclid(1_000_000), hi.div_euclid(1_000_000));
                if matches!(base, Gen::TimestampTz) { Value::TimestampTz(secs * 1_000_000) } else { Value::Timestamp(secs * 1_000_000) }
            }
            Pattern::Times(lo, hi) => Value::Time(rng.range(*lo, *hi) * 1_000_000),
            Pattern::OneOf(v) => {
                let pick = &v[rng.below(v.len() as u64) as usize];
                super::sample_value(base, pick).unwrap_or(Value::Text(pick.clone()))
            }
            Pattern::Template(toks) => {
                // A few tries for one that fits the column, then cut to fit.
                let mut s = String::new();
                for _ in 0..8 {
                    s = fill(toks, flavor, rng);
                    if max.is_none_or(|m| s.len() <= m) {
                        return Value::Text(s);
                    }
                }
                Value::Text(super::truncate(s, max))
            }
        }
    }

    /// An AI value as the column's value when it follows the pattern.
    pub fn accept(&self, base: &Gen, s: &str) -> Option<Value> {
        match self {
            Pattern::Ints(lo, hi) => s.parse::<i64>().ok().filter(|v| (lo..=hi).contains(&v)).map(Value::Int),
            Pattern::Numbers(lo, hi) => {
                let f = s.parse::<f64>().ok().filter(|f| f.is_finite() && (lo..=hi).contains(&f))?;
                match base {
                    Gen::Decimal { scale, .. } => {
                        let frac = s.split_once('.').map_or(0, |(_, f)| f.len());
                        (frac <= *scale as usize && !s.contains(['e', 'E'])).then(|| Value::Text(s.to_string()))
                    }
                    _ => Some(Value::Float(f)),
                }
            }
            Pattern::Dates(lo, hi) => match (real_date(s), cell_value(databrain_connector_core::ColType::Date, s)) {
                (true, Value::Date(d)) if (*lo..=*hi).contains(&i64::from(d)) => Some(Value::Date(d)),
                _ => None,
            },
            Pattern::DateTimes(lo, hi) => {
                let t = base.col_type();
                match (real_date(s), cell_value(t, s)) {
                    (true, v @ (Value::Timestamp(m) | Value::TimestampTz(m))) if (*lo..=*hi).contains(&m) => Some(v),
                    _ => None,
                }
            }
            Pattern::Times(lo, hi) => match cell_value(databrain_connector_core::ColType::Time, s) {
                Value::Time(m) if (*lo..=*hi).contains(&(m / 1_000_000)) => Some(Value::Time(m)),
                _ => None,
            },
            Pattern::OneOf(v) => v.iter().find(|o| o.eq_ignore_ascii_case(s)).and_then(|o| super::sample_value(base, o)),
            Pattern::Template(toks) => {
                let chars: Vec<char> = s.chars().collect();
                (chars.len() <= 500 && matches(toks, &chars)).then(|| Value::Text(s.to_string()))
            }
        }
    }

    /// In words, for the plan ("whole numbers 18–90").
    pub fn describe(&self, text: &str) -> String {
        match self {
            Pattern::Ints(lo, hi) => format!("whole numbers {lo}–{hi}"),
            Pattern::Numbers(lo, hi) => format!("numbers {lo}–{hi}"),
            Pattern::Dates(..) | Pattern::DateTimes(..) | Pattern::Times(..) => format!("between {}", text.replace("..", " and ")),
            Pattern::OneOf(v) => format!("one of {}", v.join(", ")),
            Pattern::Template(_) => format!("text like {text}"),
        }
    }
}

fn parse_template(t: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => out.push(Tok::Lit(it.next().ok_or("\\ at the end: nothing to keep")?)),
            '#' => out.push(Tok::Digit),
            '?' => out.push(Tok::Letter),
            '%' => match it.peek().copied() {
                Some('%') => {
                    it.next();
                    out.push(Tok::Lit('%'));
                }
                Some(p) if DATE_PARTS.contains(p) => {
                    it.next();
                    out.push(Tok::Date(p));
                }
                _ => out.push(Tok::Word),
            },
            '{' => {
                let mut inner = String::new();
                loop {
                    match it.next() {
                        Some('}') => break,
                        Some(x) => inner.push(x),
                        None => return Err("{ without }".into()),
                    }
                }
                out.push(Tok::Alt(inner.split('|').map(str::to_string).collect()));
            }
            c => out.push(Tok::Lit(c)),
        }
    }
    Ok(out)
}

fn min_len(t: &Tok) -> usize {
    match t {
        Tok::Lit(c) => c.len_utf8(),
        Tok::Digit | Tok::Letter | Tok::Word => 1,
        Tok::Date('Y') => 4,
        Tok::Date(_) => 2,
        Tok::Alt(v) => v.iter().map(String::len).min().unwrap_or(0),
    }
}

/// A word that suits the column (`%`).
fn word(flavor: Flavor, rng: &mut Rng) -> String {
    let ascii = |s: &str| s.to_lowercase().replace('ü', "u");
    match flavor {
        Flavor::Email => format!("{}.{}", ascii(rng.pick(FIRST)), ascii(rng.pick(LAST))),
        Flavor::FirstName => rng.pick(FIRST).to_string(),
        Flavor::LastName => rng.pick(LAST).to_string(),
        Flavor::FullName => format!("{} {}", rng.pick(FIRST), rng.pick(LAST)),
        Flavor::City => rng.pick(CITIES).to_string(),
        Flavor::Country => rng.pick(COUNTRIES).to_string(),
        _ => rng.pick(WORDS).to_string(),
    }
}

fn fill(toks: &[Tok], flavor: Flavor, rng: &mut Rng) -> String {
    // One date for every date part of the value: 2000–2025.
    let day = rng.range(days_from_civil(2000, 1, 1), days_from_civil(2026, 1, 1) - 1);
    let secs = rng.below(86_400) as i64;
    let (y, m, d) = civil(day);
    let mut s = String::new();
    for t in toks {
        match t {
            Tok::Lit(c) => s.push(*c),
            Tok::Digit => s.push((b'0' + rng.below(10) as u8) as char),
            Tok::Letter => s.push((b'A' + rng.below(26) as u8) as char),
            Tok::Word => s.push_str(&word(flavor, rng)),
            Tok::Alt(v) => s.push_str(&v[rng.below(v.len() as u64) as usize]),
            Tok::Date(p) => s.push_str(&match p {
                'd' => format!("{d:02}"),
                'm' => format!("{m:02}"),
                'y' => format!("{:02}", y % 100),
                'Y' => format!("{y:04}"),
                'H' => format!("{:02}", secs / 3600),
                'M' => format!("{:02}", secs / 60 % 60),
                _ => format!("{:02}", secs % 60),
            }),
        }
    }
    s
}

/// Year, month and day of days since 1970.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Whether `s` follows the template.
fn matches(toks: &[Tok], s: &[char]) -> bool {
    let Some((t, rest)) = toks.split_first() else { return s.is_empty() };
    let digits = |n: usize| s.len() >= n && s[..n].iter().all(char::is_ascii_digit);
    let num = |n: usize| s[..n].iter().collect::<String>().parse::<u32>().unwrap_or(0);
    match t {
        Tok::Lit(c) => s.first() == Some(c) && matches(rest, &s[1..]),
        Tok::Digit => s.first().is_some_and(char::is_ascii_digit) && matches(rest, &s[1..]),
        Tok::Letter => s.first().is_some_and(|c| c.is_alphabetic()) && matches(rest, &s[1..]),
        Tok::Word => (1..=s.len()).any(|n| matches(rest, &s[n..])),
        Tok::Alt(v) => v.iter().any(|o| {
            let o: Vec<char> = o.chars().collect();
            s.starts_with(&o) && matches(rest, &s[o.len()..])
        }),
        Tok::Date(p) => {
            let n = if *p == 'Y' { 4 } else { 2 };
            let ok = digits(n)
                && match p {
                    'd' => (1..=31).contains(&num(2)),
                    'm' => (1..=12).contains(&num(2)),
                    'H' => num(2) <= 23,
                    'M' | 'S' => num(2) <= 59,
                    _ => true,
                };
            ok && matches(rest, &s[n..])
        }
    }
}

/// A default pattern from the column's name (`age` → `18..90`), within what
/// the column's generator can hold; `None` when the name says nothing.
pub fn default_pattern(base: &Gen, column: &str) -> Option<String> {
    let lower = column.to_ascii_lowercase();
    // The name's words: `birth_date`, `BirthDate`, `unit-price`.
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in column.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let is = |w: &[&str]| words.iter().any(|x| w.contains(&x.as_str()));
    let has = |w: &[&str]| w.iter().any(|x| lower.contains(x));
    let number: Option<(f64, f64)> = if has(&["salary", "wage", "income"]) {
        Some((1000.0, 20000.0))
    } else if has(&["price", "amount", "cost", "total", "subtotal", "balance"]) || is(&["fee", "fees", "tax"]) {
        Some((1.0, 1000.0))
    } else if is(&["age"]) {
        Some((18.0, 90.0))
    } else if has(&["rating", "stars"]) {
        Some((1.0, 5.0))
    } else if has(&["percent", "percentage"]) || is(&["pct", "score"]) {
        Some((0.0, 100.0))
    } else if has(&["quantity"]) || is(&["qty"]) {
        Some((1.0, 20.0))
    } else if has(&["latitude"]) || is(&["lat"]) {
        Some((-90.0, 90.0))
    } else if has(&["longitude"]) || is(&["lon", "lng", "long"]) {
        Some((-180.0, 180.0))
    } else if is(&["year", "yr"]) {
        Some((1950.0, 2025.0))
    } else if is(&["month"]) {
        Some((1.0, 12.0))
    } else if is(&["hour"]) {
        Some((0.0, 23.0))
    } else if is(&["minute", "minutes"]) {
        Some((0.0, 59.0))
    } else {
        None
    };
    match base {
        Gen::Int { min, max } => {
            let (lo, hi) = number?;
            // Within the generator's range (the type's size and sign).
            let (lo, hi) = ((lo as i64).max(*min.min(&0)), (hi as i64).min(*max));
            (lo <= hi).then(|| format!("{lo}..{hi}"))
        }
        Gen::Decimal { int_digits, scale } => {
            let (lo, hi) = number?;
            let cap = 10f64.powi(*int_digits as i32) - 10f64.powi(-(*scale as i32));
            let (lo, hi) = (lo.max(-cap), hi.min(cap));
            (lo <= hi).then(|| format!("{lo}..{hi}"))
        }
        Gen::Float => number.map(|(lo, hi)| format!("{lo}..{hi}")),
        Gen::Date | Gen::Timestamp | Gen::TimestampTz if has(&["birth", "dob", "born"]) => Some("1950-01-01..2005-12-31".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(max: Option<usize>, flavor: Flavor) -> Gen {
        Gen::Text { max, flavor }
    }

    #[test]
    fn patterns_parse_by_column_type() {
        let int = Gen::Int { min: 1, max: 1_000_000 };
        assert_eq!(Pattern::parse(&int, "18..90"), Ok(Pattern::Ints(18, 90)));
        assert_eq!(Pattern::parse(&int, " 18-90 "), Ok(Pattern::Ints(18, 90)));
        assert_eq!(Pattern::parse(&int, "-5..5"), Ok(Pattern::Ints(-5, 5)));
        assert!(Pattern::parse(&int, "90..18").unwrap_err().contains("larger"));
        assert!(Pattern::parse(&int, "abc").is_err());
        assert_eq!(Pattern::parse(&int, "{1|2|3}"), Ok(Pattern::OneOf(vec!["1".into(), "2".into(), "3".into()])));
        assert!(Pattern::parse(&int, "{1|x}").unwrap_err().contains("\"x\""));
        assert_eq!(Pattern::parse(&Gen::Decimal { int_digits: 4, scale: 2 }, "0.5..99.99"), Ok(Pattern::Numbers(0.5, 99.99)));
        let d = |y, m, dd| days_from_civil(y, m, dd);
        assert_eq!(Pattern::parse(&Gen::Date, "1950..2005"), Ok(Pattern::Dates(d(1950, 1, 1), d(2005, 12, 31))));
        assert_eq!(Pattern::parse(&Gen::Date, "1990-02-01..1990-02-28"), Ok(Pattern::Dates(d(1990, 2, 1), d(1990, 2, 28))));
        assert!(Pattern::parse(&Gen::Date, "1990-02-30..1991-01-01").is_err());
        assert!(Pattern::parse(&Gen::Date, "%d-%m-%y").unwrap_err().contains("1950-01-01"), "a date column stores dates, not text");
        assert_eq!(Pattern::parse(&Gen::Time, "08:00..18:00"), Ok(Pattern::Times(8 * 3600, 18 * 3600)));
        assert!(matches!(Pattern::parse(&Gen::Timestamp, "2024..2024"), Ok(Pattern::DateTimes(..))));
        assert!(Pattern::parse(&Gen::Uuid, "#-#").is_err());
        assert!(Pattern::parse(&text(Some(5), Flavor::Code), "INV-#####").unwrap_err().contains("at least 9"));
        assert_eq!(
            Pattern::parse(&text(None, Flavor::Email), "%@gmail.com"),
            Ok(Pattern::Template(vec![
                Tok::Word,
                Tok::Lit('@'),
                Tok::Lit('g'),
                Tok::Lit('m'),
                Tok::Lit('a'),
                Tok::Lit('i'),
                Tok::Lit('l'),
                Tok::Lit('.'),
                Tok::Lit('c'),
                Tok::Lit('o'),
                Tok::Lit('m')
            ]))
        );
    }

    #[test]
    fn made_up_values_follow_the_pattern_and_ai_values_are_checked() {
        let mut rng = Rng(3);
        let email = text(Some(60), Flavor::Email);
        let p = Pattern::parse(&email, "%@gmail.com").unwrap();
        for _ in 0..50 {
            let Value::Text(v) = p.generate(&email, Flavor::Email, Some(60), &mut rng) else { panic!() };
            assert!(v.ends_with("@gmail.com") && v.contains('.') && v.is_ascii(), "{v}");
            assert!(p.accept(&email, &v).is_some(), "{v}");
        }
        assert!(p.accept(&email, "ann@yahoo.com").is_none());
        assert!(p.accept(&email, "@gmail.com").is_none(), "% is at least one character");

        let code = text(Some(20), Flavor::Code);
        let p = Pattern::parse(&code, "INV-#### ?{A|B}").unwrap();
        let Value::Text(v) = p.generate(&code, Flavor::Code, Some(20), &mut rng) else { panic!() };
        assert!(p.accept(&code, &v).is_some(), "{v}");
        assert!(p.accept(&code, "INV-12a4 XA").is_none());

        let dates = text(Some(10), Flavor::Word);
        let p = Pattern::parse(&dates, "%d-%m-%y").unwrap();
        let Value::Text(v) = p.generate(&dates, Flavor::Word, Some(10), &mut rng) else { panic!() };
        assert_eq!(v.len(), 8, "{v}");
        assert!(p.accept(&dates, "31-12-99").is_some());
        assert!(p.accept(&dates, "32-12-99").is_none());
        assert!(p.accept(&dates, "1-12-99").is_none());

        let age = Gen::Int { min: 0, max: 100 };
        let p = Pattern::parse(&age, "18..90").unwrap();
        for _ in 0..100 {
            let Value::Int(a) = p.generate(&age, Flavor::Word, None, &mut rng) else { panic!() };
            assert!((18..=90).contains(&a));
        }
        assert_eq!(p.accept(&age, "45"), Some(Value::Int(45)));
        assert_eq!(p.accept(&age, "1000"), None);

        let money = Gen::Decimal { int_digits: 4, scale: 2 };
        let p = Pattern::parse(&money, "1..1000").unwrap();
        let Value::Text(v) = p.generate(&money, Flavor::Word, None, &mut rng) else { panic!() };
        assert!(v.split_once('.').is_some_and(|(_, f)| f.len() == 2), "{v}");
        assert!(p.accept(&money, "12.345").is_none(), "scale");

        let born = Pattern::parse(&Gen::Date, "1950..2005").unwrap();
        assert!(born.accept(&Gen::Date, "1980-05-06").is_some());
        assert!(born.accept(&Gen::Date, "2010-05-06").is_none());
        let shift = Pattern::parse(&Gen::Time, "08:00..18:00").unwrap();
        assert!(shift.accept(&Gen::Time, "09:30:00").is_some());
        assert!(shift.accept(&Gen::Time, "19:00:00").is_none());
    }

    #[test]
    fn names_give_sensible_defaults() {
        let int = Gen::Int { min: 1, max: 1_000_000 };
        assert_eq!(default_pattern(&int, "age").as_deref(), Some("18..90"));
        assert_eq!(default_pattern(&int, "customer_age").as_deref(), Some("18..90"));
        assert_eq!(default_pattern(&int, "page"), None, "not a word of the name");
        assert_eq!(default_pattern(&int, "Rating").as_deref(), Some("1..5"));
        assert_eq!(default_pattern(&int, "birthYear").as_deref(), Some("1950..2025"));
        assert_eq!(default_pattern(&Gen::Int { min: 0, max: 100 }, "unit_price").as_deref(), Some("1..100"), "within a tinyint");
        assert_eq!(default_pattern(&Gen::Decimal { int_digits: 3, scale: 2 }, "price").as_deref(), Some("1..999.99"));
        assert_eq!(default_pattern(&Gen::Float, "latitude").as_deref(), Some("-90..90"));
        assert_eq!(default_pattern(&Gen::Date, "date_of_birth").as_deref(), Some("1950-01-01..2005-12-31"));
        assert_eq!(default_pattern(&Gen::Date, "created_at"), None);
        assert_eq!(default_pattern(&text(None, Flavor::Email), "email"), None);
        // Every default parses for its column.
        for (g, n) in [(&int, "age"), (&int, "qty"), (&Gen::Float, "lng")] {
            assert!(Pattern::parse(g, &default_pattern(g, n).unwrap()).is_ok(), "{n}");
        }
    }

    /// The examples of the dialog's Pattern guide (ui/src/lib/testDataPatterns.ts)
    /// work for their column types.
    #[test]
    fn the_pattern_guide_examples_parse() {
        let int = Gen::Int { min: 0, max: 1_000_000 };
        let money = Gen::Decimal { int_digits: 6, scale: 2 };
        let txt = text(Some(60), Flavor::FullName);
        for (g, p) in [
            (&int, "18..90"),
            (&int, "1..5"),
            (&money, "0.5..99.99"),
            (&Gen::Date, "1950-01-01..2005-12-31"),
            (&Gen::Date, "2024..2025"),
            (&Gen::Time, "08:00..18:00"),
            (&txt, "{gold|silver|bronze}"),
            (&txt, "%@gmail.com"),
            (&txt, "INV-#####"),
            (&txt, "+84 ### ### ###"),
            (&txt, "??-####"),
            (&txt, "%d/%m/%Y"),
            (&txt, "{Mr.|Ms.} %"),
        ] {
            let pat = Pattern::parse(g, p).unwrap_or_else(|e| panic!("{p}: {e}"));
            let v = pat.generate(g, Flavor::FullName, Some(60), &mut Rng(1));
            if let (Pattern::Template(_), Value::Text(t)) = (&pat, &v) {
                assert!(pat.accept(g, t).is_some(), "{p} made {t}");
            }
        }
    }

    #[test]
    fn civil_dates_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2024, 12, 31), (1950, 6, 15)] {
            assert_eq!(civil(days_from_civil(y, m, d)), (y, m, d));
        }
    }
}
