package pidfile

import (
	"path/filepath"
	"testing"
	"time"
)

func TestWriteAndRead(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "test.pid")
	start := time.Date(2026, 6, 6, 12, 0, 0, 0, time.UTC)

	err := Write(path, 1001, 1002, start, "/usr/bin/datum-connect")
	if err != nil {
		t.Fatalf("Write() failed: %v", err)
	}

	p, err := Read(path)
	if err != nil {
		t.Fatalf("Read() failed: %v", err)
	}

	if p.GoPID != 1001 {
		t.Errorf("GoPID = %d, want 1001", p.GoPID)
	}
	if p.RustPID != 1002 {
		t.Errorf("RustPID = %d, want 1002", p.RustPID)
	}
	if !p.StartTime.Equal(start) {
		t.Errorf("StartTime = %v, want %v", p.StartTime, start)
	}
	if p.BinaryPath != "/usr/bin/datum-connect" {
		t.Errorf("BinaryPath = %q, want %q", p.BinaryPath, "/usr/bin/datum-connect")
	}
}

func TestReadMissingFile(t *testing.T) {
	_, err := Read("/nonexistent/pid")
	if err == nil {
		t.Fatal("Read() should fail for missing file")
	}
}

func TestExists(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "exists.pid")

	if Exists(path) {
		t.Error("Exists() should be false before file is created")
	}

	Write(path, 1, 2, time.Now(), "/bin/fake")
	if !Exists(path) {
		t.Error("Exists() should be true after file is created")
	}
}

func TestParse(t *testing.T) {
	data := []byte("1001\n1002\n2026-06-06T12:00:00Z\n/usr/bin/datum-connect\n")
	p, err := Parse(data)
	if err != nil {
		t.Fatalf("Parse() failed: %v", err)
	}
	if p.GoPID != 1001 || p.RustPID != 1002 {
		t.Errorf("unexpected pids: go=%d rust=%d", p.GoPID, p.RustPID)
	}
}

func TestRemove(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "remove.pid")
	Write(path, 1, 2, time.Now(), "/bin/fake")

	if err := Remove(path); err != nil {
		t.Fatalf("Remove() failed: %v", err)
	}
	if Exists(path) {
		t.Error("file should not exist after Remove")
	}

	// Remove again should not error
	if err := Remove(path); err != nil {
		t.Errorf("Remove() on missing file should not error: %v", err)
	}
}
