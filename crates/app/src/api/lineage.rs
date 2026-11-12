//! Column lineage of SQL, checked against the database when asked.

use super::*;

// ------------------------------------------------------------ lineage

/// The database's check of one statement (`EXPLAIN`, not run).
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LineageCheck {
    pub index: usize,
    /// `ok`, `error`, `unsupported` (no cheap check on this engine), `skipped`
    /// (not a query), `offline` (could not connect), `timeout`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message: Option<String>,
    /// UTF-16 offset of the error in the script, when the engine gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LineageView {
    /// Ranges are UTF-16 offsets into the script.
    pub graph: databrain_lineage::Lineage,
    pub checks: Vec<LineageCheck>,
    /// Tables whose columns were read from the server for this view.
    pub described: Vec<String>,
}

/// Same-length stand-in for `:name` so the parser accepts the text and every
/// offset stays that of the editor.
fn mask_parameters(sql: &str, kind: ConnectorKind) -> String {
    let mut out = sql.to_string();
    for p in databrain_connector_core::sql::find_parameters(sql, kind).into_iter().rev() {
        let len = p.end - p.start;
        let fill = if len >= 4 { "NULL" } else { "0" };
        out.replace_range(p.start..p.end, &format!("{fill:<len$}"));
    }
    out
}

/// Byte offset → UTF-16 offset, for every byte of `text`.
fn utf16_table(text: &str) -> Vec<usize> {
    let mut t = vec![0; text.len() + 1];
    let mut u = 0;
    for (b, ch) in text.char_indices() {
        for k in 0..ch.len_utf8() {
            t[b + k] = u;
        }
        u += ch.len_utf16();
    }
    t[text.len()] = u;
    t
}

/// Lineage of `sql` (a query tab or selection): parsed locally with the
/// explorer cache for table columns; when `connection_id` is given and
/// `check` is on, tables whose columns aren't cached are described (at most
/// 20, 8 s) and every query statement is checked with `EXPLAIN` (never run).
pub async fn lineage(
    state: &AppState,
    connection_id: Option<&str>,
    kind: ConnectorKind,
    sql: &str,
    params: std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>,
    check: bool,
) -> Result<LineageView> {
    if !kind.traits().language.is_sql() {
        return Err(invalid(format!("Lineage traces SQL; {} queries aren't supported", kind.traits().name)));
    }
    let ws = state.engine.workspace().clone();
    let masked = mask_parameters(sql, kind);
    // View definitions read from the server for this view (lower-case
    // `schema.name` → DDL; empty = asked, none to trace).
    let mut views: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let cat = |id: Option<String>, views: &std::collections::HashMap<String, String>| {
        let ws = ws.clone();
        let views = views.clone();
        move |parts: &[String]| -> Option<databrain_lineage::CatalogTable> {
            let id = id.as_deref()?;
            let (schema, name) = ws.meta_get(id, &parts.join(".")).ok().flatten()?;
            let columns = ws.kn_column_names(id, &schema, &name).ok().flatten()?;
            let view_sql = views.get(&format!("{schema}.{name}").to_lowercase()).filter(|d| !d.trim().is_empty()).cloned();
            Some(databrain_lineage::CatalogTable { name: format!("{schema}.{name}"), columns, view_sql })
        }
    };
    let conn = connection_id.map(str::to_string);
    let mut graph = databrain_lineage::analyze(&masked, kind, &cat(conn.clone(), &views));
    let mut described = Vec::new();
    if let (Some(id), true) = (connection_id, check) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut tried: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Tables whose cached columns may be out of date (their schema changed
        // since): read again first. Without `check`, or if that fails, the
        // cached columns are used as they are.
        let mut tables: Vec<String> = graph.nodes.iter().filter(|n| n.kind == databrain_lineage::NodeKind::Table && !n.view).map(|n| n.name.clone()).collect();
        tables.sort();
        tables.dedup();
        let mut refreshed = false;
        for t in tables.into_iter().take(20) {
            let Some((schema, name)) = ws.meta_get(id, &t).ok().flatten() else { continue };
            if !ws.meta_table(id, &schema, &name).ok().flatten().is_some_and(|c| c.stale) {
                continue;
            }
            match tokio::time::timeout_at(deadline, describe(state, id, &schema, &name)).await {
                Ok(Ok(d)) if !d.columns.is_empty() => {
                    described.push(format!("{schema}.{name}"));
                    refreshed = true;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if refreshed {
            state.meta.flush().await;
            graph = databrain_lineage::analyze(&masked, kind, &cat(conn.clone(), &views));
        }
        // Rounds: describe the tables whose columns aren't known, then read the
        // definitions of the views among them; a view's definition names more
        // tables (and views), traced in the next round.
        'rounds: for _ in 0..4 {
            let mut changed = false;
            for t in graph.unknown_tables.iter().filter(|t| !tried.contains(&t.to_lowercase())).take(20).cloned().collect::<Vec<_>>() {
                tried.insert(t.to_lowercase());
                let (schema, name) = match ws.meta_get(id, &t).ok().flatten() {
                    Some(x) => x,
                    None => match t.rsplit_once('.') {
                        Some((s, n)) => (s.to_string(), n.to_string()),
                        // Bare name not seen yet: find it (cached for next time).
                        None => match tokio::time::timeout_at(deadline, search_objects(state, id, &t, Some(20))).await {
                            Ok(Ok(hits)) => match hits.into_iter().find(|o| o.name.eq_ignore_ascii_case(&t)) {
                                Some(o) => (o.schema, o.name),
                                None => continue,
                            },
                            Err(_) => break 'rounds,
                            _ => continue,
                        },
                    },
                };
                match tokio::time::timeout_at(deadline, describe(state, id, &schema, &name)).await {
                    Ok(Ok(d)) if !d.columns.is_empty() => {
                        // Known now, also to later lookups by this name, with its kind.
                        let mut obj = d.object.clone();
                        (obj.schema, obj.name) = (schema.clone(), name.clone());
                        if !matches!(obj.kind, databrain_connector_core::ObjectKind::View | databrain_connector_core::ObjectKind::MaterializedView) {
                            obj.kind = databrain_connector_core::ObjectKind::Table;
                        }
                        let _ = ws.meta_add_objects(id, &[obj]);
                        described.push(format!("{schema}.{name}"));
                        changed = true;
                    }
                    Ok(_) => {}
                    Err(_) => break 'rounds,
                }
            }
            if changed {
                // The described columns are read back from the cache.
                state.meta.flush().await;
                graph = databrain_lineage::analyze(&masked, kind, &cat(conn.clone(), &views));
            }
            // Views the SQL (or a view's definition) reads: their definitions.
            let mut names: Vec<String> =
                graph.nodes.iter().filter(|n| n.kind == databrain_lineage::NodeKind::Table && !n.written && !n.view).map(|n| n.name.clone()).collect();
            names.sort();
            names.dedup();
            let mut read_views = false;
            for t in names.into_iter().take(20) {
                let Some((schema, name)) = ws.meta_get(id, &t).ok().flatten() else { continue };
                let key = format!("{schema}.{name}").to_lowercase();
                if views.contains_key(&key) {
                    continue;
                }
                let Some(obj) = ws.meta_object(id, &schema, &name).ok().flatten() else { continue };
                if !matches!(obj.kind, databrain_connector_core::ObjectKind::View | databrain_connector_core::ObjectKind::MaterializedView) {
                    continue;
                }
                match tokio::time::timeout_at(deadline, object_ddl(state, id, &schema, &name, obj.kind)).await {
                    Ok(r) => {
                        let ddl = r.ok().flatten().unwrap_or_default();
                        read_views |= !ddl.trim().is_empty();
                        views.insert(key, ddl);
                    }
                    Err(_) => break 'rounds,
                }
            }
            if read_views {
                graph = databrain_lineage::analyze(&masked, kind, &cat(conn.clone(), &views));
            }
            if !changed && !read_views {
                break;
            }
        }
    }

    let mut checks = Vec::new();
    for st in &graph.statements {
        let mut c = LineageCheck { index: st.index, status: String::new(), message: None, position: None };
        match (connection_id, check, st.query) {
            (_, _, None) if st.kind == "error" => {
                c.status = "error".into();
                c.message = st.error.clone();
            }
            (None, _, _) | (_, false, _) => c.status = "skipped".into(),
            (_, _, None) => c.status = "skipped".into(),
            (Some(id), true, Some(q)) => {
                let text = &sql[q.start..q.end];
                let bound = databrain_connector_core::sql::substitute_parameters(text, kind, &params).unwrap_or_else(|_| masked[q.start..q.end].to_string());
                // What is sent is checked: a raw parameter value (`1; drop
                // table t`) can change what the statement does.
                let read = [text, bound.as_str()]
                    .iter()
                    .all(|t| databrain_connector_core::sql::classify(t, kind).kind == databrain_connector_core::sql::StatementKind::Read);
                if !read {
                    c.status = "skipped".into();
                } else {
                    let same = bound == text;
                    match state.engine.check_sql(id, &bound).await {
                        Ok(true) => c.status = "ok".into(),
                        Ok(false) => c.status = "unsupported".into(),
                        Err(e) => {
                            c.status = match e.kind.as_str() {
                                "connection" | "auth" => "offline",
                                "timeout" => "timeout",
                                _ => "error",
                            }
                            .into();
                            c.message = Some(e.message.clone());
                            // 1-based character → byte in the script (only when no value changed the text).
                            c.position = e.position.filter(|_| same).and_then(|p| text.char_indices().nth(p as usize - 1)).map(|(b, _)| q.start + b);
                        }
                    }
                    if c.status == "offline" {
                        checks.push(c);
                        // Don't try every statement against a server that isn't there.
                        break;
                    }
                }
            }
        }
        checks.push(c);
    }

    // Bytes → UTF-16 for the UI.
    let t = utf16_table(sql);
    let at = |b: usize| t[b.min(sql.len())];
    let fix = |r: &mut databrain_lineage::Range| {
        r.start = at(r.start);
        r.end = at(r.end);
    };
    for n in &mut graph.nodes {
        n.span.iter_mut().for_each(fix);
        for c in &mut n.columns {
            c.span.iter_mut().for_each(fix);
        }
    }
    for e in &mut graph.edges {
        e.span.iter_mut().for_each(fix);
    }
    for s in &mut graph.statements {
        fix(&mut s.range);
        s.query.iter_mut().for_each(fix);
    }
    for w in &mut graph.warnings {
        w.span.iter_mut().for_each(fix);
    }
    for c in &mut checks {
        c.position = c.position.map(at);
    }
    Ok(LineageView { graph, checks, described })
}
