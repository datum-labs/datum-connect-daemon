package stop

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/pidfile"
	"go.datum.net/datumctl-plugins/connect/internal/state"
	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
	"go.datum.net/datumctl-plugins/connect/internal/svcunit"
)

const (
	gracePeriod = 30 * time.Second
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "stop --name N",
		Short: "Stop a tunnel",
		RunE:  runStop,
	}
	cmd.Flags().String("name", "", "Tunnel name (required)")
	return cmd
}

func runStop(cmd *cobra.Command, args []string) error {
	name, _ := cmd.Flags().GetString("name")
	if name == "" {
		fmt.Fprintln(os.Stderr, "Error: --name is required")
		os.Exit(64)
	}

	// Phase 6: Check if this is an installed service (no running daemon)
	pidPath := state.PidFilePath(name)
	if !pidfile.Exists(pidPath) {
		// No running daemon — try service stop
		installed, _ := svcconfig.Exists(name)
		if installed {
			binPath, err := resolveBinaryPath()
			if err != nil {
				return fmt.Errorf("resolve binary: %w", err)
			}
			if err := svcunit.Stop(name, binPath); err != nil {
				return fmt.Errorf("stop service: %w", err)
			}
			fmt.Fprintf(cmd.OutOrStdout(), "Tunnel '%s' service stopped\n", name)
			return nil
		}
		return fmt.Errorf("tunnel '%s' not running and not installed", name)
	}

	pf, err := pidfile.Read(pidPath)
	if err != nil {
		return fmt.Errorf("tunnel '%s' not running: %w", name, err)
	}

	// Kill Rust child first (per CONTEXT.md stop flow)
	rustProc, err := os.FindProcess(pf.RustPID)
	if err == nil {
		// Send SIGTERM to Rust
		_ = rustProc.Signal(syscall.SIGTERM)
	}

	// Wait up to grace period for Rust to exit
	done := make(chan struct{})
	go func() {
		for i := 0; i < int(gracePeriod/time.Second); i++ {
			if !pidfile.PIDAlive(pf.RustPID) {
				close(done)
				return
			}
			time.Sleep(time.Second)
		}
		// Timeout — force kill Rust
		if rustProc != nil {
			_ = rustProc.Signal(syscall.SIGKILL)
		}
		close(done)
	}()
	<-done

	// Clean up PID file
	_ = pidfile.Remove(pidPath)

	fmt.Fprintf(cmd.OutOrStdout(), "Tunnel '%s' stopped\n", name)
	return nil
}

func resolveBinaryPath() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return exec.LookPath("datumctl-connect")
	}
	return path, nil
}
