package exec

import (
	"context"
	"os"
	"os/exec"
	"strings"
	"testing"
)

func buildFakeBinary(t *testing.T, src string) string {
	t.Helper()
	// Build from connect-plugin/ module root — use absolute path for reliability
	bin := "fake-datum-connect-test"
	cmd := exec.Command("go", "build", "-o", bin, "./"+src)
	cmd.Dir = "/home/drewr/src/datum-connect-plugin-build/connect/connect-plugin"
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("failed to build %s: %v\n%s", src, err, out)
	}
	t.Cleanup(func() { os.Remove(bin) })
	// Return absolute path so Run() can find it regardless of CWD
	absBin := "/home/drewr/src/datum-connect-plugin-build/connect/connect-plugin/" + bin
	return absBin
}

func TestRunWithValidBinary(t *testing.T) {
	// Test 1: Run() with valid binary, args, env — returns result with stdout, stderr, exit code 0
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	env := []string{"DATUM_ACCESS_TOKEN=test-token"}

	result, err := Run(context.Background(), fakeBin, []string{"--json", "list"}, env, OutputModeJSON)
	if err != nil {
		t.Fatalf("Run() returned error: %v", err)
	}
	if result.ExitCode != 0 {
		t.Errorf("expected exit code 0, got %d", result.ExitCode)
	}
	if len(result.Stdout) == 0 {
		t.Error("expected stdout to be non-empty")
	}
	// Verify it's valid JSON from the fake binary
	if !strings.Contains(string(result.Stdout), "tun-123") {
		t.Errorf("expected stdout to contain 'tun-123', got: %s", string(result.Stdout))
	}
}

func TestRunWithNonZeroExit(t *testing.T) {
	// Test 2: Run() with binary that exits non-zero — returns captured output and exit code
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	env := []string{"FAKE_DUMMY_MODE=child-crash"}

	result, err := Run(context.Background(), fakeBin, []string{"--json", "list"}, env, OutputModeJSON)
	if err != nil {
		t.Fatalf("Run() returned error (expected nil for non-zero exit): %v", err)
	}
	if result.ExitCode == 0 {
		t.Error("expected non-zero exit code from child crash")
	}
}

func TestRunWithNotFoundBinary(t *testing.T) {
	// Test 3: Run() with binary not found — returns error (not wrapped in result)
	_, err := Run(context.Background(), "/nonexistent/binary", []string{"list"}, nil, OutputModeJSON)
	if err == nil {
		t.Fatal("expected error for non-existent binary, got nil")
	}
	if !strings.Contains(err.Error(), "failed to start") {
		t.Errorf("expected 'failed to start' in error, got: %v", err)
	}
}

func TestRunWithOutputModeYAML(t *testing.T) {
	// Test 4: Run() with OutputModeYAML — stdout is YAML-converted from JSON
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	env := []string{"DATUM_ACCESS_TOKEN=test-token"}

	result, err := Run(context.Background(), fakeBin, []string{"--json", "list"}, env, OutputModeYAML)
	if err != nil {
		t.Fatalf("Run() returned error: %v", err)
	}
	if result.ExitCode != 0 {
		t.Errorf("expected exit code 0, got %d", result.ExitCode)
	}
	// YAML output should not be raw JSON — it should contain YAML markers
	yamlStr := string(result.Stdout)
	if strings.Contains(yamlStr, "[{") {
		t.Errorf("expected YAML output, got raw JSON: %s", yamlStr)
	}
	if !strings.Contains(yamlStr, "tun-123") {
		t.Errorf("expected YAML output to contain 'tun-123', got: %s", yamlStr)
	}
}

func TestRunWithOutputModeJSON(t *testing.T) {
	// Test 5: Run() with OutputModeJSON — stdout is passed through as-is
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	env := []string{"DATUM_ACCESS_TOKEN=test-token"}

	result, err := Run(context.Background(), fakeBin, []string{"--json", "list"}, env, OutputModeJSON)
	if err != nil {
		t.Fatalf("Run() returned error: %v", err)
	}
	if result.ExitCode != 0 {
		t.Errorf("expected exit code 0, got %d", result.ExitCode)
	}
	// JSON output should be raw JSON
	if !strings.Contains(string(result.Stdout), "tun-123") {
		t.Errorf("expected JSON output to contain 'tun-123', got: %s", string(result.Stdout))
	}
}

func TestRunWithOutputModeTable(t *testing.T) {
	// Test 6: Run() with OutputModeTable — stdout is rendered as a table
	fakeBin := buildFakeBinary(t, "testdata/fake-datum-connect")
	env := []string{"DATUM_ACCESS_TOKEN=test-token"}

	result, err := Run(context.Background(), fakeBin, []string{"--json", "list"}, env, OutputModeTable)
	if err != nil {
		t.Fatalf("Run() returned error: %v", err)
	}
	if result.ExitCode != 0 {
		t.Errorf("expected exit code 0, got %d", result.ExitCode)
	}
	// Table output should contain tab-separated values
	tableStr := string(result.Stdout)
	if !strings.Contains(tableStr, "dev-server") {
		t.Errorf("expected table output to contain 'dev-server', got: %s", tableStr)
	}
	if !strings.Contains(tableStr, "localhost:8080") {
		t.Errorf("expected table output to contain 'localhost:8080', got: %s", tableStr)
	}
}

func TestParseTypedMessage(t *testing.T) {
	// Verify ParseTypedMessage handles typed messages correctly.
	// Rust-side contract guarantees every message has a "type" field.
	// Malformed JSON returns false; caller treats as fatal error.
	tests := []struct {
		name       string
		line       []byte
		expectType string
		expectOk   bool
	}{
		{
			name:       "ready message",
			line:       []byte(`{"type":"ready","id":"tun-123","status":"ready"}`),
			expectType: "ready",
			expectOk:   true,
		},
		{
			name:       "error message",
			line:       []byte(`{"type":"error","message":"something failed"}`),
			expectType: "error",
			expectOk:   true,
		},
		{
			name:       "heartbeat message without message field",
			line:       []byte(`{"type":"heartbeat"}`),
			expectType: "heartbeat",
			expectOk:   true,
		},
		{
			name:       "malformed JSON",
			line:       []byte(`{invalid json}`),
			expectType: "",
			expectOk:   false,
		},
		{
			name:       "empty line",
			line:       []byte(``),
			expectType: "",
			expectOk:   false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			msg, ok := ParseTypedMessage(tt.line)
			if ok != tt.expectOk {
				t.Errorf("ParseTypedMessage(%q) ok=%v, want %v", tt.line, ok, tt.expectOk)
			}
			if tt.expectOk && msg.Type != tt.expectType {
				t.Errorf("ParseTypedMessage(%q) type=%q, want %q", tt.line, msg.Type, tt.expectType)
			}
			// Verify no panic on messages without "message" field
			if tt.expectOk && tt.name == "heartbeat message without message field" {
				if msg.Message != "" {
					t.Errorf("expected empty message for heartbeat, got %q", msg.Message)
				}
			}
		})
	}
}
