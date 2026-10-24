// Columns go-ora can't decode (Oracle object types: SDO_GEOMETRY, XMLTYPE,
// user-defined objects and collections, REFs, native JSON / VECTOR): go-ora
// stops with "TTC error: received code … during response reading" and the
// connection's protocol stream is out of step. The query is then run again,
// on a new connection, with those columns converted to text on the server.
package main

import (
	"context"
	"database/sql"
	"fmt"
	"strings"
)

// undecodable: go-ora failed to decode a value of the result.
func undecodable(err error) bool {
	if err == nil {
		return false
	}
	m := err.Error()
	return strings.Contains(m, "TTC error") || strings.Contains(m, "unable to decode oracle type")
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
