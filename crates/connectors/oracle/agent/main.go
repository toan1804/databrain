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
// Requests run one at a time, except {"op":"cancel","target":id}, which
// cancels the running request (go-ora sends a break to the server).
// The password arrives over stdin, never on the command line.
package main

import (
	"bufio"
	"context"
	"database/sql"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
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
}

func (w *writer) frame(kind byte, payload []byte) error {
	w.mu.Lock()
	defer w.mu.Unlock()
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

func readFrame(r *bufio.Reader) (byte, []byte, error) {
	var h [5]byte
	if _, err := io.ReadFull(r, h[:]); err != nil {
		return 0, nil, err
	}
	n := binary.BigEndian.Uint32(h[:4])
	if n > 64<<20 {
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
	return m
}

// ---------------------------------------------------------------- connection

type session struct {
	db   *sql.DB
	conn *sql.Conn
	tx   *sql.Tx // read-only connections run everything in a READ ONLY transaction
	ro   bool
}

// q is what statements run on: the read-only transaction or the connection.
type q interface {
	QueryContext(ctx context.Context, query string, args ...any) (*sql.Rows, error)
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
}

func (s *session) target(ctx context.Context) (q, error) {
	if !s.ro {
		return s.conn, nil
	}
	if s.tx == nil {
		tx, err := s.conn.BeginTx(ctx, nil)
		if err != nil {
			return nil, err
		}
		if _, err := tx.ExecContext(ctx, "set transaction read only"); err != nil {
			_ = tx.Rollback()
			return nil, err
		}
		s.tx = tx
	}
	return s.tx, nil
}

// dsn builds the go-ora URL. `connect_string` may be an Easy Connect string
// ([//]host[:port]/service), a full (DESCRIPTION=…) or a tnsnames.ora alias.
func dsn(r *request) (string, error) {
	opts := map[string]string{}
	for k, v := range r.Options {
		opts[strings.ToUpper(strings.TrimSpace(k))] = strings.TrimSpace(v)
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
	for _, d := range dirs {
		b, err := os.ReadFile(filepath.Join(d, "tnsnames.ora"))
		if err != nil {
			continue
		}
		if desc, ok := findAlias(string(b), alias); ok {
			return desc, nil
		}
	}
	return "", fmt.Errorf("TNS alias %q not found (looked for tnsnames.ora in TNS_ADMIN, ORACLE_HOME/network/admin, ~/.oracle)", alias)
}

func findAlias(text, alias string) (string, bool) {
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
			i = j
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
		for _, n := range strings.Split(names, ",") {
			n = strings.TrimSpace(n)
			if strings.EqualFold(n, alias) || strings.EqualFold(strings.SplitN(n, ".", 2)[0], alias) {
				return s[j : k+1], true
			}
		}
		i = k + 1
	}
	return "", false
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
	s.db, s.conn, s.ro = db, conn, r.ReadOnly
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
	case name == "RAW" || name == "BLOB" || name == "LONG RAW" || name == "LONGRAW":
		c.Type = "binary"
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
	}
	if err := w.json(map[string]any{"id": r.ID, "type": "schema", "columns": cols}); err != nil {
		return err
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
		for i := range cols {
			e.value(cols[i], dests[i])
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
		if err := s.conn.PingContext(ctx); err != nil {
			return err
		}
		return w.json(map[string]any{"id": r.ID, "type": "ok"})
	case "query":
		return s.query(ctx, w, r)
	case "text":
		return s.textQuery(ctx, w, r)
	case "exec":
		return s.exec(ctx, w, r)
	}
	return fmt.Errorf("unknown op %q", r.Op)
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "--version" {
		fmt.Println(Version)
		return
	}
	in := bufio.NewReaderSize(os.Stdin, 1<<16)
	w := &writer{w: bufio.NewWriterSize(os.Stdout, 1<<20)}
	work := make(chan *request, 16)

	var mu sync.Mutex
	var running uint64
	var cancel context.CancelFunc

	go func() {
		defer close(work)
		for {
			kind, b, err := readFrame(in)
			if err != nil {
				return // stdin closed: DataBrain went away
			}
			if kind != 'J' {
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
				}
				mu.Unlock()
				continue
			}
			work <- r
		}
	}()

	s := &session{}
	defer s.close()
	for r := range work {
		if r.Op == "close" {
			return
		}
		ctx, c := context.WithCancel(context.Background())
		mu.Lock()
		running, cancel = r.ID, c
		mu.Unlock()
		err := s.handle(ctx, w, r)
		mu.Lock()
		running, cancel = 0, nil
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
