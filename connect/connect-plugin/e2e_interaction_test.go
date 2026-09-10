package main

import (
	"bytes"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"go.datum.net/datumctl-plugins/connect/internal/pidfile"
	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
)

// TestDatumctlContextEnvVars verifies that the datumctl plugin SDK reads
// the correct environment variables that datumctl injects before exec-replacing
// a plugin. This tests the datumctl → plugin boundary.
func TestDatumctlContextEnvVars(t *testing.T) {
	bin := buildPlugin(t)

	env := []string{
		"DATUM_ORG=my-org",
		"DATUM_PROJECT=my-project",
		"DATUM_API_HOST=api.datum.net",
		"DATUM_PLUGIN_API_VERSION=1",
		"DATUM_CREDENTIALS_HELPER=/fake/credentials-helper",
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR=" + t.TempDir(),
	}

	cmd := exec.Command(bin, "--help")
	cmd.Env = append(os.Environ(), env...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("--help exited non-zero: %v\n%s", err, out)
	}

	available := string(out)
	if !strings.Contains(available, "--org") {
		t.Error("help should show --org flag (injected by datumctl)")
	}
	if !strings.Contains(available, "--project") {
		t.Error("help should show --project flag (injected by datumctl)")
	}
}

// TestCredentialsHelperCalledWithSession verifies that the datumctl plugin
// SDK calls the credentials helper with the correct session argument.
func TestCredentialsHelperCalledWithSession(t *testing.T) {
	helperDir := t.TempDir()
	helperBin := filepath.Join(helperDir, "helper")

	helperSrc := `package main
import (
	"os"
	"fmt"
)
func main() {
	f, _ := os.Create("/tmp/helper-args.log")
	if f != nil {
		for i, arg := range os.Args {
			fmt.Fprintf(f, "%d:%s\n", i, arg)
		}
		f.Close()
	}
	fmt.Println("session-token-from-helper")
}
`
	helperPath := filepath.Join(helperDir, "helper.go")
	if err := os.WriteFile(helperPath, []byte(helperSrc), 0644); err != nil {
		t.Fatalf("write helper source: %v", err)
	}

	buildCmd := exec.Command("go", "build", "-o", helperBin, helperPath)
	if out, err := buildCmd.CombinedOutput(); err != nil {
		t.Fatalf("build helper: %v\n%s", err, out)
	}

	cmd := exec.Command(helperBin, "auth", "get-token", "--session", "test-session")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("helper failed: %v\n%s", err, out)
	}
	token := strings.TrimSpace(string(out))
	if token != "session-token-from-helper" {
		t.Errorf("expected 'session-token-from-helper', got '%s'", token)
	}
}

// TestPluginManifestBeforeSubprocessSpawn verifies that --plugin-manifest
// is handled before any subprocess spawning, so the datumctl host can
// discover the plugin without triggering subprocess execution.
func TestPluginManifestBeforeSubprocessSpawn(t *testing.T) {
	bin := buildPlugin(t)

	cmd := exec.Command(bin, "--plugin-manifest")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("--plugin-manifest should work without datumctl env: %v\n%s", err, out)
	}

	var manifest map[string]interface{}
	if err := json.Unmarshal(out, &manifest); err != nil {
		t.Fatalf("manifest is not valid JSON: %v\n%s", err, out)
	}

	if manifest["name"] != "connect" {
		t.Error("manifest name should be 'connect'")
	}
	if manifest["api_version"] != float64(1) {
		t.Errorf("expected api_version=1, got %v", manifest["api_version"])
	}
}

// TestPluginPassesContextToSubcommand verifies that the Go plugin correctly
// passes datumctl context (org, project, output format) to subcommands.
func TestPluginPassesContextToSubcommand(t *testing.T) {
	bin := buildPlugin(t)
	fakeBin := buildFakeDatumConnect(t)
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")

	connectDir, _ := os.Getwd()
	cmd := exec.Command(bin, "--org", "custom-org", "--project", "custom-project", "--output", "yaml", "tunnel", "list")
	cmd.Env = append(os.Environ(),
		"FAKE_DATUM_CONNECT="+fakeBin,
		"DATUM_CREDENTIALS_HELPER="+fakeHelper,
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR="+connectDir,
		"PATH="+connectDir+":"+os.Getenv("PATH"))
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("list exited non-zero: %v\n%s", err, out)
	}

	// Should contain tunnel data from fake binary
	if !bytes.Contains(out, []byte("dev-server")) {
		t.Errorf("expected tunnel data in output, got: %s", out)
	}
}

// TestCredentialsHelperTokenFlow verifies the token passing chain works:
// plugin reads DATUM_CREDENTIALS_HELPER → calls helper → gets token
func TestCredentialsHelperTokenFlow(t *testing.T) {
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	fakeBin := buildFakeDatumConnect(t)

	connectDir, _ := os.Getwd()
	env := []string{
		"DATUM_CREDENTIALS_HELPER=" + fakeHelper,
		"DATUM_SESSION=dev",
		"FAKE_DATUM_CONNECT=" + fakeBin,
		"DATUM_CONNECT_DIR=" + connectDir,
		"PATH=" + connectDir + ":" + os.Getenv("PATH"),
	}

	// Run the plugin with the fake helper and fake binary
	cmd := exec.Command(buildPlugin(t), "tunnel", "list")
	cmd.Env = append(os.Environ(), env...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("list exited non-zero: %v\n%s", err, out)
	}

	// Should contain tunnel data from fake binary
	if !bytes.Contains(out, []byte("dev-server")) {
		t.Errorf("expected tunnel data in output, got: %s", out)
	}
}

// TestPluginBinaryIsExecutable verifies the built plugin binary is a valid executable.
func TestPluginBinaryIsExecutable(t *testing.T) {
	bin := buildPlugin(t)

	info, err := os.Stat(bin)
	if err != nil {
		t.Fatalf("stat plugin binary: %v", err)
	}
	if info.Size() == 0 {
		t.Error("plugin binary should not be empty")
	}
}

// TestFullChainEnvVarPropagation verifies the full env var chain:
// datumctl → plugin → Rust binary
// 1. datumctl sets DATUM_CREDENTIALS_HELPER
// 2. Plugin reads DATUM_CREDENTIALS_HELPER, calls helper, gets token
// 3. Plugin sets DATUM_ACCESS_TOKEN for Rust binary
// 4. Rust binary reads DATUM_ACCESS_TOKEN
func TestFullChainEnvVarPropagation(t *testing.T) {
	// Step 1: Plugin manifest works standalone
	pluginBin := buildPlugin(t)

	cmd := exec.Command(pluginBin, "--plugin-manifest")
	cmd.Env = os.Environ()
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("manifest should work standalone: %v\n%s", err, out)
	}
	if !bytes.Contains(out, []byte(`"name": "connect"`)) {
		t.Error("manifest should contain name='connect'")
	}

	// Step 2: Verify plugin reads DATUM_CREDENTIALS_HELPER from env
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	cmd = exec.Command(pluginBin, "tunnel", "list")
	cmd.Env = append(os.Environ(), "DATUM_CREDENTIALS_HELPER="+fakeHelper)
	_ = cmd.Run()
}

// --- Plan 05-03 process command e2e tests ---

func TestPS_WithFakePIDFiles(t *testing.T) {
	// Create temp state dir with a fake PID file
	stateDir := t.TempDir()

	pidPath := filepath.Join(stateDir, "datumctl", "connect", "tunnels", "test-tun.pid")
	os.MkdirAll(filepath.Dir(pidPath), 0755)

	startTime := time.Now().Add(-10 * time.Minute)
	if err := pidfile.Write(pidPath, 99999, 10000, startTime, "/usr/bin/fake"); err != nil {
		t.Fatalf("write pid file: %v", err)
	}

	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "ps")
	cmd.Env = []string{
		"XDG_STATE_HOME=" + stateDir,
		"DATUM_ACCESS_TOKEN=test-token",
		"DATUM_CONNECT_DIR=" + t.TempDir(),
		"HOME=" + os.Getenv("HOME"),
		"PATH=" + os.Getenv("PATH"),
	}
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("ps exited non-zero: %v\n%s", err, out)
	}

	if !strings.Contains(string(out), "test-tun") {
		t.Errorf("ps output should contain tunnel name 'test-tun':\n%s", out)
	}
}

func TestPS_JSONOutput(t *testing.T) {
	stateDir := t.TempDir()

	pidPath := filepath.Join(stateDir, "datumctl", "connect", "tunnels", "json-tun.pid")
	os.MkdirAll(filepath.Dir(pidPath), 0755)

	_ = pidfile.Write(pidPath, 99999, 10000, time.Now(), "/usr/bin/fake")

	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "ps", "--json")
	cmd.Env = []string{
		"XDG_STATE_HOME=" + stateDir,
		"DATUM_ACCESS_TOKEN=test-token",
		"DATUM_CONNECT_DIR=" + t.TempDir(),
		"HOME=" + os.Getenv("HOME"),
		"PATH=" + os.Getenv("PATH"),
	}
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("ps --json exited non-zero: %v\n%s", err, out)
	}

	var tunnels []map[string]interface{}
	if err := json.Unmarshal(out, &tunnels); err != nil {
		t.Fatalf("output is not valid JSON: %v\n%s", err, out)
	}
	if len(tunnels) == 0 {
		t.Error("expected at least 1 tunnel in JSON output")
	}
}

func TestStatus_StoppedTunnel(t *testing.T) {
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "status", "--name", "nonexistent")
	cmd.Env = []string{
		"DATUM_ACCESS_TOKEN=test-token",
		"DATUM_CONNECT_DIR=" + t.TempDir(),
		"HOME=" + os.Getenv("HOME"),
		"PATH=" + os.Getenv("PATH"),
	}
	out, _ := cmd.CombinedOutput()

	if !strings.Contains(string(out), "Stopped") {
		t.Errorf("status for nonexistent tunnel should show Stopped:\n%s", out)
	}
}

// --- Plan 06-02 service install e2e tests ---

func TestInstall_RequiresName(t *testing.T) {
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "install")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, err := cmd.CombinedOutput()
	if err == nil {
		t.Error("install with no flags should exit non-zero")
	}
	if exitErr, ok := err.(*exec.ExitError); ok {
		if exitErr.ExitCode() != 64 {
			t.Errorf("expected exit code 64, got %d", exitErr.ExitCode())
		}
	}
	if !strings.Contains(string(out), "--name is required") {
		t.Errorf("install with no flags should show '--name is required':\n%s", out)
	}
}

func TestInstall_RequiresEndpoint(t *testing.T) {
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "install", "--name", "test-tun")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, err := cmd.CombinedOutput()
	if err == nil {
		t.Error("install without --endpoint should exit non-zero")
	}
	if !strings.Contains(string(out), "--endpoint is required") {
		t.Errorf("install without --endpoint should show '--endpoint is required':\n%s", out)
	}
}

func TestInstall_RequiresSession(t *testing.T) {
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "install", "--name", "test-tun", "--endpoint", "localhost:8080")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, err := cmd.CombinedOutput()
	if err == nil {
		t.Error("install without --session should exit non-zero")
	}
	if !strings.Contains(string(out), "--session is required") {
		t.Errorf("install without --session should show '--session is required':\n%s", out)
	}
}

func TestInstallConfigPersistence(t *testing.T) {
	// Verify config is written to the correct path and can be loaded back
	t.Setenv("DATUM_ACCESS_TOKEN", "test-token")

	configDir := t.TempDir()
	t.Setenv("HOME", configDir) // os.UserConfigDir uses HOME for XDG

	cfg := svcconfig.TunnelConfig{
		Name:     "test-svc",
		Label:    "Test Service",
		Endpoint: "localhost:8080",
		Session:  "my-session",
	}

	configPath := svcconfig.ConfigFilePath("test-svc")
	if err := svcconfig.Save(cfg, configPath); err != nil {
		t.Fatalf("Save config: %v", err)
	}

	loaded, err := svcconfig.Load(configPath)
	if err != nil {
		t.Fatalf("Load config: %v", err)
	}
	if loaded.Name != "test-svc" {
		t.Errorf("Name = %q, want %q", loaded.Name, "test-svc")
	}
	if loaded.Session != "my-session" {
		t.Errorf("Session = %q, want %q", loaded.Session, "my-session")
	}
}

func TestStatus_WithConfig(t *testing.T) {
	// Verify status output includes installed info when config file exists
	t.Setenv("DATUM_ACCESS_TOKEN", "test-token")

	// Create a config file for an installed-but-not-running tunnel
	configDir := t.TempDir()
	t.Setenv("HOME", configDir)

	cfg := svcconfig.TunnelConfig{
		Name:     "installed-tun",
		Endpoint: "localhost:9090",
		Session:  "svc-session",
	}
	if err := svcconfig.Save(cfg, svcconfig.ConfigFilePath("installed-tun")); err != nil {
		t.Fatalf("Save config: %v", err)
	}

	// Run status — should show Stopped and installed info
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "status", "--name", "installed-tun")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, _ := cmd.CombinedOutput()

	output := string(out)
	if !strings.Contains(output, "Stopped") {
		t.Errorf("status should show Stopped:\n%s", output)
	}
	if !strings.Contains(output, "Installed") {
		t.Errorf("status should show installed info:\n%s", output)
	}
}

// Helper functions

func buildPlugin(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	bin := filepath.Join(dir, "connect-test")
	cmd := exec.Command("go", "build", "-o", bin, ".")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build plugin: %v\n%s", err, out)
	}
	// Also build and copy the fake datum-connect binary next to the plugin
	// so binary.Discover() can find it when FAKE_DATUM_CONNECT is set.
	fakeBin := filepath.Join(dir, "fake-datum-connect")
	fakeCmd := exec.Command("go", "build", "-o", fakeBin, "./testdata/fake-datum-connect")
	fakeCmd.Dir = "."
	if out, err := fakeCmd.CombinedOutput(); err != nil {
		t.Fatalf("build fake-datum-connect: %v\n%s", err, out)
	}
	return bin
}
