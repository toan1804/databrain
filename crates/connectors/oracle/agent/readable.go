// Columns go-ora can't decode (Oracle object types: SDO_GEOMETRY, XMLTYPE,
// user-defined objects and collections, REFs, native JSON / VECTOR): go-ora
// loses its place in the server's reply. Sometimes it stops with an error
// ("TTC error: received code … during response reading", "invalid size for
// GetInt64"); but when the bytes it misreads look like a valid length, it
// waits for that many more, which never come, and both sides wait forever.
// So a query is described first (no row is read) and its columns of an
// object type are converted to text on the server before it runs
// (readableFirst). The run-again-after-an-error path stays for a query that
// can't be described that way.
package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"strings"

	go_ora "github.com/sijms/go-ora/v2"
)

// undecodable: go-ora failed to decode a value of the result.
func undecodable(err error) bool {
	if err == nil {
		return false
	}
	m := err.Error()
	// "invalid size for GetInt64: 32": go-ora decoding the value of an
	// XMLTYPE table column (one made in the query reads fine). It, the
	// buffer under-run and the unexpected packet all mean go-ora lost its
	// place in the server's reply (the connection is reopened).
	for _, s := range []string{"TTC error", "unable to decode oracle type", "invalid size for Get", "buffer under-run", "abnormal packet type"} {
		if strings.Contains(m, s) {
			return true
		}
	}
	return false
}

// unreadableType: a column type go-ora describes but may not decode.
// Named types (objects, collections, XMLTYPE) are all described as XMLType;
// types it has no name for come out as "TNSType(n)" (e.g. JSON, VECTOR).
func unreadableType(dbType string) bool {
	switch dbType {
	case "XMLType", "OCIXMLType", "OCIRef":
		return true
	}
	return strings.HasPrefix(dbType, "TNSType(")
}

// converters turn a column into text on the server, most specific first;
// the first one Oracle accepts for the column's type is used. A wrong type
// is a parse error, so each is tried on an empty result (`where 1 = 0`).
var converters = []struct{ expr, label string }{
	{"sdo_util.to_wktgeometry(%s)", "SDO_GEOMETRY (WKT)"},
	{"xmlserialize(content %s as clob)", "XMLTYPE"},
	{"json_serialize(%s returning clob)", "JSON"},
	{"from_vector(%s returning clob)", "VECTOR"},
	{"reftohex(%s)", "REF"},
	{"xmlserialize(content sys_xmlgen(%s) as clob)", "OBJECT (XML)"},
}

func unreadableHint(cause error, why error) error {
	msg := fmt.Sprintf("%v\n\nThe thin Oracle driver can't read a column of this result (an object type such as SDO_GEOMETRY or XMLTYPE). "+
		"Convert it in the query, e.g. sdo_util.to_wktgeometry(col) or xmlserialize(content col as clob), "+
		"or switch the connection's driver to Instant Client.", cause)
	if why != nil {
		msg += fmt.Sprintf("\n\n(Converting it automatically failed: %v)", why)
	}
	return fmt.Errorf("%s", msg)
}

// wrap puts the statement in an inline view; the newline ends a trailing
// `--` comment.
func wrap(selectList, stmt, tail string) string {
	return "select " + selectList + " from (\n" + stmt + "\n) q" + tail
}

func quoted(name string) string { return `q."` + name + `"` }

func (s *session) columnTypes(ctx context.Context, r *request, query string) ([]*sql.ColumnType, error) {
	t, err := s.target(ctx)
	if err != nil {
		return nil, err
	}
	rows, err := t.QueryContext(ctx, query, args(r.Binds)...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return rows.ColumnTypes()
}

// readablePlan: the statement with its unreadable columns converted, and
// the db_type to show for them (by position).
func (s *session) readablePlan(ctx context.Context, r *request) (string, map[int]string, error) {
	cts, err := s.columnTypes(ctx, r, wrap("*", r.SQL, " where 1 = 0"))
	if err != nil {
		return "", nil, err
	}
	seen := map[string]bool{}
	list := make([]string, len(cts))
	labels := map[int]string{}
	for i, ct := range cts {
		name := ct.Name()
		if name == "" || strings.Contains(name, `"`) {
			return "", nil, fmt.Errorf("column %d has no usable name; give it an alias", i+1)
		}
		if seen[name] {
			return "", nil, fmt.Errorf("two columns are named %s; give them different aliases", name)
		}
		seen[name] = true
		expr := quoted(name)
		if unreadableType(ct.DatabaseTypeName()) {
			found := false
			for _, c := range converters {
				conv := fmt.Sprintf(c.expr, quoted(name))
				if _, err := s.columnTypes(ctx, r, wrap(conv, r.SQL, " where 1 = 0")); err != nil {
					if ctx.Err() != nil || lostConn(err) {
						return "", nil, err
					}
					continue
				}
				expr, labels[i], found = conv, c.label, true
				break
			}
			if !found {
				return "", nil, fmt.Errorf("no conversion to text works for column %s", name)
			}
		}
		list[i] = expr + ` as "` + name + `"`
	}
	if len(labels) == 0 {
		return "", nil, fmt.Errorf("no column of an object type was found")
	}
	return wrap(strings.Join(list, ", "), r.SQL, ""), labels, nil
}

// retryReadable handles a result go-ora could not decode: it reopens the
// connection (the old one is out of step) and, when nothing was streamed
// yet (the first page failed), runs the query again with the unreadable
// columns converted to text.
func (s *session) retryReadable(ctx context.Context, w *writer, r *request, cause error, streamed bool) error {
	if err := s.reconnect(ctx); err != nil {
		return fmt.Errorf("%w (reconnecting afterwards failed: %v)", unreadableHint(cause, nil), err)
	}
	if streamed {
		return unreadableHint(cause, nil)
	}
	query, labels, err := s.readablePlan(ctx, r)
	if err != nil {
		if ctx.Err() != nil {
			return err
		}
		return unreadableHint(cause, err)
	}
	retry := *r
	retry.SQL, retry.labels = query, labels
	err = s.query(ctx, w, &retry)
	if undecodable(err) {
		_ = s.reconnect(ctx)
		return unreadableHint(cause, err)
	}
	return err
}

// plan is what readableFirst found for a query: the statement to run
// instead ("" = the query as it is) and its converted columns' db_type.
type plan struct {
	sql    string
	labels map[int]string
	refuse error // an object column that can't be converted: not run
}

// maxPlans bounds the remembered plans (a session running ever new SQL).
const maxPlans = 256

// readableFirst describes the query (on an empty result: go-ora reads no
// value) and, when a column is of a type go-ora can't decode, returns it
// with that column converted to text, before anything is read. Remembered
// by the query's text until the session reconnects or runs an exec (DDL may
// change a column's type). A query that can't be described so (a duplicate
// column name, FOR UPDATE, not a SELECT) runs as it is.
func (s *session) readableFirst(ctx context.Context, r *request) (*request, error) {
	if len(r.labels) > 0 {
		return r, nil
	}
	p, ok := s.plans[r.SQL]
	if !ok {
		var sure bool
		p, sure = s.describe(ctx, r)
		if !sure {
			// Not answered (stopped, the connection lost): not remembered.
			return r, nil
		}
		if s.plans == nil || len(s.plans) >= maxPlans {
			s.plans = map[string]plan{}
		}
		s.plans[r.SQL] = p
	}
	if p.refuse != nil {
		return nil, unreadableHint(errors.New("a column of this result has an object type the thin Oracle driver can't read, and it couldn't be converted to text"), p.refuse)
	}
	if p.sql == "" {
		return r, nil
	}
	out := *r
	out.SQL, out.labels = p.sql, p.labels
	return &out, nil
}

// describeSQL parses :1 (a SELECT; parse runs nothing) and returns its
// columns' type codes, each followed by a comma.
const describeSQL = "declare c integer; n integer; d dbms_sql.desc_tab2; r varchar2(32767); " +
	"begin c := dbms_sql.open_cursor; " +
	"begin dbms_sql.parse(c, :1, dbms_sql.native); dbms_sql.describe_columns2(c, n, d); " +
	"exception when others then dbms_sql.close_cursor(c); raise; end; " +
	"for i in 1 .. n loop r := r || d(i).col_type || ','; end loop; " +
	"dbms_sql.close_cursor(c); :2 := r; end;"

// objectTypeCodes: DBMS_SQL's codes of the types go-ora may not decode
// (named types: objects, collections, XMLTYPE, SDO_GEOMETRY; REF; opaque;
// native JSON; VECTOR).
var objectTypeCodes = map[string]bool{"58": true, "109": true, "111": true, "119": true, "127": true}

// isSelect: the statement starts with SELECT or WITH (past comments): only
// those are parsed with DBMS_SQL, which would run DDL at parse.
func isSelect(sql string) bool {
	f := strings.Fields(strings.ToUpper(stripComments(sql)))
	return len(f) > 0 && (f[0] == "SELECT" || f[0] == "WITH" || strings.HasPrefix(f[0], "SELECT(") || strings.HasPrefix(f[0], "("))
}

// hasObjectColumn: whether the query returns a column of an object type,
// asked of the server without running it (DBMS_SQL describes any SELECT,
// one with two columns of a name too). ok: the server answered.
func (s *session) hasObjectColumn(ctx context.Context, query string) (found, ok bool, err error) {
	t, err := s.target(ctx)
	if err != nil {
		return false, false, err
	}
	var codes string
	if _, err := t.ExecContext(ctx, describeSQL, go_ora.Clob{String: query, Valid: true}, go_ora.Out{Dest: &codes, Size: 32767}); err != nil {
		return false, false, err
	}
	for _, c := range strings.Split(codes, ",") {
		if objectTypeCodes[strings.TrimSpace(c)] {
			return true, true, nil
		}
	}
	return false, true, nil
}

// describe finds the query's columns of an object type (see readableFirst);
// sure: the server answered (an error it gave counts: the query runs as it
// is, and fails with that error). A query with such a column that can't be
// converted (two columns of a name, FOR UPDATE) is refused (refuse): run as
// it is, go-ora may wait forever on it.
func (s *session) describe(ctx context.Context, r *request) (plan, bool) {
	unsure := func(err error) bool { return ctx.Err() != nil || lostConn(err) }
	if !isSelect(r.SQL) {
		return plan{}, true
	}
	found, _, err := s.hasObjectColumn(ctx, r.SQL)
	if err != nil {
		return plan{}, !unsure(err)
	}
	if !found {
		return plan{}, true
	}
	query, labels, err := s.readablePlan(ctx, r)
	if err != nil {
		if unsure(err) {
			return plan{}, false
		}
		return plan{refuse: err}, true
	}
	return plan{sql: query, labels: labels}, true
}
