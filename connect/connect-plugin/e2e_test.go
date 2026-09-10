package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
)

func TestPluginManifestEmitsValidJSON(t *testing.T) {
	// PLUG-01: --plugin-manifest emits valid JSON and exits 0
	cmd := exec.Command(buildPlugin(t), "--plugin-manifest")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("--plugin-manifest exited non-zero: %v\n%s", err, out)
	}

	var manifest map[string]interface{}
	if err := json.Unmarshal(out, &manifest); err != nil {
		t.Fatalf("manifest is not valid JSON: %v\n%s", err, out)
	}

	if manifest["name"] != "connect" {
		t.Errorf("expected name='connect', got '%v'", manifest["name"])
	}
	if manifest["version"] != "v0.1.0-dev" {
		t.Errorf("expected version='v0.1.0-dev', got '%v'", manifest["version"])
	}
	if manifest["description"] != "Manage Datum Connect tunnels" {
		t.Errorf("expected description='Manage Datum Connect tunnels', got '%v'", manifest["description"])
	}
}

func TestPluginManifestExitsBeforeCobraParses(t *testing.T) {
	// --plugin-manifest must be handled before cobra parses args
	// so even invalid cobra flags should not prevent manifest output
	cmd := exec.Command(buildPlugin(t), "--plugin-manifest", "--invalid-cobra-flag")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("--plugin-manifest should exit 0 even with invalid flags: %v\n%s", err, out)
	}

	var manifest map[string]interface{}
	if err := json.Unmarshal(out, &manifest); err != nil {
		t.Fatalf("manifest is not valid JSON: %v\n%s", err, out)
	}
	if manifest["name"] != "connect" {
		t.Error("manifest name should be 'connect'")
	}
}

func TestAll12SubcommandsScaffolded(t *testing.T) {
	// PLUG-06: All 12 subcommands available in help
	cmd := exec.Command(buildPlugin(t), "tunnel", "--help")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("tunnel help exited non-zero: %v\n%s", err, out)
	}

	expectedSubcommands := []string{
		"list", "listen", "update", "delete",
		"ps", "stop", "logs", "status",
		"install", "uninstall", "start", "run",
	}

	available := string(out)
	for _, sub := range expectedSubcommands {
		if !strings.Contains(available, sub) {
			t.Errorf("tunnel help should list subcommand '%s'", sub)
		}
	}
}

func TestAllSubcommandsRunWithoutCrash(t *testing.T) {
	// All 12 subcommands should run without crashing (may exit non-zero for
	// missing required flags, but should not panic or produce stack traces)
	subcommands := []string{
		"list", "listen", "update", "delete",
		"ps", "stop", "logs", "status",
		"install", "uninstall", "start", "run",
	}
	pluginBin := buildPlugin(t)

	for _, subcmd := range subcommands {
		t.Run(subcmd, func(t *testing.T) {
			cmd := exec.Command(pluginBin, "tunnel", subcmd)
			cmd.Env = append(os.Environ(), "FAKE_DATUM_CONNECT=1")
			out, err := cmd.CombinedOutput()
			// May exit non-zero due to missing required flags — that's OK
			// as long as there's no panic/stack trace
			if err != nil {
				if bytes.Contains(out, []byte("panic:")) {
					t.Fatalf("%s panicked:\n%s", subcmd, out)
				}
			}
			if !bytes.Contains(out, []byte(subcmd)) && !bytes.Contains(out, []byte("Error:")) {
				// Some commands show usage on missing flags, others show "Error:"
				// Just verify output isn't empty
				if len(bytes.TrimSpace(out)) == 0 {
					t.Errorf("%s produced no output", subcmd)
				}
			}
		})
	}
}

func TestFakeDatumConnectListJSON(t *testing.T) {
	// Test fakes are functional and testable
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	cmd := exec.Command(fakeBin, "--json", "list")
	cmd.Env = append(os.Environ(), "DATUM_ACCESS_TOKEN=test-token")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("fake-datum-connect list --json exited non-zero: %v\n%s", err, out)
	}

	var tunnels []map[string]interface{}
	if err := json.Unmarshal(out, &tunnels); err != nil {
		t.Fatalf("fake output is not valid JSON: %v\n%s", err, out)
	}
	if len(tunnels) == 0 {
		t.Error("expected at least one tunnel in list output")
	}
}

func TestFakeDatumConnectDeleteJSON(t *testing.T) {
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	cmd := exec.Command(fakeBin, "--json", "delete")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("fake-datum-connect delete --json exited non-zero: %v\n%s", err, out)
	}

	var result map[string]interface{}
	if err := json.Unmarshal(out, &result); err != nil {
		t.Fatalf("delete output is not valid JSON: %v\n%s", err, out)
	}
	if deleted, ok := result["deleted"].(bool); !ok || !deleted {
		t.Error("delete should return {\"deleted\": true}")
	}
}

func TestFakeCredentialsHelperDefaultMode(t *testing.T) {
	fakeBin := buildFakeHelper(t, "testdata/fake-credentials-helper")
	cmd := exec.Command(fakeBin)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("fake-credentials-helper exited non-zero: %v\n%s", err, out)
	}

	token := strings.TrimSpace(string(out))
	if token == "" {
		t.Error("default mode should output a token")
	}
}

func TestFakeCredentialsHelperRefusesToken(t *testing.T) {
	fakeBin := buildFakeHelper(t, "testdata/fake-credentials-helper")
	cmd := exec.Command(fakeBin)
	cmd.Env = append(os.Environ(), "FAKE_HELPER_MODE=refuses-token")
	err := cmd.Run()
	if err == nil {
		t.Error("refuses-token mode should exit non-zero")
	}
}

func TestFakeCredentialsHelperSessionDependent(t *testing.T) {
	fakeBin := buildFakeHelper(t, "testdata/fake-credentials-helper")

	// Should succeed with matching session
	cmd := exec.Command(fakeBin, "--session", "test-session")
	cmd.Env = append(os.Environ(), "FAKE_HELPER_MODE=session-dependent")
	if err := cmd.Run(); err != nil {
		t.Fatalf("session-dependent with correct session should succeed: %v", err)
	}

	// Should fail with wrong session
	cmd = exec.Command(fakeBin, "--session", "wrong-session")
	cmd.Env = append(os.Environ(), "FAKE_HELPER_MODE=session-dependent")
	if err := cmd.Run(); err == nil {
		t.Error("session-dependent with wrong session should fail")
	}
}

func TestBuildScriptExists(t *testing.T) {
	// Build script exists and is executable
	info, err := os.Stat("scripts/build.sh")
	if err != nil {
		t.Fatalf("scripts/build.sh should exist: %v", err)
	}
	if info.Mode()&0111 == 0 {
		t.Error("scripts/build.sh should be executable")
	}
}

func TestReleaseScriptExists(t *testing.T) {
	// Release script exists and is executable
	info, err := os.Stat("scripts/release.sh")
	if err != nil {
		t.Fatalf("scripts/release.sh should exist: %v", err)
	}
	if info.Mode()&0111 == 0 {
		t.Error("scripts/release.sh should be executable")
	}
}

func buildFakeBinary(t *testing.T, src string) string {
	t.Helper()
	bin := src + "-test"
	cmd := exec.Command("go", "build", "-o", bin, "./"+src)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("failed to build %s: %v\n%s", src, err, out)
	}
	t.Cleanup(func() { os.Remove(bin) })
	return bin
}

// buildFakeDatumConnect builds the fake binary and returns its absolute path
// for use with FAKE_DATUM_CONNECT env var override in binary.Discover().
func buildFakeDatumConnect(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	bin := filepath.Join(dir, "fake-datum-connect")
	cmd := exec.Command("go", "build", "-o", bin, "./testdata/fake-datum-connect")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("failed to build fake-datum-connect: %v\n%s", err, out)
	}
	return bin
}

func buildFakeHelper(t *testing.T, src string) string {
	t.Helper()
	bin := src + "-test"
	cmd := exec.Command("go", "build", "-o", bin, "./"+src)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("failed to build %s: %v\n%s", src, err, out)
	}
	t.Cleanup(func() { os.Remove(bin) })
	return bin
}

// --- Plan 04-02 CRUD e2e tests ---

func TestListCommandWithFakeBinary(t *testing.T) {
	// CRUD-01: list delegates to Rust, renders table/json/yaml
	fakeBin := buildFakeDatumConnect(t)
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	pluginBin := buildPlugin(t)

	connectDir, _ := os.Getwd()
	cmd := exec.Command(pluginBin, "tunnel", "list")
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
	output := string(out)
	if !strings.Contains(output, "dev-server") {
		t.Errorf("table output should contain tunnel label 'dev-server': %s", output)
	}
	if !strings.Contains(output, "localhost:8080") {
		t.Errorf("table output should contain endpoint 'localhost:8080': %s", output)
	}

	// Test JSON output
	cmd = exec.Command(pluginBin, "tunnel", "list", "--output", "json")
	cmd.Env = append(os.Environ(),
		"FAKE_DATUM_CONNECT="+fakeBin,
		"DATUM_CREDENTIALS_HELPER="+fakeHelper,
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR="+connectDir,
		"PATH="+connectDir+":"+os.Getenv("PATH"))
	out, err = cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("list --output json exited non-zero: %v\n%s", err, out)
	}
	var tunnels []map[string]interface{}
	if err := json.Unmarshal(out, &tunnels); err != nil {
		t.Fatalf("json output is not valid JSON: %v\n%s", err, out)
	}
	if len(tunnels) != 2 {
		t.Errorf("expected 2 tunnels, got %d", len(tunnels))
	}
}

func TestDeleteCommandWithFakeBinary(t *testing.T) {
	// CRUD-04: delete delegates to Rust with correct output
	fakeBin := buildFakeDatumConnect(t)
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	pluginBin := buildPlugin(t)

	connectDir, _ := os.Getwd()
	cmd := exec.Command(pluginBin, "tunnel", "delete", "--id", "tun-123", "--output", "json")
	cmd.Env = append(os.Environ(),
		"FAKE_DATUM_CONNECT="+fakeBin,
		"DATUM_CREDENTIALS_HELPER="+fakeHelper,
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR="+connectDir,
		"PATH="+connectDir+":"+os.Getenv("PATH"))
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("delete exited non-zero: %v\n%s", err, out)
	}

	var result map[string]interface{}
	if err := json.Unmarshal(out, &result); err != nil {
		t.Fatalf("delete output is not valid JSON: %v\n%s", err, out)
	}
	if deleted, ok := result["deleted"].(bool); !ok || !deleted {
		t.Error("delete should return {\"deleted\": true}")
	}
}

func TestDeleteCommandMissingID(t *testing.T) {
	// EXIT-02: missing required flag exits with POSIX code 64
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "delete")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, err := cmd.CombinedOutput()
	if err == nil {
		t.Error("delete without --id should exit non-zero")
	}
	if exitErr, ok := err.(*exec.ExitError); ok {
		if exitErr.ExitCode() != 64 {
			t.Errorf("expected exit code 64 (semantic rejection), got %d", exitErr.ExitCode())
		}
	}
	if !bytes.Contains(out, []byte("required")) {
		t.Error("delete without --id should show 'required' error message")
	}
}

// --- Plan 04-03 listen e2e tests ---

func TestListenCommandWithFakeBinary(t *testing.T) {
	// CRUD-02, CRUD-06: listen creates tunnel, blocks, handles signals
	fakeBin := buildFakeDatumConnect(t)
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	pluginBin := buildPlugin(t)

	connectDir, _ := os.Getwd()
	// Start listen command
	cmd := exec.Command(pluginBin, "tunnel", "listen", "--endpoint", "localhost:8080")
	cmd.Env = append(os.Environ(),
		"FAKE_DATUM_CONNECT="+fakeBin,
		"DATUM_CREDENTIALS_HELPER="+fakeHelper,
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR="+connectDir,
		"PATH="+connectDir+":"+os.Getenv("PATH"))

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("failed to get stdout pipe: %v", err)
	}

	if err := cmd.Start(); err != nil {
		t.Fatalf("listen failed to start: %v", err)
	}

	// Read output until we see "Tunnel ready" or "Press Ctrl+C"
	scanner := bufio.NewScanner(stdout)
	var foundReady bool
	for scanner.Scan() {
		line := scanner.Text()
		if strings.Contains(line, "ready") || strings.Contains(line, "Tunnel") {
			foundReady = true
			break
		}
	}
	if !foundReady {
		t.Error("listen should print tunnel ready message")
	}

	// Send SIGINT to stop
	if err := cmd.Process.Signal(syscall.SIGINT); err != nil {
		t.Fatalf("failed to send SIGINT: %v", err)
	}

	// Wait for child to exit
	if err := cmd.Wait(); err != nil {
		// Non-nil error is expected (signal termination)
		if exitErr, ok := err.(*exec.ExitError); ok {
			// SIGINT typically gives exit code 130
			if exitErr.ExitCode() != 130 && exitErr.ExitCode() != 0 {
				t.Logf("listen exited with code %d (expected 0 or 130)", exitErr.ExitCode())
			}
		}
	}
}

func TestListenJSONMode(t *testing.T) {
	// CRUD-05: listen --json emits single JSON object on ready
	fakeBin := buildFakeDatumConnect(t)
	fakeHelper := buildFakeHelper(t, "testdata/fake-credentials-helper")
	pluginBin := buildPlugin(t)

	connectDir, _ := os.Getwd()
	cmd := exec.Command(pluginBin, "tunnel", "listen", "--endpoint", "localhost:8080", "--output", "json")
	cmd.Env = append(os.Environ(),
		"FAKE_DATUM_CONNECT="+fakeBin,
		"DATUM_CREDENTIALS_HELPER="+fakeHelper,
		"DATUM_SESSION=dev",
		"DATUM_CONNECT_DIR="+connectDir,
		"PATH="+connectDir+":"+os.Getenv("PATH"))

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("failed to get stdout pipe: %v", err)
	}

	if err := cmd.Start(); err != nil {
		t.Fatalf("listen --json failed to start: %v", err)
	}

	// Read first line — should be the ready JSON
	scanner := bufio.NewScanner(stdout)
	if !scanner.Scan() {
		t.Fatal("listen --json should output ready JSON")
	}
	firstLine := scanner.Bytes()

	var ready map[string]interface{}
	if err := json.Unmarshal(firstLine, &ready); err != nil {
		t.Fatalf("first line is not valid JSON: %s", firstLine)
	}
	if ready["status"] != "ready" {
		t.Errorf("expected status='ready', got '%v'", ready["status"])
	}

	// Send SIGINT to stop
	if err := cmd.Process.Signal(syscall.SIGINT); err != nil {
		t.Fatalf("failed to send SIGINT: %v", err)
	}
	cmd.Wait()
}

func TestPluginDefaultsConnectDir(t *testing.T) {
	// When DATUM_CONNECT_DIR is unset, the plugin should compute the
	// canonical default $HOME/.datumctl/connect and proceed (not exit 64).
	// This allows the plugin to work without datumctl host injection.
	pluginBin := buildPlugin(t)

	// Strip DATUM_CONNECT_DIR (and the legacy DATUM_CONNECT_REPO) from env.
	env := []string{}
	for _, e := range os.Environ() {
		if strings.HasPrefix(e, "DATUM_CONNECT_DIR=") {
			continue
		}
		if strings.HasPrefix(e, "DATUM_CONNECT_REPO=") {
			continue
		}
		env = append(env, e)
	}

	// Run tunnel list — should NOT exit 64. Will fail with "binary not
	// found" because no fake binary is set up, but that's a different error.
	cmd := exec.Command(pluginBin, "tunnel", "list")
	cmd.Env = env
	out, err := cmd.CombinedOutput()

	if err == nil {
		t.Fatalf("expected non-zero exit (binary not found); got success with output:\n%s", out)
	}
	exitErr, ok := err.(*exec.ExitError)
	if !ok {
		t.Fatalf("expected *exec.ExitError, got %T: %v", err, err)
	}
	if exitErr.ExitCode() == 64 {
		t.Errorf("plugin exited 64 when DATUM_CONNECT_DIR was unset; should have computed default instead:\n%s", out)
	}
	if bytes.Contains(out, []byte("DATUM_CONNECT_DIR is not set")) {
		t.Errorf("unexpected 'DATUM_CONNECT_DIR is not set' message — plugin should have auto-computed the default:\n%s", out)
	}
}

func TestPluginManifestProbeWorksWithoutConnectDir(t *testing.T) {
	// The --plugin-manifest probe must work even when DATUM_CONNECT_DIR
	// is unset (datumctl probes plugins before injecting env).
	// plugin.ServeManifest self-exits 0 before our RequireConnectDir
	// check runs; this test pins that ordering.
	pluginBin := buildPlugin(t)
	env := []string{}
	for _, e := range os.Environ() {
		if strings.HasPrefix(e, "DATUM_CONNECT_DIR=") {
			continue
		}
		env = append(env, e)
	}
	cmd := exec.Command(pluginBin, "--plugin-manifest")
	cmd.Env = env
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("--plugin-manifest must exit 0 even without DATUM_CONNECT_DIR; err=%v, out=%s", err, out)
	}
	if !bytes.Contains(out, []byte("{")) {
		t.Errorf("--plugin-manifest output should be JSON; got:\n%s", out)
	}
}

func TestListenMissingEndpointAndId(t *testing.T) {
	// EXIT-02: missing both --endpoint and --id exits with code 64.
	// 12-02 expanded the validation: either --endpoint or --id satisfies
	// the requirement; neither still rejects.
	pluginBin := buildPlugin(t)
	cmd := exec.Command(pluginBin, "tunnel", "listen")
	cmd.Env = append(os.Environ(), "DATUM_CONNECT_DIR="+t.TempDir())
	out, err := cmd.CombinedOutput()
	if err == nil {
		t.Error("listen without --endpoint or --id should exit non-zero")
	}
	if exitErr, ok := err.(*exec.ExitError); ok {
		if exitErr.ExitCode() != 64 {
			t.Errorf("expected exit code 64 (semantic rejection), got %d", exitErr.ExitCode())
		}
	}
	if !bytes.Contains(out, []byte("required")) {
		t.Error("listen without --endpoint or --id should show 'required' error message")
	}
}
