// Package exec provides shared subprocess orchestration for fire-and-forget CRUD commands.
package exec

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/output"
	"go.datum.net/datumctl-plugins/connect/internal/signals"
)

// OutputMode controls how subprocess output is handled.
type OutputMode int

const (
	// OutputModeTable: Rust outputs JSON, Go converts to human-readable table.
	OutputModeTable OutputMode = iota
	// OutputModeJSON: Rust outputs JSON, Go passes through verbatim.
	OutputModeJSON
	// OutputModeYAML: Rust outputs JSON, Go converts to YAML.
	OutputModeYAML
)

// TypedMessage represents a typed JSON message from the Rust binary.
//
// Rust-side contract (enforced by Rust code):
// Every message emitted to stdout is a single-line JSON object with a "type" field.
// No message is emitted without a "type" field. No malformed JSON is emitted.
//
//	{"type":"ready","id":"...","label":"...","endpoint":"...","hostnames":["..."],"status":"ready"}
//	{"type":"error","message":"..."}
//	{"type":"heartbeat"}
//	{"type":"status","state":"..."}
//
// Go-side parse policy:
//	- Valid JSON with "type" → dispatch on type
//	- Valid JSON without "type" → fatal error (Rust contract requires "type" on every message)
//	- Invalid JSON → fatal error (should never occur from Rust)
//	- Empty line → skip silently
type TypedMessage struct {
	Type    string                 `json:"type"`
	Message string                 `json:"message,omitempty"`
	Fields  map[string]interface{} `json:",inline"`
}

// RunResult holds the captured output and exit status from a subprocess run.
type RunResult struct {
	Stdout   []byte
	Stderr   []byte
	ExitCode int
}

// Run executes the datum-connect binary with the given arguments and environment,
// captures its output, forwards signals, and returns the result.
//
// The function:
// 1. Creates the command with the given args and env
// 2. Captures stdout and stderr into buffers
// 3. Starts the command
// 4. Sets up signal forwarding (SIGINT/SIGTERM) with grace period
// 5. Waits for completion
// 6. Returns the captured result
//
// stderr handling: child stderr is captured into RunResult.Stderr.
// The caller (RunWithOutput) decides whether to surface it.
// This is the ONLY path data goes to stderr — no progress/status messages.
//
// Exit code mapping:
//	- Child exits normally: RunResult.ExitCode = child's exit code, returned nil error
//	- Child exits via signal: RunResult.ExitCode = 128 + signal number, returned nil error
//	- Child not found: returned error (not a RunResult)
//	- Go-side setup failure: returned error
//
// IMPORTANT: This function is for fire-and-forget commands only (list, update, delete).
// The listen command manages process lifecycle directly to handle streaming output.
func Run(ctx context.Context, binaryPath string, args []string, env []string, outputMode OutputMode) (*RunResult, error) {
	cmd := exec.CommandContext(ctx, binaryPath, args...)
	cmd.Env = env
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("failed to start %s: %w", binaryPath, err)
	}

	// Start signal forwarding in a goroutine
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		if err := signals.Forward(cmd.Process, 30*time.Second); err != nil {
			// Signal forwarding failure is non-fatal; child may have already exited
		}
	}()

	// Wait for completion
	cmd.Wait()

	// Wait for signal goroutine to finish
	wg.Wait()

	exitCode := 0
	if cmd.ProcessState != nil {
		exitCode = cmd.ProcessState.ExitCode()
	}

	result := &RunResult{
		Stdout:   stdout.Bytes(),
		Stderr:   stderr.Bytes(),
		ExitCode: exitCode,
	}

	// Format output based on mode
	if outputMode == OutputModeYAML && len(result.Stdout) > 0 {
		yaml, err := output.ConvertJSONToYAML(result.Stdout)
		if err == nil {
			result.Stdout = yaml
		}
	}

	return result, nil
}

// RunWithOutput is a convenience wrapper that writes formatted output to a
// cmd.OutOrStdout() and exits with the child's exit code on failure.
//
// Exit code policy:
//	- Child exits non-zero: os.Exit(child_exit_code) — EXIT-01: propagate verbatim
//	- Go-side error: returns error (caller decides exit code)
//	- Child not found: os.Exit(1)
func RunWithOutput(ctx context.Context, cmd *cobra.Command, binaryPath string, args []string, env []string, outputMode OutputMode) error {
	result, err := Run(ctx, binaryPath, args, env, outputMode)
	if err != nil {
		return err
	}
	if result.ExitCode != 0 {
		// Print stderr for debugging (child error output)
		if len(result.Stderr) > 0 {
			fmt.Fprintln(cmd.ErrOrStderr(), strings.TrimSpace(string(result.Stderr)))
		}
		// Exit with child's exit code (EXIT-01)
		os.Exit(result.ExitCode)
	}
	// Write stdout to cmd output
	if len(result.Stdout) > 0 {
		fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
	}
	return nil
}

// ParseTypedMessage parses a JSON line from the Rust binary into a TypedMessage.
// Returns (TypedMessage, true) if the line is valid JSON with a "type" field.
// Returns (TypedMessage{}, false) for invalid JSON, missing "type", or empty lines.
//
// Parse/error policy:
//	- Valid JSON with "type" field → parse and return (dispatch on type)
//	- Valid JSON without "type" field → fatal error (Rust contract requires "type" on every message)
//	- Invalid JSON → fatal error (should never occur from Rust)
//	- Empty line → skip silently (trailing newline, whitespace)
//
// This function is safe to call on every line from child stdout.
func ParseTypedMessage(line []byte) (TypedMessage, bool) {
	var msg map[string]interface{}
	if err := json.Unmarshal(line, &msg); err != nil {
		// Invalid JSON — caller treats as fatal error (should never occur from Rust)
		return TypedMessage{}, false
	}

	typeField, hasType := msg["type"]
	if !hasType {
		// Fatal: Rust contract requires "type" on every message
		return TypedMessage{}, false
	}

	typeStr, _ := typeField.(string)
	var message string
	if msgData, ok := msg["message"]; ok {
		message, _ = msgData.(string)
	}
	return TypedMessage{
		Type:    typeStr,
		Message: message,
		Fields:  msg,
	}, true
}
