//! Finding a saved connection by what someone typed.

use crate::ConnectionProfile;

/// The kind of the connection a combined query's part names (`@name` or
/// `@id:…`), as the run finds it; `None` when none does (the splitter then
/// reads the part with the tab's dialect, and the run refuses it).
pub fn part_kind(all: &[ConnectionProfile], r: &databrain_connector_core::sql::ConnectionRef) -> Option<databrain_connector_core::ConnectorKind> {
    use databrain_connector_core::sql::ConnectionRef;
    match r {
        ConnectionRef::Id(id) => all.iter().find(|c| c.id == *id),
        ConnectionRef::Name(n) => find_connection(all, n, "").ok(),
    }
    .map(|c| c.config.kind)
}

/// The saved connection `name` names: its name in any case (spaces around
/// it ignored) or its id, nothing else. A part of a name or a typo picks
/// none, so a command never runs on a connection that wasn't meant: the
/// error offers the names close to it, then `listing` (where to see them
/// all). `dbr -c` and the parts of a combined query (`@name`) find theirs so.
pub fn find_connection<'a>(all: &'a [ConnectionProfile], name: &str, listing: &str) -> Result<&'a ConnectionProfile, String> {
    let typed = name.trim();
    let low = typed.to_lowercase();
    // The id first (what can't be two), then the name as spelled, then in any case.
    if let Some(c) = all.iter().find(|c| c.id == typed) {
        return Ok(c);
    }
    for same in
        [all.iter().filter(|c| c.name.trim() == typed).collect::<Vec<_>>(), all.iter().filter(|c| c.name.trim().to_lowercase() == low).collect::<Vec<_>>()]
    {
        match same.as_slice() {
            [] => {}
            [one] => return Ok(one),
            many => {
                return Err(format!(
                    "several connections are named {name:?}: give one's id ({})",
                    many.iter().map(|c| format!("{:?} is {}", c.name, c.id)).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    if all.is_empty() {
        return Err(format!("no connection named {name:?}: none is saved (add one in DataBrain)"));
    }
    let close = close_names(all.iter().map(|c| c.name.as_str()), &low);
    let hint = match close.as_slice() {
        [] => String::new(),
        [one] => format!("; did you mean {one:?}?"),
        many => format!("; did you mean one of {}?", many.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ")),
    };
    Err(format!("no connection named {name:?}{hint}{listing}"))
}

/// Up to 5 of `names` close to `typed` (lower case): holding it or held in
/// it, or a few letters off; the closest first.
fn close_names<'a>(names: impl Iterator<Item = &'a str>, typed: &str) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &str)> = names
        .filter_map(|n| {
            let low = n.trim().to_lowercase();
            let d = edit_distance(&low, typed);
            // Held in what was typed only when not a letter or two (`a` is in most words).
            let part = !typed.is_empty() && (low.contains(typed) || (low.chars().count() >= 3 && typed.contains(&low)));
            (part || d <= (typed.chars().count() / 3).max(1)).then_some((if part { 0 } else { d }, n))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(5).map(|(_, n)| n).collect()
}

/// Letters to add, remove, change or swap with the next to turn `a` into
/// `b` (Damerau-Levenshtein, optimal string alignment: `saels` is one from
/// `sales`).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    // d[i][j]: the distance between a[..i] and b[..j].
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, v) in d[0].iter_mut().enumerate() {
        *v = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_counts_swaps_as_one() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "ab"), 2);
        assert_eq!(edit_distance("saels", "sales"), 1);
    }
}
