package state

import (
	"runtime"
	"strings"
	"testing"
)

func TestDir_NotEmpty(t *testing.T) {
	d := Dir()
	if d == "" {
		t.Fatal("Dir() returned empty string")
	}
	if !strings.Contains(d, "datumctl") {
		t.Errorf("Dir() should contain 'datumctl', got %q", d)
	}
}

func TestDir_IncludesConnect(t *testing.T) {
	d := Dir()
	if !strings.HasSuffix(d, "connect") && !strings.HasSuffix(d, "connect/") {
		t.Errorf("Dir() should end with 'connect', got %q", d)
	}
}

func TestTunnelDir(t *testing.T) {
	td := TunnelDir()
	if !strings.HasSuffix(td, "tunnels") && !strings.HasSuffix(td, "tunnels/") {
		t.Errorf("TunnelDir() should end with 'tunnels', got %q", td)
	}
}

func TestPidFilePath(t *testing.T) {
	p := PidFilePath("mytun")
	if !strings.HasSuffix(p, "mytun.pid") {
		t.Errorf("PidFilePath('mytun') should end with 'mytun.pid', got %q", p)
	}
}

func TestLogDir(t *testing.T) {
	ld := LogDir()
	if runtime.GOOS == "darwin" && !strings.Contains(ld, "Library/Logs") && !strings.Contains(ld, "Library\\Logs") {
		t.Errorf("LogDir() on darwin should contain Library/Logs, got %q", ld)
	}
}

func TestLogFilePath(t *testing.T) {
	p := LogFilePath("mytun")
	if !strings.HasSuffix(p, "mytun.log") {
		t.Errorf("LogFilePath('mytun') should end with 'mytun.log', got %q", p)
	}
}
