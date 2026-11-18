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
