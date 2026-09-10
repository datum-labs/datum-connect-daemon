package pidfile

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestPIDAlive_CurrentProcess(t *testing.T) {
	if !PIDAlive(os.Getpid()) {
		t.Error("PIDAlive should return true for current process")
	}
}

func TestPIDAlive_Zero(t *testing.T) {
	if PIDAlive(0) {
		t.Error("PIDAlive(0) should return false")
	}
}

func TestPIDAlive_Negative(t *testing.T) {
	if PIDAlive(-1) {
		t.Error("PIDAlive(-1) should return false")
	}
}

func TestListRunningTunnels_EmptyDir(t *testing.T) {
	dir := t.TempDir()
	tunnels, err := ListRunningTunnels(dir)
	if err != nil {
		t.Fatalf("ListRunningTunnels() failed: %v", err)
	}
	if len(tunnels) != 0 {
		t.Errorf("expected 0 tunnels, got %d", len(tunnels))
	}
}

func TestListRunningTunnels_WithFiles(t *testing.T) {
	dir := t.TempDir()
	tunnelsDir := filepath.Join(dir, "tunnels")
	os.MkdirAll(tunnelsDir, 0755)

	// Create a PID file for current process (should show as Running)
	path := filepath.Join(tunnelsDir, "mytun.pid")
	if err := Write(path, os.Getpid(), os.Getpid(), time.Now(), "/bin/fake"); err != nil {
		t.Fatalf("Write() failed: %v", err)
	}

	tunnels, err := ListRunningTunnels(dir)
	if err != nil {
		t.Fatalf("ListRunningTunnels() failed: %v", err)
	}
	if len(tunnels) != 1 {
		t.Fatalf("expected 1 tunnel, got %d", len(tunnels))
	}
	if tunnels[0].Name != "mytun" {
		t.Errorf("expected name 'mytun', got %q", tunnels[0].Name)
	}
	if tunnels[0].Status != "Running" {
		t.Errorf("expected status 'Running', got %q", tunnels[0].Status)
	}
}
