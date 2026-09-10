// Package daemon provides tunnel supervisor and daemonization primitives.
//
// The supervisor manages the lifecycle of the Rust tunnel binary, forwards
// its typed JSON output to stdout, and writes/removes PID files.
// The daemonize functions provide cross-platform background process spawning.
package daemon

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"go.datum.net/datumctl-plugins/connect/internal/binary"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	"go.datum.net/datumctl-plugins/connect/internal/pidfile"
	rexec "go.datum.net/datumctl-plugins/connect/internal/exec"
	"go.datum.net/datumctl-plugins/connect/internal/state"
	"go.datum.net/datumctl/plugin"
)

// Config holds the supervisor configuration.
type Config struct {
	Name     string
	Label    string
	Endpoint string
	LogFile  string // optional Rust debug log file path
	Yes      bool   // skip confirmation
}

// RunSupervisor starts the Rust tunnel binary and supervises its lifecycle.
// It writes a PID file at start, removes it on exit, and forwards Rust output
// to stdout. The function blocks until the Rust binary exits.
func RunSupervisor(ctx context.Context, cfg Config) error {
	// Discover binary
	binaryPath, err := binary.Discover()
	if err != nil {
		return fmt.Errorf("binary discovery: %w", err)
	}

	// Get plugin context
	pluginCtx := plugin.Context()

	// Build environment (no DATUM_ACCESS_TOKEN — binary obtains token via credentials helper)
	childEnv := env.Build(pluginCtx)

	// Build Rust args
	rustArgs := []string{"--json", "listen", "--endpoint", cfg.Endpoint}
	if cfg.Label != "" {
		rustArgs = append(rustArgs, "--label", cfg.Label)
	}
	if cfg.Yes {
		rustArgs = append(rustArgs, "--yes")
	}
	if cfg.LogFile != "" {
		rustArgs = append(rustArgs, "--log-file", cfg.LogFile)
	}

	// Start Rust binary
	rustCmd := exec.CommandContext(ctx, binaryPath, rustArgs...)
	rustCmd.Env = childEnv

	stdoutPipe, err := rustCmd.StdoutPipe()
	if err != nil {
		return fmt.Errorf("stdout pipe: %w", err)
	}
	rustCmd.Stderr = os.Stderr

	if err := rustCmd.Start(); err != nil {
		return fmt.Errorf("start datum-connect: %w", err)
	}

	// Write PID file (Go PID known, Rust PID just started)
	if cfg.Name != "" {
		pidPath := pidFilePath(cfg.Name)
		startTime := time.Now()
		if err := pidfile.Write(pidPath, os.Getpid(), rustCmd.Process.Pid, startTime, binaryPath); err != nil {
			// Non-fatal — supervisor continues
			fmt.Fprintf(os.Stderr, "warning: failed to write pid file: %v\n", err)
		}
		defer func() {
			_ = pidfile.Remove(pidPath)
		}()
	}

	// Read and forward typed JSON messages
	scanner := bufio.NewScanner(stdoutPipe)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		msg, ok := rexec.ParseTypedMessage(line)
		if !ok {
			continue
		}
		// Forward all messages to stdout (the parent/supervisor)
		fmt.Fprintln(os.Stdout, string(line))
		_ = msg // Use msg to suppress unused warning
	}

	// Wait for Rust to exit
	waitErr := rustCmd.Wait()
	return waitErr
}

// pidFilePath returns the PID file path for a named tunnel.
// Uses DATUM_CONNECT_TUNNEL_DIR env var if set (for testing isolation),
// otherwise falls back to the state package's tunnel directory.
func pidFilePath(name string) string {
	if d := os.Getenv("DATUM_CONNECT_TUNNEL_DIR"); d != "" {
		return filepath.Join(d, name+".pid")
	}
	return state.PidFilePath(name)
}
