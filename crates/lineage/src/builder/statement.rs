use sqlparser::ast::{
    AlterTableOperation, Assignment, AssignmentTarget, Delete, Expr, FromTable, Merge, MergeAction, MergeInsertKind, MergeUpdateKind, ObjectName, ObjectType,
    Query, SetExpr, Statement, TableFactor, TableObject, Update, UpdateTableFromKind,
};
use sqlparser::tokenizer::Span;
use std::ops::ControlFlow;

use super::*;
use crate::*;

impl<'a> Builder<'a> {
    // ---- statements

    pub(super) fn statement(&mut self, st: &Statement, range: Range, many: bool) {
        let qrange = |b: &Self, q: &Query| b.range(q.span());
        let index = self.stmt;
        let info =
            move |kind: &str, query: Option<Range>, result: Option<String>| StatementInfo { index, kind: kind.into(), range, query, result, error: None };
        match st {
            Statement::Query(q) if select_into(q).is_some() => {
                let target = select_into(q).unwrap();
                let id = self.write_into(&target.0, q, &[], true, &Env::default());
                if target.1
                    && let Some(t) = self.g.nodes.iter().position(|n| n.id == id)
                {
                    self.g.nodes[t].temporary = true;
                }
                self.g.statements.push(info("select_into", Some(range), Some(id)));
            }
            Statement::Query(q) if matches!(*q.body, SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_)) => {
                // `WITH … INSERT/UPDATE/DELETE/MERGE`: the statement reads the CTEs.
                let env = self.with_ctes(q, &Env::default());
                match &*q.body {
                    SetExpr::Insert(Statement::Insert(ins)) if ins.source.is_some() => {
                        let src = ins.source.as_ref().unwrap();
                        let TableObject::TableName(name) = &ins.table else {
                            self.other(st, range);
                            return;
                        };
                        let names: Vec<String> = ins.columns.iter().map(|c| name_parts(c).last().cloned().unwrap_or_default()).collect();
                        let id = self.write_into(name, src, &names, false, &env);
                        let qr = qrange(self, src);
                        self.g.statements.push(info("insert", qr, Some(id)));
                    }
                    SetExpr::Update(Statement::Update(u)) => {
                        let id = self.update(u, &env);
                        self.g.statements.push(info("update", None, id));
                    }
                    SetExpr::Delete(Statement::Delete(d)) => {
                        let id = self.delete(d, &env);
                        self.g.statements.push(info("delete", None, id));
                    }
                    SetExpr::Merge(Statement::Merge(m)) => {
                        let id = self.merge(m, &env);
                        self.g.statements.push(info("merge", None, id));
                    }
                    _ => self.other(st, range),
                }
            }
            Statement::Query(q) => {
                let r = self.query(q, &Env::default());
                let r = self.as_result(r);
                self.g.nodes[r].name = if many { format!("Result {}", self.stmt + 1) } else { "Result".into() };
                let id = self.g.nodes[r].id.clone();
                let q = Some(range);
                self.g.statements.push(info("select", q, Some(id)));
            }
            Statement::CreateTable(ct) if ct.query.is_some() => {
                let q = ct.query.as_ref().unwrap();
                let names: Vec<String> = ct.columns.iter().map(|c| c.name.value.clone()).collect();
                let defines = !self.exists_already(&ct.name, ct.if_not_exists);
                let id = self.write_into(&ct.name, q, &names, defines, &Env::default());
                if ct.temporary
                    && let Some(t) = self.g.nodes.iter().position(|n| n.id == id)
                {
                    self.g.nodes[t].temporary = true;
                }
                let qr = qrange(self, q);
                self.g.statements.push(info("create_table_as", qr, Some(id)));
            }
            Statement::CreateTable(ct) => {
                let keep = self.exists_already(&ct.name, ct.if_not_exists);
                let t = self.table_node(&ct.name);
                let names: Vec<String> = ct.columns.iter().map(|c| c.name.value.clone()).collect();
                if !keep {
                    self.created.insert(t);
                }
                if !names.is_empty() && !keep {
                    self.define_columns(t, &names);
                }
                if let Some(like) = ct.like.as_ref().map(|l| match l {
                    sqlparser::ast::CreateTableLikeKind::Parenthesized(l) | sqlparser::ast::CreateTableLikeKind::Plain(l) => &l.name,
                }) {
                    // `CREATE TABLE t (LIKE s)`: same columns, no rows.
                    let s = self.table_node(like);
                    let cols: Vec<String> = self.g.nodes[s].columns.iter().map(|c| c.name.clone()).collect();
                    if self.g.nodes[s].columns_known && !keep {
                        self.define_columns(t, &cols);
                    }
                }
                self.g.nodes[t].written |= !keep;
                self.g.nodes[t].temporary |= ct.temporary && !keep;
                let id = self.g.nodes[t].id.clone();
                self.g.statements.push(info("create_table", None, Some(id)));
            }
            Statement::CreateView(v) => {
                let names: Vec<String> = v.columns.iter().map(|c| c.name.value.clone()).collect();
                let id = self.write_into(&v.name, &v.query, &names, true, &Env::default());
                let qr = qrange(self, &v.query);
                self.g.statements.push(info("create_view", qr, Some(id)));
            }
            Statement::Insert(ins) if ins.source.is_some() => {
                let q = ins.source.as_ref().unwrap();
                let TableObject::TableName(name) = &ins.table else {
                    self.other(st, range);
                    return;
                };
                let names: Vec<String> = ins.columns.iter().map(|c| name_parts(c).last().cloned().unwrap_or_default()).collect();
                let id = self.write_into(name, q, &names, false, &Env::default());
                let qr = qrange(self, q);
                self.g.statements.push(info("insert", qr, Some(id)));
            }
            Statement::Update(u) => {
                let id = self.update(u, &Env::default());
                self.g.statements.push(info("update", None, id));
            }
            Statement::Delete(d) => {
                let id = self.delete(d, &Env::default());
                self.g.statements.push(info("delete", None, id));
            }
            Statement::Merge(m) => {
                let id = self.merge(m, &Env::default());
                self.g.statements.push(info("merge", None, id));
            }
            Statement::Drop { object_type: ObjectType::Table | ObjectType::View | ObjectType::MaterializedView, .. } => {
                // Nothing flows; a table only dropped isn't drawn (nor described).
                self.g.statements.push(info("drop", None, None));
            }
            Statement::Truncate(t) => {
                for n in &t.table_names {
                    let i = self.table_node(&n.name);
                    self.g.nodes[i].written = true;
                }
                self.g.statements.push(info("truncate", None, None));
            }
            Statement::AlterTable(a) => {
                let t = self.table_node(&a.name);
                for op in &a.operations {
                    match op {
                        AlterTableOperation::AddColumn { column_def, .. } => {
                            self.ensure_col(t, &column_def.name.value);
                        }
                        AlterTableOperation::RenameTable { .. } => {
                            let span = Some(range);
                            self.warn("ALTER TABLE … RENAME is not traced: the new name is a table of its own".into(), span);
                        }
                        _ => {}
                    }
                }
                let id = self.g.nodes[t].id.clone();
                self.g.statements.push(info("alter", None, Some(id)));
            }
            _ => self.other(st, range),
        }
    }

    /// Unsupported statement: its tables, without lineage.
    pub(super) fn other(&mut self, st: &Statement, range: Range) {
        let mut names = Vec::new();
        let _ = sqlparser::ast::visit_relations(st, |r| {
            names.push(r.clone());
            ControlFlow::<()>::Continue(())
        });
        for n in &names {
            self.table_node(n);
        }
        let kw = st.to_string().split_whitespace().next().unwrap_or("").to_uppercase();
        self.warn(format!("{kw} statements are not traced yet; their tables are shown without columns"), Some(range));
        self.g.statements.push(StatementInfo { index: self.stmt, kind: "other".into(), range, query: None, result: None, error: None });
    }

    /// `CREATE TABLE/VIEW … AS q` / `INSERT INTO t (cols) q`: q's columns feed t's.
    /// `defines`: the statement creates the table, so its columns are exactly these.
    pub(super) fn write_into(&mut self, name: &ObjectName, q: &Query, names: &[String], defines: bool, env: &Env) -> String {
        let r = self.query(q, env);
        let t = self.table_node(name);
        self.g.nodes[t].written = true;
        let outs: Vec<String> = self.g.nodes[r].columns.iter().map(|c| c.name.clone()).collect();
        if defines {
            // Its columns are the query's (renamed by a column list), even
            // when the catalog knew an older version of the table.
            let defined: Vec<String> = outs.iter().enumerate().map(|(i, o)| names.get(i).cloned().unwrap_or_else(|| o.clone())).collect();
            self.define_columns(t, &defined);
        }
        let known: Vec<String> = self.g.nodes[t].columns.iter().map(|c| c.name.clone()).collect();
        for (i, o) in outs.iter().enumerate() {
            // Target column: listed names, else the table's own order (INSERT), else the query's name.
            let target = names.get(i).cloned().or_else(|| (!names.is_empty()).then(String::new)).filter(|s| !s.is_empty());
            let target = target.or_else(|| known.get(i).cloned()).unwrap_or_else(|| o.clone());
            let tc = self.ensure_col(t, &target);
            self.edge(r, Some(o.clone()), t, Some(tc), EdgeKind::Direct, None);
        }
        self.g.nodes[t].id.clone()
    }

    /// `UPDATE t SET c = e … [FROM …] [WHERE …]`: what `e` reads feeds `t.c`;
    /// the `FROM` tables' join and `WHERE` columns decide which rows change.
    pub(super) fn update(&mut self, u: &Update, env: &Env) -> Option<String> {
        let env = env.clone();
        let TableFactor::Table { name, alias, .. } = &u.table.relation else {
            self.warn("UPDATE of something other than a table is not traced".into(), self.range(u.table.span()));
            return None;
        };
        let t = self.table_node(name);
        self.g.nodes[t].written = true;
        let at = self.g.edges.len();
        let mut sources = vec![Source { alias: alias_or_name(alias, name), node: t }];
        self.add_joins(&u.table.joins, &env, &mut sources, t);
        if let Some(UpdateTableFromKind::BeforeSet(f) | UpdateTableFromKind::AfterSet(f)) = &u.from {
            for twj in f {
                self.add_from(twj, &env, &mut sources, t);
            }
        }
        let scope = Env { ctes: env.ctes.clone(), outer: sources.clone() };
        for a in &u.assignments {
            self.assign(t, a, &sources, &scope);
        }
        if let Some(w) = &u.selection {
            let at = self.g.edges.len();
            self.rows(t, w, EdgeKind::Filter, &sources, &scope);
            self.detail_since(at, format!("UPDATE … WHERE {w}"));
        }
        self.drop_self_edges(at);
        Some(self.g.nodes[t].id.clone())
    }

    /// `SET c = e` (or `(a, b) = (…)`) of an UPDATE or MERGE into `t`; a
    /// qualified target (`SET c.name = …` of MySQL's `UPDATE a JOIN c`)
    /// writes into the table it names.
    pub(super) fn assign(&mut self, t: usize, a: &Assignment, sources: &[Source], env: &Env) {
        let targets: Vec<&ObjectName> = match &a.target {
            AssignmentTarget::ColumnName(n) => vec![n],
            AssignmentTarget::Tuple(v) => v.iter().collect(),
        };
        let values: Vec<&Expr> = match (&a.value, targets.len()) {
            (Expr::Tuple(v), n) if v.len() == n => v.iter().collect(),
            (v, n) => vec![v; n],
        };
        for (target, value) in targets.into_iter().zip(values) {
            let parts = name_parts(target);
            let col = parts.last().cloned().unwrap_or_default();
            let into = match parts.len() {
                n if n > 1 => self.find_source(sources, &parts[..n - 1]).map(|s| s.node).unwrap_or(t),
                _ => t,
            };
            self.g.nodes[into].written = true;
            self.value_into(into, &col, value, sources, env);
        }
    }

    /// Column `col` of table `t` gets the value of `e`.
    pub(super) fn value_into(&mut self, t: usize, col: &str, e: &Expr, sources: &[Source], env: &Env) {
        let tc = self.ensure_col(t, col);
        let r = refs(e);
        let kind = if is_plain(e) {
            EdgeKind::Direct
        } else if r.aggregate {
            EdgeKind::Aggregate
        } else {
            EdgeKind::Transform
        };
        for parts in &r.cols {
            if let Some((src, c)) = self.resolve(sources, &env.outer, parts) {
                let s = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
                self.edge(src, Some(c), t, Some(tc.clone()), kind, s);
            }
        }
        for q in &r.subqueries {
            let m = self.query(q, env);
            let first = self.g.nodes[m].columns.first().map(|c| c.name.clone());
            let s = self.range(q.span());
            self.edge(m, first, t, Some(tc.clone()), EdgeKind::Transform, s);
        }
    }

    /// `DELETE FROM t [USING …] WHERE …`: the other tables' columns in the
    /// join and `WHERE` decide which rows of `t` go.
    pub(super) fn delete(&mut self, d: &Delete, env: &Env) -> Option<String> {
        let env = env.clone();
        let from = match &d.from {
            FromTable::WithFromKeyword(v) | FromTable::WithoutKeyword(v) => v,
        };
        // Tables of the FROM by alias and name (MySQL `DELETE t1 FROM orders t1 JOIN …`).
        let mut named: Vec<(String, ObjectName)> = Vec::new();
        for twj in from {
            for f in std::iter::once(&twj.relation).chain(twj.joins.iter().map(|j| &j.relation)) {
                if let TableFactor::Table { name, alias, .. } = f {
                    named.push((alias_or_name(alias, name), name.clone()));
                }
            }
        }
        let resolve = |n: &ObjectName| -> ObjectName {
            let parts = name_parts(n);
            match &parts[..] {
                [one] => named.iter().find(|(a, _)| a.eq_ignore_ascii_case(one)).map(|(_, n)| n.clone()).unwrap_or_else(|| n.clone()),
                _ => n.clone(),
            }
        };
        let targets: Vec<ObjectName> =
            if d.tables.is_empty() { named.first().map(|(_, n)| n.clone()).into_iter().collect() } else { d.tables.iter().map(resolve).collect() };
        let Some(target) = targets.first().cloned() else {
            self.warn("DELETE from something other than a table is not traced".into(), self.range(d.delete_token.0.span));
            return None;
        };
        let t = self.table_node(&target);
        for n in &targets {
            let i = self.table_node(n);
            self.g.nodes[i].written = true;
        }
        let at = self.g.edges.len();
        let mut sources = Vec::new();
        for twj in from.iter().chain(d.using.iter().flatten()) {
            self.add_from(twj, &env, &mut sources, t);
        }
        if sources.is_empty() {
            sources.push(Source { alias: name_parts(&target).last().cloned().unwrap_or_default().to_lowercase(), node: t });
        }
        let scope = Env { ctes: env.ctes.clone(), outer: sources.clone() };
        if let Some(w) = &d.selection {
            let at = self.g.edges.len();
            self.rows(t, w, EdgeKind::Filter, &sources, &scope);
            self.detail_since(at, format!("DELETE … WHERE {w}"));
        }
        self.drop_self_edges(at);
        Some(self.g.nodes[t].id.clone())
    }

    /// `MERGE INTO t USING s ON … WHEN MATCHED THEN UPDATE SET … WHEN NOT
    /// MATCHED THEN INSERT (…) VALUES (…)`: the source's columns feed the
    /// target's; the `ON` columns join, `WHEN … AND` conditions filter.
    pub(super) fn merge(&mut self, m: &Merge, env: &Env) -> Option<String> {
        let env = env.clone();
        let TableFactor::Table { name, alias, .. } = &m.table else {
            self.warn("MERGE into something other than a table is not traced".into(), self.range(m.table.span()));
            return None;
        };
        let t = self.table_node(name);
        self.g.nodes[t].written = true;
        let at = self.g.edges.len();
        let mut sources = vec![Source { alias: alias_or_name(alias, name), node: t }];
        self.factor(&m.source, &env, &mut sources, t);
        let scope = Env { ctes: env.ctes.clone(), outer: sources.clone() };
        let e = self.g.edges.len();
        self.rows(t, &m.on, EdgeKind::Join, &sources, &scope);
        self.detail_since(e, format!("MERGE … USING {} ON {}", m.source, m.on));
        for c in &m.clauses {
            if let Some(p) = &c.predicate {
                let e = self.g.edges.len();
                self.rows(t, p, EdgeKind::Filter, &sources, &scope);
                self.detail_since(e, format!("WHEN {} AND {p}", c.clause_kind));
            }
            match &c.action {
                MergeAction::Update(u) => match &u.kind {
                    MergeUpdateKind::Set(asg) => {
                        for a in asg {
                            self.assign(t, a, &sources, &scope);
                        }
                    }
                    MergeUpdateKind::Wildcard => self.same_names_into(t, &sources),
                },
                MergeAction::Insert(ins) => match &ins.kind {
                    MergeInsertKind::Values(v) => {
                        let known: Vec<String> = self.g.nodes[t].columns.iter().map(|c| c.name.clone()).collect();
                        if let Some(row) = v.rows.first() {
                            for (i, e) in row.iter().enumerate() {
                                let col = ins.columns.get(i).and_then(|c| name_parts(c).last().cloned()).or_else(|| known.get(i).cloned());
                                match col {
                                    Some(col) => self.value_into(t, &col, e, &sources, &scope),
                                    None => self.warn(
                                        format!("MERGE … INSERT: value {} has no column (list them, or cache {})", i + 1, self.g.nodes[t].name),
                                        self.range(e.span()),
                                    ),
                                }
                            }
                        }
                    }
                    _ => self.same_names_into(t, &sources),
                },
                _ => {}
            }
        }
        self.drop_self_edges(at);
        Some(self.g.nodes[t].id.clone())
    }

    /// `UPDATE *` / `INSERT ROW`: every source column into the target column of the same name.
    pub(super) fn same_names_into(&mut self, t: usize, sources: &[Source]) {
        for s in sources.iter().filter(|s| s.node != t) {
            for c in self.g.nodes[s.node].columns.clone() {
                let tc = self.ensure_col(t, &c.name);
                self.edge(s.node, Some(c.name), t, Some(tc), EdgeKind::Direct, None);
            }
        }
    }

    /// A statement's final relation as a Result node of its own.
    pub(super) fn as_result(&mut self, r: usize) -> usize {
        if matches!(self.g.nodes[r].kind, NodeKind::Subquery | NodeKind::SetOp) {
            self.g.nodes[r].kind = NodeKind::Result;
            return r;
        }
        let n = self.node(NodeKind::Result, "Result".into(), None);
        for c in self.g.nodes[r].columns.clone() {
            let o = self.add_out(n, c.name.clone(), None, None);
            self.edge(r, Some(c.name), n, Some(o), EdgeKind::Direct, None);
        }
        n
    }
}
