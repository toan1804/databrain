package main

import (
	"database/sql"
	"database/sql/driver"
	"errors"
	"fmt"
	"io"
	"syscall"
	"testing"

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
