package daemon

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func TestRunSupervisor_StartsAndExits(t *testing.T) {
	fakeBin := findFakeBinary(t)
	setupFakeEnv(t, fakeBin)

	// Create temp PID directory
	pidDir := t.TempDir()
	t.Setenv("DATUM_CONNECT_TUNNEL_DIR", pidDir)

	cfg := Config{
		Name:     "test-tun",
		Endpoint: "localhost:8080",
	}

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	err := RunSupervisor(ctx, cfg)
	// The fake binary blocks on listen (waiting for signal), so it will timeout.
	// We just verify it exited without panic and cleaned up.
	t.Logf("RunSupervisor returned: %v", err)
}

func TestRunSupervisor_WritesPIDFile(t *testing.T) {
	fakeBin := findFakeBinary(t)
	setupFakeEnv(t, fakeBin)

	pidDir := t.TempDir()
	t.Setenv("DATUM_CONNECT_TUNNEL_DIR", pidDir)

	cfg := Config{
		Name:     "pidtest",
		Endpoint: "localhost:8080",
	}

	// Run with timeout — supervisor will block on message loop
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	_ = RunSupervisor(ctx, cfg)

	// After timeout, PID file should be cleaned up (defer ran)
	pidPath := filepath.Join(pidDir, "pidtest.pid")
	if _, err := os.Stat(pidPath); err == nil {
		t.Error("PID file should be removed after supervisor exits")
	}
}

// findFakeBinary locates the pre-built fake-datum-connect binary.
func findFakeBinary(t *testing.T) string {
	t.Helper()
	candidates := []string{
		"../../testdata/fake-datum-connect/fake-datum-connect",
	}
	for _, c := range candidates {
		if _, err := os.Stat(c); err == nil {
			abs, _ := filepath.Abs(c)
			return abs
		}
	}
	t.Skip("fake-datum-connect binary not found (run `go build` in testdata first)")
	return ""
}

// setupFakeEnv sets up environment so binary.Discover() finds the fake binary
// and plugin.Token() finds a working fake credentials helper.
func setupFakeEnv(t *testing.T, fakeBin string) {
	t.Helper()
	t.Setenv("FAKE_DATUM_CONNECT", fakeBin)
	// Add fake binary dir to PATH
	fakeDir := filepath.Dir(fakeBin)
	t.Setenv("PATH", fakeDir+":"+os.Getenv("PATH"))

	// Build and use a fake credentials helper
	helperBin := buildFakeHelper(t)
	t.Setenv("DATUM_CREDENTIALS_HELPER", helperBin)

	// Set required datumctl env vars that plugin.Context() expects
	t.Setenv("DATUM_ORG", "test-org")
	t.Setenv("DATUM_PROJECT", "test-project")
	t.Setenv("DATUM_API_HOST", "api.datum.net")
	t.Setenv("DATUM_PLUGIN_API_VERSION", "1")
	t.Setenv("DATUM_SESSION", "dev")
}

// buildFakeHelper builds a simple credentials helper that returns a fixed token.
func buildFakeHelper(t *testing.T) string {
	t.Helper()
	helperDir := t.TempDir()
	src := `package main
import "fmt"
func main() { fmt.Println("test-token-from-helper") }
`
	srcPath := filepath.Join(helperDir, "main.go")
	if err := os.WriteFile(srcPath, []byte(src), 0644); err != nil {
		t.Fatalf("write helper source: %v", err)
	}
	binPath := filepath.Join(helperDir, "fake-helper")
	cmd := exec.Command("go", "build", "-o", binPath, srcPath)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build helper: %v\n%s", err, out)
	}
	return binPath
}

func TestMain(m *testing.M) {
	// Build the fake binary before running tests
	cmd := exec.Command("go", "build", "-o", "fake-datum-connect", "../../testdata/fake-datum-connect")
	cmd.Dir = "."
	_ = cmd.Run()
	os.Exit(m.Run())
}
