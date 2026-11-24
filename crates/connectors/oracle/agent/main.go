// databrain-oracle-agent: a small helper process that talks to Oracle with
// go-ora (pure Go, no Oracle Instant Client) on behalf of DataBrain.
//
// Protocol (stdin → requests, stdout → responses), one frame per message:
//
//	u32 big-endian length | u8 kind | payload
//
// kind 'J': a JSON object. kind 'B': a batch of rows for request `id`:
//
//	u64 id | u32 ncols | u32 nrows | values, row-major, each:
//	  u8 tag: 0 null, 1 i64, 2 f64, 3 text, 4 bytes, 5 timestamp µs (wall clock),
//	          6 timestamp µs (UTC instant), 7 bool
//	  i64/f64/µs: 8 bytes little-endian; text/bytes: u32 LE length + bytes; bool: 1 byte
//
// {"op":"execmany","sql":…,"columns":[[…],…]} runs one INSERT with every
// column's values (null: NULL) bound as arrays (array DML), all or nothing.
//
// Requests run one at a time, except {"op":"cancel","target":id}, which
// cancels the running request (go-ora sends a break to the server).
// The password arrives over stdin, never on the command line.
package main

import (
	"bufio"
	"context"
	"database/sql"
	"database/sql/driver"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	go_ora "github.com/sijms/go-ora/v2"
	"github.com/sijms/go-ora/v2/network"
)

// Version is reported by {"op":"hello"}; the DataBrain side checks it.
const Version = "1"

type request struct {
	ID     uint64 `json:"id"`
	Op     string `json:"op"`
	Target uint64 `json:"target,omitempty"`

	// connect
	User          string            `json:"user,omitempty"`
	Password      string            `json:"password,omitempty"`
	Host          string            `json:"host,omitempty"`
	Port          int               `json:"port,omitempty"`
	Service       string            `json:"service,omitempty"`
	ConnectString string            `json:"connect_string,omitempty"`
	Options       map[string]string `json:"options,omitempty"`
	ReadOnly      bool              `json:"read_only,omitempty"`

	// query / exec / text
	SQL       string             `json:"sql,omitempty"`
	Binds     map[string]*string `json:"binds,omitempty"`
	BatchSize int                `json:"batch_size,omitempty"`
	MaxRows   int                `json:"max_rows,omitempty"`
	// exec: read DBMS_OUTPUT afterwards ("output" in the reply).
	Output bool `json:"output,omitempty"`

	// execmany: every column's values (nil: NULL), bound as arrays to SQL.
	Columns [][]*string `json:"columns,omitempty"`

	// db_type shown for columns converted to text by a retry (by position).
	labels map[int]string
}

type column struct {
	Name   string `json:"name"`
	Type   string `json:"type"`
	DbType string `json:"db_type"`
}

// ---------------------------------------------------------------- framing

type writer struct {
	mu sync.Mutex
	w  *bufio.Writer
	// Frames written so far (a query is only retried before its first one).
	frames uint64
}

// maxFrame is the largest frame either side sends or accepts (the
// header's length is 32 bits; a row larger than this is refused). Kept in
// sync with MAX_FRAME in src/agent.rs; a variable for tests.
var maxFrame = 1 << 30

func (w *writer) frame(kind byte, payload []byte) error {
	if len(payload) > maxFrame {
		return fmt.Errorf("a reply of %d bytes is larger than the %d bytes a frame holds", len(payload), maxFrame)
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	w.frames++
	var h [5]byte
	binary.BigEndian.PutUint32(h[:4], uint32(len(payload)))
	h[4] = kind
	if _, err := w.w.Write(h[:]); err != nil {
		return err
	}
	if _, err := w.w.Write(payload); err != nil {
		return err
	}
	return w.w.Flush()
}

func (w *writer) json(v any) error {
	b, err := json.Marshal(v)
	if err != nil {
		return err
	}
	return w.frame('J', b)
}

// maxRequest is the largest request handled (MAX_REQUEST in src/agent.rs):
// a larger one is read up to maxFrame and answered with an error, so the
// helper goes on instead of losing its place in the stream.
var maxRequest = 64 << 20

func readFrame(r *bufio.Reader) (byte, []byte, error) {
	var h [5]byte
	if _, err := io.ReadFull(r, h[:]); err != nil {
		return 0, nil, err
	}
	n := binary.BigEndian.Uint32(h[:4])
	if int64(n) > int64(maxFrame) {
		return 0, nil, fmt.Errorf("frame too large: %d", n)
	}
	b := make([]byte, n)
	if _, err := io.ReadFull(r, b); err != nil {
		return 0, nil, err
	}
	return h[4], b, nil
}

// ---------------------------------------------------------------- errors

func errReply(id uint64, err error) map[string]any {
	m := map[string]any{"id": id, "type": "error", "message": err.Error()}
	var oe *network.OracleError
	if errors.As(err, &oe) {
		m["code"] = fmt.Sprintf("ORA-%05d", oe.ErrCode)
		m["message"] = strings.TrimSpace(oe.ErrMsg)
		if p := oe.ErrPos(); p > 0 {
			m["offset"] = p
		}
	}
	if errors.Is(err, context.Canceled) {
		m["code"] = "cancelled"
	}
	var out *outputError
	if errors.As(err, &out) {
		m["output"] = out.lines
		m["output_more"] = out.more
	}
	return m
}

// outputError: a failed statement and the DBMS_OUTPUT it wrote first.
type outputError struct {
	err   error
	lines []string
	more  bool
}

func (e *outputError) Error() string { return e.err.Error() }
func (e *outputError) Unwrap() error { return e.err }

// ---------------------------------------------------------------- connection

type session struct {
	db   *sql.DB
	conn *sql.Conn
	tx   *sql.Tx // read-only connections run everything in a READ ONLY transaction
	ro   bool
	// The connect request, to reconnect after the connection is lost.
	req *request
	// ALTER SESSION statements run so far, replayed after a reconnect
	// (e.g. current_schema, which unqualified names depend on), in the
	// order they last ran; see remember.
	setup []setupStmt
	// plans: each query's columns of an object type, converted before it
	// runs (see readableFirst), by its text.
	plans map[string]plan
}

// setupStmt is a remembered ALTER SESSION statement and what it sets.
type setupStmt struct {
	sql  string
	keys []string
}

// isAlterSession: ALTER SESSION, past leading comments.
func isAlterSession(sql string) bool {
	f := strings.Fields(strings.ToUpper(stripComments(sql)))
	return len(f) >= 2 && f[0] == "ALTER" && f[1] == "SESSION"
}

// stripComments drops leading whitespace and `--` / `/* */` comments.
func stripComments(sql string) string {
	for {
		sql = strings.TrimLeft(sql, " \t\r\n")
		switch {
		case strings.HasPrefix(sql, "--"):
			i := strings.IndexByte(sql, '\n')
			if i < 0 {
				return ""
			}
			sql = sql[i+1:]
		case strings.HasPrefix(sql, "/*"):
			i := strings.Index(sql[2:], "*/")
			if i < 0 {
				return ""
			}
			sql = sql[i+4:]
		default:
			return sql
		}
	}
}

// sessionKeys: what an ALTER SESSION statement sets, so a later statement
// can take its place: the parameters of SET (`SET a = 1 b = 'x y'`: A, B),
// the feature of ENABLE/DISABLE/FORCE (`ENABLE PARALLEL DML 4`: PARALLEL
// DML). Anything else (SET EVENTS, ADVISE, a statement it can't read) is
// keyed by its whole text: replaced only by the same statement.
func sessionKeys(sql string) []string {
	body := strings.TrimSpace(stripComments(sql))
	body = strings.TrimRight(body, "; \t\r\n")
	whole := []string{"=" + strings.ToUpper(strings.Join(strings.Fields(body), " "))}
	f := strings.Fields(body)
	if len(f) < 3 {
		return whole
	}
	verb := strings.ToUpper(f[2])
	switch verb {
	case "ENABLE", "DISABLE", "FORCE":
		rest := make([]string, 0, len(f)-3)
		for _, x := range f[3:] {
			rest = append(rest, strings.ToUpper(x))
		}
		if len(rest) >= 2 && rest[0] == "PARALLEL" {
			return []string{"PARALLEL " + rest[1]}
		}
		if len(rest) == 0 {
			return whole
		}
		return []string{strings.Join(rest, " ")}
	case "SET":
	default:
		return whole
	}
	// SET name = value [name = value …]; values may be quoted.
	i := strings.Index(strings.ToUpper(body), "SET") + 3
	var keys []string
	for {
		for i < len(body) && strings.ContainsRune(" \t\r\n", rune(body[i])) {
			i++
		}
		if i >= len(body) {
			break
		}
		j := i
		for j < len(body) && (body[j] == '_' || body[j] == '$' || body[j] == '#' || body[j] == '.' ||
			(body[j] >= 'a' && body[j] <= 'z') || (body[j] >= 'A' && body[j] <= 'Z') || (body[j] >= '0' && body[j] <= '9')) {
			j++
		}
		if j == i {
			return whole
		}
		name := strings.ToUpper(body[i:j])
		for j < len(body) && strings.ContainsRune(" \t\r\n", rune(body[j])) {
			j++
		}
		if j >= len(body) || body[j] != '=' {
			return whole // SET EVENTS '…', SET CONTAINER = … is fine above
		}
		j++
		for j < len(body) && strings.ContainsRune(" \t\r\n", rune(body[j])) {
			j++
		}
		if j < len(body) && (body[j] == '\'' || body[j] == '"') {
			qc := body[j]
			j++
			for j < len(body) {
				if body[j] == qc {
					if j+1 < len(body) && body[j+1] == qc {
						j += 2
						continue
					}
					break
				}
				j++
			}
			if j >= len(body) {
				return whole
			}
			j++
		} else {
			for j < len(body) && !strings.ContainsRune(" \t\r\n", rune(body[j])) {
				j++
			}
		}
		keys = append(keys, name)
		i = j
	}
	if len(keys) == 0 {
		return whole
	}
	return keys
}

// remember keeps an ALTER SESSION statement that ran, to replay after a
// reconnect. Statements are replayed in the order they last ran, and one
// whose settings later statements all set again is dropped:
// `set current_schema = A`, `= B`, `= A` replays as just A. So the list
// holds at most one statement per setting, and needs no cap.
func (s *session) remember(sql string) {
	all := append(s.setup, setupStmt{sql: sql, keys: sessionKeys(sql)})
	// From the newest back: a statement stays when it sets something no
	// later statement sets again.
	set := map[string]bool{}
	keep := make([]bool, len(all))
	for i := len(all) - 1; i >= 0; i-- {
		for _, k := range all[i].keys {
			if !set[k] {
				keep[i] = true
				set[k] = true
			}
		}
	}
	s.setup = s.setup[:0:0]
	for i, x := range all {
		if keep[i] {
			s.setup = append(s.setup, x)
		}
	}
}

// notSent: the statement never reached the server (database/sql closes a
// *sql.Conn for good after the driver reports a broken connection, and
// every later call fails with ErrConnDone), so running it again is safe.
func notSent(err error) bool {
	return errors.Is(err, sql.ErrConnDone) || errors.Is(err, driver.ErrBadConn) || errors.Is(err, sql.ErrTxDone)
}

// lostConn: the connection is unusable (idle timeout, killed session,
// network drop, a cancel whose break failed).
func lostConn(err error) bool {
	if notSent(err) || errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) || errors.Is(err, net.ErrClosed) ||
		errors.Is(err, syscall.EPIPE) || errors.Is(err, syscall.ECONNRESET) || errors.Is(err, syscall.ECONNABORTED) {
		return true
	}
	var oe *network.OracleError
	if errors.As(err, &oe) {
		switch oe.ErrCode {
		// end-of-file on channel, not connected, connection lost, not logged on, session killed
		case 3113, 3114, 3135, 1012, 28:
			return true
		}
	}
	var ne net.Error
	return errors.As(err, &ne)
}

// reconnect opens a new connection with the same settings. ALTER SESSION
// statements run before are replayed; other session state (package
// variables, temporary tables' rows) starts fresh.
func (s *session) reconnect(ctx context.Context) error {
	if s.req == nil {
		return errors.New("not connected")
	}
	s.close()
	s.db, s.conn, s.tx, s.plans = nil, nil, nil, nil
	if err := s.connect(ctx, s.req); err != nil {
		return err
	}
	for _, q := range s.setup {
		_, _ = s.conn.ExecContext(ctx, q.sql)
	}
	return nil
}

// withReconnect runs f; when the connection turns out to be lost, it
// reconnects, and runs f again when that is safe: the statement never
// reached the server, or it is a catalog/text query (nothing streamed yet).
func (s *session) withReconnect(ctx context.Context, w *writer, r *request, f func() error) error {
	w.mu.Lock()
	before := w.frames
	w.mu.Unlock()
	err := f()
	if err == nil || ctx.Err() != nil || s.req == nil || !lostConn(err) {
		return err
	}
	if rerr := s.reconnect(ctx); rerr != nil {
		return fmt.Errorf("the connection to Oracle was lost (%v) and reconnecting failed: %w", err, rerr)
	}
	w.mu.Lock()
	streamed := w.frames != before
	w.mu.Unlock()
	if !streamed && (notSent(err) || r.Op == "text" || r.Op == "ping") {
		return f()
	}
	return fmt.Errorf("the connection to Oracle was lost while running the statement (%v); DataBrain reconnected, run it again", err)
}

// q is what statements run on: the read-only transaction or the connection.
type q interface {
	QueryContext(ctx context.Context, query string, args ...any) (*sql.Rows, error)
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
}

// target: the connection, or on a read-only connection a new READ ONLY
// transaction for this statement. A new one each time: one transaction
// reads the data as of its start, and the last one may have been ended
// (a COMMIT run in it). It is begun with a context of its own: database/sql
// rolls a transaction back when its context ends, and the request's ends
// with the request (the next statement then found it gone and reconnected).
func (s *session) target(ctx context.Context) (q, error) {
	if !s.ro {
		return s.conn, nil
	}
	if s.tx != nil {
		_ = s.tx.Rollback()
		s.tx = nil
	}
	tx, err := s.conn.BeginTx(context.Background(), nil)
	if err != nil {
		return nil, err
	}
	if _, err := tx.ExecContext(ctx, "set transaction read only"); err != nil {
		_ = tx.Rollback()
		return nil, err
	}
	s.tx = tx
	return s.tx, nil
}

// endReadOnly rolls back and ends a read-only connection's transaction.
func (s *session) endReadOnly() {
	if s.tx != nil {
		_ = s.tx.Rollback()
		s.tx = nil
	}
}

// outputSQL reads the DBMS_OUTPUT lines into a CLOB in one call (go-ora's
// string out-binds hold about 8 KB, which cut longer lines; a line is up to
// 32767 bytes): :1 and :2 the caps (lines, characters), :3 the lines (each
// ended by a line break), :4 1 when none are left, 2 when the caps stopped
// it with lines still waiting. go-ora binds in the order the placeholders
// appear, so they appear in that order.
const outputSQL = "declare mx_lines integer := :1; mx_chars integer := :2; " +
	"l varchar2(32767); s integer := 0; n integer := 0; r clob; " +
	"begin dbms_lob.createtemporary(r, true); " +
	"loop if n >= mx_lines or dbms_lob.getlength(r) >= mx_chars then s := 2; exit; end if; " +
	"dbms_output.get_line(l, s); exit when s <> 0; " +
	// The line and its break apart: a 32767-byte line plus one is past a varchar2.
	"if l is not null then dbms_lob.writeappend(r, length(l), l); end if; " +
	"dbms_lob.writeappend(r, 1, chr(10)); n := n + 1; end loop; " +
	":3 := r; :4 := s; end;"

// Caps on the DBMS_OUTPUT read after one statement (as in src/exec.rs).
const (
	outputMaxLines = 10000
	outputMaxBytes = 1 << 20
)

// readOutput returns the DBMS_OUTPUT lines the last statement wrote, and
// whether more were left unread (past the caps). Errors end the reading:
// the output is a convenience, never the statement's failure.
func readOutput(ctx context.Context, t q) ([]string, bool) {
	lines, bytes := []string{}, 0
	var out go_ora.Clob
	var status int64
	if _, err := t.ExecContext(ctx, outputSQL, outputMaxLines, outputMaxBytes, go_ora.Out{Dest: &out}, go_ora.Out{Dest: &status}); err != nil {
		return lines, false
	}
	if out.Valid && out.String != "" {
		for _, l := range strings.Split(strings.TrimSuffix(out.String, "\n"), "\n") {
			if len(lines) >= outputMaxLines || bytes+len(l) > outputMaxBytes {
				return lines, true
			}
			bytes += len(l)
			lines = append(lines, l)
		}
	}
	return lines, status == 2
}

// prefetchRows is how many rows a query fetches per round trip (see dsn).
const prefetchRows = 1000

// dsn builds the go-ora URL. `connect_string` may be an Easy Connect string
// ([//]host[:port]/service), a full (DESCRIPTION=…) or a tnsnames.ora alias.
func dsn(r *request) (string, error) {
	opts := map[string]string{}
	for k, v := range r.Options {
		opts[strings.ToUpper(strings.TrimSpace(k))] = strings.TrimSpace(v)
	}
	// Rows per round trip. go-ora's default (25) makes it guess from each
	// column's declared width (128 KB / the widest a row could be): a few
	// VARCHAR2(4000) columns get 2-5 rows a round trip however short the
	// values are, about 1,000 rows/s over a network. A fixed size fetches
	// that many rows' actual bytes; a connection's own PREFETCH_ROWS wins.
	if _, set := opts["PREFETCH_ROWS"]; !set {
		opts["PREFETCH_ROWS"] = strconv.Itoa(prefetchRows)
	}
	cs := strings.TrimSpace(r.ConnectString)
	if cs == "" {
		port := r.Port
		if port == 0 {
			port = 1521
		}
		svc := r.Service
		if svc == "" {
			svc = "FREEPDB1"
		}
		return go_ora.BuildUrl(r.Host, port, svc, r.User, r.Password, opts), nil
	}
	if strings.HasPrefix(cs, "(") {
		return go_ora.BuildJDBC(r.User, r.Password, cs, opts), nil
	}
	if strings.ContainsAny(cs, "/:") {
		ez := strings.TrimPrefix(cs, "//")
		hostPort, svc, _ := strings.Cut(ez, "/")
		host, portS, hasPort := strings.Cut(hostPort, ":")
		port := 1521
		if hasPort {
			p, err := strconv.Atoi(portS)
			if err != nil {
				return "", fmt.Errorf("invalid port in %q", cs)
			}
			port = p
		}
		return go_ora.BuildUrl(host, port, svc, r.User, r.Password, opts), nil
	}
	desc, err := tnsLookup(cs)
	if err != nil {
		return "", err
	}
	return go_ora.BuildJDBC(r.User, r.Password, desc, opts), nil
}

// tnsLookup finds `alias = (DESCRIPTION=…)` in tnsnames.ora ($TNS_ADMIN,
// $ORACLE_HOME/network/admin, ~/.oracle).
func tnsLookup(alias string) (string, error) {
	return tnsLookupIn(tnsDirs(), alias)
}

func tnsDirs() []string {
	var dirs []string
	if d := os.Getenv("TNS_ADMIN"); d != "" {
		dirs = append(dirs, d)
	}
	if d := os.Getenv("ORACLE_HOME"); d != "" {
		dirs = append(dirs, filepath.Join(d, "network", "admin"))
	}
	if h, err := os.UserHomeDir(); err == nil {
		dirs = append(dirs, filepath.Join(h, ".oracle"))
	}
	return dirs
}

// maxIfileDepth bounds IFILE nesting (and a file that includes itself).
const maxIfileDepth = 8

func tnsLookupIn(dirs []string, alias string) (string, error) {
	for _, d := range dirs {
		if desc, ok := findAliasInFile(filepath.Join(d, "tnsnames.ora"), alias, 0); ok {
			return desc, nil
		}
	}
	return "", fmt.Errorf("TNS alias %q not found (looked for tnsnames.ora in TNS_ADMIN, ORACLE_HOME/network/admin, ~/.oracle)", alias)
}

// findAliasInFile reads a tnsnames.ora and the files its IFILE entries
// name (relative to it), in order.
func findAliasInFile(path, alias string, depth int) (string, bool) {
	if depth > maxIfileDepth {
		return "", false
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return "", false
	}
	return findAliasWith(string(b), alias, func(inc string) (string, bool) {
		if !filepath.IsAbs(inc) {
			inc = filepath.Join(filepath.Dir(path), inc)
		}
		return findAliasInFile(inc, alias, depth+1)
	})
}

func findAlias(text, alias string) (string, bool) {
	return findAliasWith(text, alias, nil)
}

// findAliasWith finds `alias = (DESCRIPTION=…)`; an `IFILE = path` entry
// is looked up with include (nil: skipped) where it stands.
func findAliasWith(text, alias string, include func(string) (string, bool)) (string, bool) {
	// Drop comments.
	var b strings.Builder
	for _, line := range strings.Split(text, "\n") {
		if i := strings.Index(line, "#"); i >= 0 {
			line = line[:i]
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	s := b.String()
	i := 0
	for i < len(s) {
		// An entry: names[, names] = (…)
		eq := strings.Index(s[i:], "=")
		if eq < 0 {
			return "", false
		}
		names := strings.TrimSpace(s[i : i+eq])
		j := i + eq + 1
		for j < len(s) && (s[j] == ' ' || s[j] == '\t' || s[j] == '\n' || s[j] == '\r') {
			j++
		}
		if j >= len(s) || s[j] != '(' {
			// A value without parentheses (IFILE = /x/other.ora) ends
			// with its line; the next entry starts after it.
			end := strings.IndexByte(s[j:], '\n')
			if end < 0 {
				end = len(s) - j
			}
			if strings.EqualFold(lastLine(names), "IFILE") && include != nil {
				if desc, ok := include(strings.Trim(strings.TrimSpace(s[j:j+end]), `"'`)); ok {
					return desc, true
				}
			}
			i = j + end
			continue
		}
		depth, k := 0, j
		for ; k < len(s); k++ {
			if s[k] == '(' {
				depth++
			} else if s[k] == ')' {
				depth--
				if depth == 0 {
					break
				}
			}
		}
		for _, n := range strings.Split(lastLine(names), ",") {
			n = strings.TrimSpace(n)
			if strings.EqualFold(n, alias) || strings.EqualFold(strings.SplitN(n, ".", 2)[0], alias) {
				return s[j : k+1], true
			}
		}
		i = k + 1
	}
	return "", false
}

// lastLine: an entry's names are on the line of its `=` (text before it
// is the end of the last entry, e.g. a stray line).
func lastLine(names string) string {
	names = strings.TrimSpace(names)
	if i := strings.LastIndexByte(names, '\n'); i >= 0 {
		names = names[i+1:]
	}
	return strings.TrimSpace(names)
}

func (s *session) connect(ctx context.Context, r *request) error {
	url, err := dsn(r)
	if err != nil {
		return err
	}
	db, err := sql.Open("oracle", url)
	if err != nil {
		return err
	}
	db.SetMaxOpenConns(1)
	db.SetConnMaxIdleTime(0)
	conn, err := db.Conn(ctx)
	if err != nil {
		_ = db.Close()
		return err
	}
	if err := conn.PingContext(ctx); err != nil {
		_ = conn.Close()
		_ = db.Close()
		return err
	}
	_, _ = conn.ExecContext(ctx, "alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'")
	// put_line output is kept for the app to read after each block or CALL.
	_, _ = conn.ExecContext(ctx, "begin dbms_output.enable(null); end;")
	s.db, s.conn, s.ro, s.req = db, conn, r.ReadOnly, r
	return nil
}

func (s *session) close() {
	if s.tx != nil {
		_ = s.tx.Rollback()
	}
	if s.conn != nil {
		_ = s.conn.Close()
	}
	if s.db != nil {
		_ = s.db.Close()
	}
}

func args(binds map[string]*string) []any {
	var out []any
	for k, v := range binds {
		if v == nil {
			out = append(out, sql.Named(k, sql.NullString{}))
		} else {
			out = append(out, sql.Named(k, *v))
		}
	}
	return out
}

// ---------------------------------------------------------------- types

// colType maps an Oracle column to a DataBrain column type (same rules as the
// Instant Client driver: exact NUMBERs that do not fit in i64 stay text).
func colType(ct *sql.ColumnType) column {
	name := strings.ToUpper(ct.DatabaseTypeName())
	c := column{Name: ct.Name(), Type: "utf8", DbType: name}
	switch {
	case name == "NUMBER" || name == "DECIMAL" || name == "INTEGER":
		p, s, ok := ct.DecimalSize()
		if ok && p > 0 && s >= 0 && s != 255 {
			c.DbType = fmt.Sprintf("NUMBER(%d,%d)", p, s)
		} else {
			c.DbType = "NUMBER"
		}
		if ok && s == 0 && p > 0 && p <= 18 {
			c.Type = "int64"
		}
	case name == "IBFLOAT" || name == "IBDOUBLE" || name == "BINARY_FLOAT" || name == "BINARY_DOUBLE":
		c.Type = "float64"
		if strings.Contains(name, "FLOAT") {
			c.DbType = "BINARY_FLOAT"
		} else {
			c.DbType = "BINARY_DOUBLE"
		}
	case name == "DATE" || name == "TIMESTAMP" || name == "TIMESTAMPDTY":
		c.Type = "timestamp"
	case strings.Contains(name, "TIME ZONE") || name == "TIMESTAMPTZ_DTY" || name == "TIMESTAMPLTZ_DTY" || name == "TIMESTAMPTZ" || name == "TIMESTAMPLTZ":
		c.Type = "timestamptz"
	case name == "RAW" || name == "BLOB" || name == "LONG RAW" || name == "LONGRAW" || name == "OCIBLOBLOCATOR":
		c.Type = "binary"
		// go-ora fetches LOBs inline by redefining BLOB as LONG RAW and CLOB
		// as LONG VARCHAR (the default "LOB FETCH=INLINE"); with STREAM they
		// stay locators. Name them as the user declared them. (A real LONG
		// RAW column is reported the same way; BLOB is far more common.)
		switch name {
		case "OCIBLOBLOCATOR", "LONGRAW":
			c.DbType = "BLOB"
		}
	case name == "LONGVARCHAR" || name == "OCICLOBLOCATOR":
		c.DbType = "CLOB" // also NCLOB: go-ora does not tell them apart
	case name == "OCIFILELOCATOR":
		c.DbType = "BFILE" // shown as BFILENAME('DIR', 'file')
	case name == "BOOLEAN":
		c.Type = "bool"
	}
	return c
}

func dest(c column) any {
	switch c.Type {
	case "int64":
		return new(sql.NullInt64)
	case "float64":
		return new(sql.NullFloat64)
	case "timestamp", "timestamptz":
		return new(sql.NullTime)
	case "binary":
		return new([]byte)
	case "bool":
		return new(sql.NullBool)
	}
	if strings.HasPrefix(c.DbType, "NUMBER") {
		return new(sql.NullString) // exact decimal text
	}
	return new(any)
}

type enc struct{ b []byte }

func (e *enc) u8(v byte)   { e.b = append(e.b, v) }
func (e *enc) i64(v int64) { e.b = binary.LittleEndian.AppendUint64(e.b, uint64(v)) }
func (e *enc) str(v string) {
	e.b = binary.LittleEndian.AppendUint32(e.b, uint32(len(v)))
	e.b = append(e.b, v...)
}
func (e *enc) bytes(v []byte) {
	e.b = binary.LittleEndian.AppendUint32(e.b, uint32(len(v)))
	e.b = append(e.b, v...)
}

func wallMicros(t time.Time) int64 {
	return time.Date(t.Year(), t.Month(), t.Day(), t.Hour(), t.Minute(), t.Second(), t.Nanosecond(), time.UTC).UnixMicro()
}

func (e *enc) value(c column, d any) {
	switch v := d.(type) {
	case *sql.NullInt64:
		if !v.Valid {
			e.u8(0)
			return
		}
		e.u8(1)
		e.i64(v.Int64)
	case *sql.NullFloat64:
		if !v.Valid {
			e.u8(0)
			return
		}
		e.u8(2)
		e.i64(int64(math.Float64bits(v.Float64)))
	case *sql.NullString:
		if !v.Valid {
			e.u8(0)
			return
		}
		e.u8(3)
		e.str(v.String)
	case *sql.NullTime:
		if !v.Valid {
			e.u8(0)
			return
		}
		if c.Type == "timestamptz" {
			e.u8(6)
			e.i64(v.Time.UnixMicro())
		} else {
			e.u8(5)
			e.i64(wallMicros(v.Time))
		}
	case *[]byte:
		if *v == nil {
			e.u8(0)
			return
		}
		e.u8(4)
		e.bytes(*v)
	case *sql.NullBool:
		if !v.Valid {
			e.u8(0)
			return
		}
		e.u8(7)
		if v.Bool {
			e.u8(1)
		} else {
			e.u8(0)
		}
	case *any:
		e.anyValue(*v)
	default:
		e.u8(0)
	}
}

func (e *enc) anyValue(v any) {
	switch x := v.(type) {
	case nil:
		e.u8(0)
	case string:
		e.u8(3)
		e.str(x)
	case []byte:
		e.u8(3)
		e.str(string(x))
	case time.Time:
		e.u8(3)
		e.str(x.Format("2006-01-02 15:04:05.999999999"))
	case go_ora.BFile:
		// The file reference, not its content (which may be large and needs
		// extra round trips while the rows are still being fetched).
		if !x.Valid {
			e.u8(0)
			return
		}
		e.u8(3)
		e.str(fmt.Sprintf("BFILENAME('%s', '%s')", strings.ReplaceAll(x.GetDirName(), "'", "''"), strings.ReplaceAll(x.GetFileName(), "'", "''")))
	case fmt.Stringer:
		e.u8(3)
		e.str(x.String())
	default:
		e.u8(3)
		e.str(fmt.Sprint(x))
	}
}

// text renders a scanned value as a string (catalog queries).
func text(c column, d any) *string {
	var e enc
	e.value(c, d)
	if len(e.b) == 0 || e.b[0] == 0 {
		return nil
	}
	var s string
	switch e.b[0] {
	case 1:
		s = strconv.FormatInt(int64(binary.LittleEndian.Uint64(e.b[1:])), 10)
	case 2:
		s = strconv.FormatFloat(math.Float64frombits(binary.LittleEndian.Uint64(e.b[1:])), 'g', -1, 64)
	case 3, 4:
		s = string(e.b[5:])
	case 5, 6:
		s = time.UnixMicro(int64(binary.LittleEndian.Uint64(e.b[1:]))).UTC().Format("2006-01-02 15:04:05")
	case 7:
		s = strconv.FormatBool(e.b[1] == 1)
	}
	return &s
}

// ---------------------------------------------------------------- requests

func (s *session) query(ctx context.Context, w *writer, r *request) error {
	t, err := s.target(ctx)
	if err != nil {
		return err
	}
	rows, err := t.QueryContext(ctx, r.SQL, args(r.Binds)...)
	if err != nil {
		return err
	}
	defer rows.Close()
	cts, err := rows.ColumnTypes()
	if err != nil {
		return err
	}
	cols := make([]column, len(cts))
	dests := make([]any, len(cts))
	for i, ct := range cts {
		cols[i] = colType(ct)
		dests[i] = dest(cols[i])
		if l, ok := r.labels[i]; ok {
			cols[i].DbType = l
		}
	}
	// The schema goes out with the first batch (or at the end): until then
	// nothing is streamed, so a first page that can't be read can be retried.
	schemaSent := false
	sendSchema := func() error {
		if schemaSent {
			return nil
		}
		schemaSent = true
		return w.json(map[string]any{"id": r.ID, "type": "schema", "columns": cols})
	}
	batch := r.BatchSize
	if batch <= 0 || batch > 10000 {
		batch = 1000
	}
	var e enc
	n, total := 0, 0
	flush := func() error {
		if n == 0 {
			return nil
		}
		if err := sendSchema(); err != nil {
			return err
		}
		head := make([]byte, 16)
		binary.BigEndian.PutUint64(head[:8], r.ID)
		binary.BigEndian.PutUint32(head[8:12], uint32(len(cols)))
		binary.BigEndian.PutUint32(head[12:16], uint32(n))
		err := w.frame('B', append(head, e.b...))
		e.b, n = e.b[:0], 0
		return err
	}
	for rows.Next() {
		if err := rows.Scan(dests...); err != nil {
			return err
		}
		start := len(e.b)
		for i := range cols {
			e.value(cols[i], dests[i])
		}
		// One frame holds a row: a larger one (LOBs read inline) is
		// refused before it is sent, and the stream stays whole.
		if len(e.b)-start > maxFrame-16 {
			return fmt.Errorf("row %d is larger than %d MB (its LOB values are read whole): select the LOBs shorter (dbms_lob.substr) or fewer of them", total+1, maxFrame>>20)
		}
		n++
		total++
		if n >= batch || len(e.b) > 8<<20 {
			if err := flush(); err != nil {
				return err
			}
		}
		if r.MaxRows > 0 && total >= r.MaxRows {
			break
		}
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if err := flush(); err != nil {
		return err
	}
	if err := sendSchema(); err != nil {
		return err
	}
	return w.json(map[string]any{"id": r.ID, "type": "done"})
}

func (s *session) textQuery(ctx context.Context, w *writer, r *request) error {
	t, err := s.target(ctx)
	if err != nil {
		return err
	}
	rows, err := t.QueryContext(ctx, r.SQL, args(r.Binds)...)
	if err != nil {
		return err
	}
	defer rows.Close()
	cts, err := rows.ColumnTypes()
	if err != nil {
		return err
	}
	cols := make([]column, len(cts))
	dests := make([]any, len(cts))
	for i, ct := range cts {
		cols[i] = colType(ct)
		dests[i] = dest(cols[i])
	}
	out := [][]*string{}
	for rows.Next() {
		if err := rows.Scan(dests...); err != nil {
			return err
		}
		row := make([]*string, len(cols))
		for i := range cols {
			row[i] = text(cols[i], dests[i])
		}
		out = append(out, row)
	}
	if err := rows.Err(); err != nil {
		return err
	}
	return w.json(map[string]any{"id": r.ID, "type": "rows", "rows": out})
}

func (s *session) exec(ctx context.Context, w *writer, r *request) error {
	t, err := s.target(ctx)
	if err != nil {
		return err
	}
	res, err := t.ExecContext(ctx, r.SQL, args(r.Binds)...)
	if err != nil {
		// What the block printed before it failed goes with the error.
		if r.Output && !lostConn(err) {
			if lines, more := readOutput(context.Background(), t); len(lines) > 0 {
				return &outputError{err: err, lines: lines, more: more}
			}
		}
		return err
	}
	if isAlterSession(r.SQL) {
		s.remember(r.SQL)
	}
	reply := map[string]any{"id": r.ID, "type": "done"}
	if r.Output {
		if lines, more := readOutput(ctx, t); len(lines) > 0 {
			reply["output"] = lines
			reply["output_more"] = more
		}
	}
	if n, err := res.RowsAffected(); err == nil {
		reply["rows_affected"] = n
	}
	return w.json(reply)
}

// execMany runs r.SQL once with every row's values bound as arrays (array
// DML: one round trip for all of them), as text the session reads (DataBrain
// writes numbers with its decimal separator). A failed call is rolled back,
// so it keeps none of its rows: the rows before the failing one would
// otherwise commit with the next statement.
func (s *session) execMany(ctx context.Context, w *writer, r *request) error {
	if s.ro {
		return errors.New("This connection is read-only")
	}
	args := make([]any, len(r.Columns))
	for i, col := range r.Columns {
		values := make([]sql.NullString, len(col))
		for j, v := range col {
			if v != nil {
				values[j] = sql.NullString{String: *v, Valid: true}
			}
		}
		args[i] = values
	}
	res, err := s.conn.ExecContext(ctx, r.SQL, args...)
	if err != nil {
		if !lostConn(err) {
			_, _ = s.conn.ExecContext(context.Background(), "ROLLBACK")
		}
		return err
	}
	reply := map[string]any{"id": r.ID, "type": "done"}
	if n, err := res.RowsAffected(); err == nil {
		reply["rows_affected"] = n
	}
	return w.json(reply)
}

func (s *session) handle(ctx context.Context, w *writer, r *request) error {
	switch r.Op {
	case "hello":
		return w.json(map[string]any{"id": r.ID, "type": "ok", "version": Version})
	case "connect":
		if err := s.connect(ctx, r); err != nil {
			return err
		}
		return w.json(map[string]any{"id": r.ID, "type": "ok"})
	case "ping":
		if s.conn == nil {
			return errors.New("not connected")
		}
		if err := s.withReconnect(ctx, w, r, func() error { return s.conn.PingContext(ctx) }); err != nil {
			return err
		}
		return w.json(map[string]any{"id": r.ID, "type": "ok"})
	case "query":
		w.mu.Lock()
		before := w.frames
		w.mu.Unlock()
		r, err := s.readableFirst(ctx, r)
		if err != nil {
			return err
		}
		err = s.withReconnect(ctx, w, r, func() error { return s.query(ctx, w, r) })
		if !undecodable(err) || ctx.Err() != nil {
			return err
		}
		w.mu.Lock()
		streamed := w.frames != before
		w.mu.Unlock()
		return s.retryReadable(ctx, w, r, err, streamed)
	case "text":
		return s.withReconnect(ctx, w, r, func() error { return s.textQuery(ctx, w, r) })
	case "exec":
		// DDL may change a table's columns: queries are described again.
		s.plans = nil
		return s.withReconnect(ctx, w, r, func() error { return s.exec(ctx, w, r) })
	case "execmany":
		return s.execMany(ctx, w, r)
	}
	return fmt.Errorf("unknown op %q", r.Op)
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "--version" {
		fmt.Println(Version)
		return
	}
	s := &session{}
	defer s.close()
	serve(os.Stdin, os.Stdout, func(ctx context.Context, w *writer, r *request) error {
		err := s.handle(ctx, w, r)
		// A read-only connection's transaction ends with its statement: a
		// COMMIT inside a block ends READ ONLY, and what ran after it in
		// the block, not committed, is rolled back now, not left holding
		// locks until the next statement.
		s.endReadOnly()
		return err
	}, func() {
		// The statement didn't stop: leave anyway (the connection closing
		// with the process ends the session on the server).
		os.Exit(1)
	})
}

// gracePeriod is how long a statement may take to stop after DataBrain went
// away before the helper exits anyway (a variable for tests).
var gracePeriod = 10 * time.Second

// cancelGrace is how long a cancelled statement may take to stop before the
// helper exits anyway: go-ora sends a break, but a read that lost its place
// in the server's reply (an object column misread as a length) waits for
// bytes that never come, and nothing else would end it. The connection
// closes with the process, which ends the session on the server; DataBrain
// starts a new helper for the next statement. A variable for tests.
var cancelGrace = 5 * time.Second

// serve reads requests from `stdin` and runs them one at a time with
// `handle`, answering on `stdout`. When stdin closes (DataBrain quit or
// died), the running statement is cancelled, queued requests are dropped,
// and serve returns; if the statement doesn't stop within gracePeriod,
// `stuck` is called (the process exits).
func serve(stdin io.Reader, stdout io.Writer, handle func(context.Context, *writer, *request) error, stuck func()) {
	in := bufio.NewReaderSize(stdin, 1<<16)
	w := &writer{w: bufio.NewWriterSize(stdout, 1<<20)}
	work := make(chan *request, 16)

	var mu sync.Mutex
	var running uint64
	var cancel context.CancelFunc
	var runDone chan struct{} // closed when the running request returns
	var gone bool
	finished := make(chan struct{})
	defer close(finished)

	go func() {
		defer close(work)
		for {
			kind, b, err := readFrame(in)
			if err != nil {
				// stdin closed: DataBrain went away. What runs is stopped
				// (it would keep running on the server for nobody).
				mu.Lock()
				gone = true
				if cancel != nil {
					cancel()
					go func() {
						select {
						case <-finished:
						case <-time.After(gracePeriod):
							stuck()
						}
					}()
				}
				mu.Unlock()
				return
			}
			if kind != 'J' {
				continue
			}
			if len(b) > maxRequest {
				// Only its id is read, to answer it.
				var head struct {
					ID uint64 `json:"id"`
				}
				_ = json.Unmarshal(b, &head)
				_ = w.json(map[string]any{"id": head.ID, "type": "error", "message": fmt.Sprintf("a request of %d MB is larger than the %d MB the Oracle driver takes", len(b)>>20, maxRequest>>20)})
				continue
			}
			r := new(request)
			if err := json.Unmarshal(b, r); err != nil {
				_ = w.json(map[string]any{"id": 0, "type": "error", "message": "bad request: " + err.Error()})
				continue
			}
			if r.Op == "cancel" {
				mu.Lock()
				if cancel != nil && (r.Target == 0 || r.Target == running) {
					cancel()
					// A cancel that isn't answered: leave (see cancelGrace).
					done := runDone
					go func() {
						select {
						case <-done:
						case <-finished:
						case <-time.After(cancelGrace):
							stuck()
						}
					}()
				}
				mu.Unlock()
				continue
			}
			work <- r
		}
	}()

	for r := range work {
		if r.Op == "close" {
			return
		}
		ctx, c := context.WithCancel(context.Background())
		mu.Lock()
		if gone {
			// Queued before DataBrain went away: nobody waits for it.
			mu.Unlock()
			c()
			continue
		}
		done := make(chan struct{})
		running, cancel, runDone = r.ID, c, done
		mu.Unlock()
		err := handle(ctx, w, r)
		close(done)
		mu.Lock()
		running, cancel, runDone = 0, nil, nil
		mu.Unlock()
		cancelled := ctx.Err() != nil
		c()
		if err != nil {
			reply := errReply(r.ID, err)
			if cancelled {
				reply["code"] = "cancelled"
			}
			if w.json(reply) != nil {
				return
			}
		}
	}
}
