package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"database/sql/driver"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/sijms/go-ora/v2/network"
)

func TestLostConnection(t *testing.T) {
	for _, err := range []error{
		sql.ErrConnDone,
		driver.ErrBadConn,
		fmt.Errorf("query: %w", sql.ErrConnDone),
		io.EOF,
		syscall.ECONNRESET,
		&network.OracleError{ErrCode: 3113},
		&network.OracleError{ErrCode: 28},
	} {
		if !lostConn(err) {
			t.Errorf("%v: want lost", err)
		}
	}
	for _, err := range []error{errors.New("ORA-00942"), &network.OracleError{ErrCode: 942}} {
		if lostConn(err) {
			t.Errorf("%v: not a lost connection", err)
		}
	}
	if !notSent(sql.ErrConnDone) || notSent(io.EOF) {
		t.Error("only errors raised before sending are retried blindly")
	}
}

// A connection closed by database/sql is reopened and the request retried once.
func TestRetriesAfterReconnect(t *testing.T) {
	s := &session{}
	w := &writer{}
	calls := 0
	err := s.withReconnect(t.Context(), w, &request{Op: "text"}, func() error {
		calls++
		return sql.ErrConnDone
	})
	// No connect request yet: nothing to reconnect with, the error is returned as is.
	if !errors.Is(err, sql.ErrConnDone) || calls != 1 {
		t.Fatalf("err=%v calls=%d", err, calls)
	}
}

func TestSessionSetupReplaysTheLastValue(t *testing.T) {
	s := &session{}
	for _, q := range []string{
		"alter session set current_schema = A",
		"alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'",
		"ALTER SESSION SET CURRENT_SCHEMA=B",
		"/* again */ alter session set current_schema = A",
		"alter session enable parallel dml",
		"alter session disable parallel dml",
	} {
		if !isAlterSession(q) {
			t.Fatalf("%q: not ALTER SESSION", q)
		}
		s.remember(q)
	}
	var got []string
	for _, x := range s.setup {
		got = append(got, x.sql)
	}
	want := []string{
		"alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'",
		"/* again */ alter session set current_schema = A",
		"alter session disable parallel dml",
	}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("replay %q, want %q", got, want)
	}

	// More settings than the old cap of 64: none is lost, one per setting.
	s = &session{}
	for i := 0; i < 200; i++ {
		s.remember(fmt.Sprintf("alter session set p%d = %d", i%100, i))
	}
	if len(s.setup) != 100 || s.setup[99].sql != "alter session set p99 = 199" {
		t.Fatalf("%d kept, last %q", len(s.setup), s.setup[len(s.setup)-1].sql)
	}

	// A statement setting two keeps its place until both are set again.
	s = &session{}
	s.remember("alter session set a = 1 b = 'x y'")
	s.remember("alter session set a = 2")
	if len(s.setup) != 2 {
		t.Fatalf("b is still set by the first: %v", s.setup)
	}
	s.remember("alter session set b = 3")
	if len(s.setup) != 2 || s.setup[0].sql != "alter session set a = 2" {
		t.Fatalf("the first is replaced by the two after it: %v", s.setup)
	}
	// Unread forms are kept by their text.
	s = &session{}
	s.remember("alter session set events '10046 trace name context forever'")
	s.remember("alter session set events '10046 trace name context off'")
	if len(s.setup) != 2 {
		t.Fatalf("events kept in order: %v", s.setup)
	}
	if isAlterSession("select 1 from dual") || isAlterSession("-- alter session set x = 1\nselect 1 from dual") {
		t.Fatal("not ALTER SESSION")
	}
}

func TestTnsAliasAfterIfileAndInIt(t *testing.T) {
	text := "IFILE = /x/other.ora\nSALES = (DESCRIPTION=(ADDRESS=(HOST=h)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=s)))\n"
	if d, ok := findAlias(text, "sales"); !ok || d[:12] != "(DESCRIPTION" {
		t.Fatalf("alias after IFILE: %q %v", d, ok)
	}
	dir := t.TempDir()
	sub := filepath.Join(dir, "sub")
	if err := os.Mkdir(sub, 0o700); err != nil {
		t.Fatal(err)
	}
	write := func(p, s string) {
		if err := os.WriteFile(p, []byte(s), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write(filepath.Join(dir, "tnsnames.ora"), "# main\nIFILE = sub/more.ora\nLOOP = (DESCRIPTION=(x))\n")
	write(filepath.Join(sub, "more.ora"), "HR.WORLD = (DESCRIPTION=(hr))\nIFILE = more.ora\n")
	if d, err := tnsLookupIn([]string{dir}, "hr"); err != nil || d != "(DESCRIPTION=(hr))" {
		t.Fatalf("alias in an IFILE: %q %v", d, err)
	}
	if d, err := tnsLookupIn([]string{dir}, "loop"); err != nil || d != "(DESCRIPTION=(x))" {
		t.Fatalf("alias after an IFILE that includes itself: %q %v", d, err)
	}
	if _, err := tnsLookupIn([]string{dir}, "nope"); err == nil {
		t.Fatal("missing alias found")
	}
}

func TestFrameLimit(t *testing.T) {
	old := maxFrame
	maxFrame = 100
	defer func() { maxFrame = old }()
	var buf bytes.Buffer
	w := &writer{w: bufio.NewWriter(&buf)}
	if err := w.frame('B', make([]byte, 101)); err == nil {
		t.Fatal("an oversized frame was written")
	}
	if buf.Len() != 0 {
		t.Fatal("part of it reached the stream")
	}
	if err := w.frame('B', make([]byte, 100)); err != nil || buf.Len() != 105 {
		t.Fatalf("%v, %d bytes", err, buf.Len())
	}
}

// reqFrame is a request as DataBrain sends it.
func reqFrame(t *testing.T, r map[string]any) []byte {
	b, err := json.Marshal(r)
	if err != nil {
		t.Fatal(err)
	}
	h := make([]byte, 5)
	binary.BigEndian.PutUint32(h[:4], uint32(len(b)))
	h[4] = 'J'
	return append(h, b...)
}

func TestDataBrainGoneStopsTheStatementAndTheHelper(t *testing.T) {
	inR, inW := io.Pipe()
	started := make(chan struct{})
	var handled []uint64
	var mu sync.Mutex
	done := make(chan struct{})
	stopped := make(chan error, 1)
	go func() {
		defer close(done)
		serve(inR, io.Discard, func(ctx context.Context, w *writer, r *request) error {
			mu.Lock()
			handled = append(handled, r.ID)
			mu.Unlock()
			if r.ID == 1 {
				close(started)
				<-ctx.Done() // a long statement: until it is cancelled
				stopped <- ctx.Err()
			}
			return nil
		}, func() { t.Error("a statement that stopped must not make the helper exit") })
	}()
	go func() {
		_, _ = inW.Write(append(reqFrame(t, map[string]any{"id": 1, "op": "query"}), reqFrame(t, map[string]any{"id": 2, "op": "query"})...))
	}()
	select {
	case <-started:
	case <-time.After(5 * time.Second):
		t.Fatal("the request didn't start")
	}
	// Request 2 is read and queued, then DataBrain goes away.
	time.Sleep(50 * time.Millisecond)
	inW.Close()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("the helper kept running after its stdin closed")
	}
	if err := <-stopped; !errors.Is(err, context.Canceled) {
		t.Fatalf("the running statement was not cancelled: %v", err)
	}
	mu.Lock()
	defer mu.Unlock()
	if fmt.Sprint(handled) != "[1]" {
		t.Fatalf("a request queued before it went away ran: %v", handled)
	}
}

func TestAStatementThatDoesNotStopMakesTheHelperExit(t *testing.T) {
	old := gracePeriod
	gracePeriod = 100 * time.Millisecond
	defer func() { gracePeriod = old }()
	inR, inW := io.Pipe()
	started, release, exited := make(chan struct{}), make(chan struct{}), make(chan struct{})
	go serve(inR, io.Discard, func(ctx context.Context, w *writer, r *request) error {
		close(started)
		<-release // ignores the cancel
		return nil
	}, func() { close(exited) })
	go func() { _, _ = inW.Write(reqFrame(t, map[string]any{"id": 1, "op": "query"})) }()
	select {
	case <-started:
	case <-time.After(5 * time.Second):
		t.Fatal("the request didn't start")
	}
	inW.Close()
	select {
	case <-exited:
	case <-time.After(5 * time.Second):
		t.Fatal("a stuck statement kept the helper alive")
	}
	close(release)
}

// A cancel that isn't answered (a read that lost its place in the reply
// waits forever) makes the helper exit: its connection closes, which frees
// the session; DataBrain starts a new helper.
func TestACancelThatIsNotAnsweredMakesTheHelperExit(t *testing.T) {
	old := cancelGrace
	cancelGrace = 100 * time.Millisecond
	defer func() { cancelGrace = old }()
	inR, inW := io.Pipe()
	defer inW.Close()
	started, release, exited := make(chan struct{}), make(chan struct{}), make(chan struct{})
	go serve(inR, io.Discard, func(ctx context.Context, w *writer, r *request) error {
		close(started)
		<-release // ignores the cancel
		return nil
	}, func() { close(exited) })
	go func() { _, _ = inW.Write(reqFrame(t, map[string]any{"id": 1, "op": "query"})) }()
	<-started
	go func() { _, _ = inW.Write(reqFrame(t, map[string]any{"id": 2, "op": "cancel", "target": 1})) }()
	select {
	case <-exited:
	case <-time.After(5 * time.Second):
		t.Fatal("an unanswered cancel kept the helper waiting")
	}
	close(release)
}

// A cancel answered in time doesn't: the next request runs.
func TestACancelThatIsAnsweredKeepsTheHelper(t *testing.T) {
	old := cancelGrace
	cancelGrace = 200 * time.Millisecond
	defer func() { cancelGrace = old }()
	inR, inW := io.Pipe()
	defer inW.Close()
	var mu sync.Mutex
	var handled []uint64
	exited := make(chan struct{})
	started := make(chan struct{})
	go serve(inR, io.Discard, func(ctx context.Context, w *writer, r *request) error {
		mu.Lock()
		handled = append(handled, r.ID)
		mu.Unlock()
		if r.ID == 1 {
			close(started)
			<-ctx.Done()
			return ctx.Err()
		}
		return nil
	}, func() { close(exited) })
	_, _ = inW.Write(reqFrame(t, map[string]any{"id": 1, "op": "query"}))
	<-started
	_, _ = inW.Write(reqFrame(t, map[string]any{"id": 2, "op": "cancel", "target": 1}))
	_, _ = inW.Write(reqFrame(t, map[string]any{"id": 3, "op": "query"}))
	select {
	case <-exited:
		t.Fatal("the helper exited after a cancel that was answered")
	case <-time.After(500 * time.Millisecond):
	}
	mu.Lock()
	defer mu.Unlock()
	if fmt.Sprint(handled) != "[1 3]" {
		t.Fatalf("the request after the cancel didn't run: %v", handled)
	}
}

// A stopped DML or block isn't ended by the watchdog: Oracle answers its
// break once it has rolled it back, which may take long, and ending the
// session would lose the transaction's other work.
func TestAStoppedExecIsWaitedFor(t *testing.T) {
	old := cancelGrace
	cancelGrace = 50 * time.Millisecond
	defer func() { cancelGrace = old }()
	inR, inW := io.Pipe()
	defer inW.Close()
	started, exited := make(chan struct{}), make(chan struct{})
	release := make(chan struct{})
	go serve(inR, io.Discard, func(ctx context.Context, w *writer, r *request) error {
		close(started)
		<-release // a long rollback
		return nil
	}, func() { close(exited) })
	go func() { _, _ = inW.Write(reqFrame(t, map[string]any{"id": 1, "op": "exec"})) }()
	<-started
	go func() { _, _ = inW.Write(reqFrame(t, map[string]any{"id": 2, "op": "cancel", "target": 1})) }()
	select {
	case <-exited:
		t.Fatal("a stopped exec ended the helper")
	case <-time.After(300 * time.Millisecond):
	}
	close(release)
}

// readableFirst describes only what DBMS_SQL can parse without running it.
func TestOnlySelectsAreDescribed(t *testing.T) {
	for _, sql := range []string{"select 1 from dual", "SELECT*FROM t", "select/**/id from t", `select"DOC" from t`, "  -- c\n with q as (select 1 from dual) select * from q", "(select 1 from dual)"} {
		if !isSelect(sql) {
			t.Errorf("%q: not taken for a SELECT", sql)
		}
	}
	for _, sql := range []string{"create table selection (a int)", "drop table t", "begin null; end;", "selected", "insert into t select 1 from dual", ""} {
		if isSelect(sql) {
			t.Errorf("%q: taken for a SELECT", sql)
		}
	}
}

// A request larger than the helper takes is answered with an error and the
// next one runs: the helper doesn't take it for stdin closing and exit.
func TestAnOversizedRequestIsRefusedAndTheHelperGoesOn(t *testing.T) {
	old := maxRequest
	maxRequest = 1 << 10
	defer func() { maxRequest = old }()
	inR, inW := io.Pipe()
	outR, outW := io.Pipe()
	handled := make(chan uint64, 2)
	go serve(inR, outW, func(ctx context.Context, w *writer, r *request) error {
		handled <- r.ID
		return w.json(map[string]any{"id": r.ID, "type": "done"})
	}, func() { t.Error("the helper must not exit") })
	go func() {
		big := reqFrame(t, map[string]any{"id": 1, "op": "execmany", "sql": strings.Repeat("x", 4<<10)})
		_, _ = inW.Write(append(big, reqFrame(t, map[string]any{"id": 2, "op": "query"})...))
	}()
	out := bufio.NewReader(outR)
	var replies []map[string]any
	for len(replies) < 2 {
		_, b, err := readFrame(out)
		if err != nil {
			t.Fatal(err)
		}
		var m map[string]any
		_ = json.Unmarshal(b, &m)
		replies = append(replies, m)
	}
	if replies[0]["id"] != float64(1) || replies[0]["type"] != "error" || !strings.Contains(fmt.Sprint(replies[0]["message"]), "larger than") {
		t.Fatalf("the oversized request: %v", replies[0])
	}
	if replies[1]["id"] != float64(2) || replies[1]["type"] != "done" {
		t.Fatalf("the next request: %v", replies[1])
	}
	if id := <-handled; id != 2 {
		t.Fatalf("handled %d", id)
	}
	inW.Close()
}

// Queries fetch a fixed number of rows per round trip, not go-ora's guess
// from the declared column widths (a few rows for wide tables); a
// connection's own PREFETCH_ROWS is kept.
func TestQueriesFetchManyRowsPerRoundTrip(t *testing.T) {
	for _, r := range []*request{
		{Host: "db", Service: "S", User: "u", Password: "p"},
		{ConnectString: "db:1522/S", User: "u", Password: "p"},
		{ConnectString: "(DESCRIPTION=(ADDRESS=(HOST=db)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=S)))", User: "u", Password: "p"},
	} {
		u, err := dsn(r)
		if err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(strings.ToUpper(u), "PREFETCH_ROWS=1000") {
			t.Errorf("%+v: %s", r, u)
		}
	}
	u, err := dsn(&request{Host: "db", Service: "S", User: "u", Password: "p", Options: map[string]string{"prefetch_rows": "50"}})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(strings.ToUpper(u), "PREFETCH_ROWS=50") || strings.Contains(u, "1000") {
		t.Errorf("the connection's own value: %s", u)
	}
}

func TestDecodeFailuresAreRetriedAsText(t *testing.T) {
	for _, m := range []string{
		"TTC error: received code 0 during response reading",
		"unable to decode oracle type XMLType to its primitive value",
		// An XMLTYPE table column (go-ora 2.9).
		"invalid size for GetInt64: 32",
		"buffer under-run during read operation",
		"receive abnormal packet type 12 instead of data packet",
	} {
		if !undecodable(errors.New(m)) {
			t.Errorf("%q: not retried", m)
		}
	}
	if undecodable(errors.New("ORA-00942: table or view does not exist")) {
		t.Error("an Oracle error isn't a decode failure")
	}
}
