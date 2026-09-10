// Package daemon manages the datum-connect-daemon background process — the
// skunkworks local HTTP API that lets datumctl, and eventually an MCP
// server and the desktop UI, create/start/stop tunnels through one shared
// service instead of each reimplementing tunnel logic. See
// D:\code\datum-desktop-tunnels\API-PLAN.md.
//
// This is distinct from the per-tunnel supervisor in internal/daemon: that
// one daemonizes a Go process which in turn spawns and streams JSON from a
// short-lived Rust `datum-connect listen` subprocess. Here there is no Go
// supervisor — datum-connect-daemon serves HTTP directly, so we daemonize
// it and track its PID alone.
package daemon

import (
	"fmt"
	"path/filepath"
	"strconv"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/binary"
	procdaemon "go.datum.net/datumctl-plugins/connect/internal/daemon"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	"go.datum.net/datumctl-plugins/connect/internal/pidfile"
	"go.datum.net/datumctl-plugins/connect/internal/state"
	"go.datum.net/datumctl/plugin"
)

const (
	defaultPort = 47780
	// Matches `tunnel stop`'s grace period (internal/pidfile.TerminateGracefully
	// is shared by both) — kept as one constant per caller so the two can't
	// silently drift apart again the way they did before this review.
	daemonStopGrace = 30 * time.Second
)

// NewCmd returns the `tunnel daemon` command group.
func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "daemon",
		Short: "Manage the local tunnel API daemon (proof-of-concept)",
	}
	cmd.AddCommand(newStartCmd())
	cmd.AddCommand(newStopCmd())
	cmd.AddCommand(newStatusCmd())
	return cmd
}

func pidFilePath() string {
	return filepath.Join(state.Dir(), "daemon.pid")
}

func newStartCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "start",
		Short: "Start the tunnel API daemon in the background",
		RunE:  runStart,
	}
	cmd.Flags().Int("port", defaultPort, "Port for the local HTTP API (loopback only)")
	return cmd
}

func runStart(cmd *cobra.Command, args []string) error {
	pidPath := pidFilePath()
	if pf, err := pidfile.Read(pidPath); err == nil && pidfile.PIDAlive(pf.RustPID) {
		fmt.Fprintf(cmd.OutOrStdout(), "Daemon already running (pid %d)\n", pf.RustPID)
		return nil
	}

	binPath, err := binary.DiscoverDaemon()
	if err != nil {
		return fmt.Errorf("locate datum-connect-daemon binary: %w", err)
	}

	pluginCtx := plugin.Context()
	childEnv := env.Build(pluginCtx)
	// NewRootCmd registers --project with ctx.Project as its default, but
	// that default is fixed at process-startup time — an explicit
	// `--project X` on this invocation lands in the flag, not in the
	// environment plugin.Context() re-reads. Read the flag directly so an
	// override actually takes effect instead of being silently ignored.
	project, _ := cmd.Flags().GetString("project")
	if project == "" {
		project = pluginCtx.Project
	}
	// Override, not append — see env.Build's doc comment: os.Environ()
	// (the base childEnv is built from) already carries datumctl's own
	// DATUM_PROJECT, and a plain append() would lose to it under
	// first-occurrence-wins duplicate-key lookup.
	childEnv = env.WithOverrides(childEnv, map[string]string{"DATUM_PROJECT": project})

	port, _ := cmd.Flags().GetInt("port")
	daemonArgs := []string{binPath, "--port", strconv.Itoa(port)}

	pid, err := procdaemon.DaemonizeWithEnv(binPath, daemonArgs, childEnv)
	if err != nil {
		return fmt.Errorf("start daemon: %w", err)
	}

	// Reuse the pidfile package's two-PID format even though there's no
	// separate Go supervisor for this process: GoPID is not applicable
	// (set to 0), RustPID holds the daemon's own PID.
	if err := pidfile.Write(pidPath, 0, pid, time.Now(), binPath); err != nil {
		return fmt.Errorf("write pid file: %w", err)
	}

	fmt.Fprintf(cmd.OutOrStdout(), "Daemon started (pid %d, port %d)\n", pid, port)
	return nil
}

func newStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "stop",
		Short: "Stop the tunnel API daemon",
		RunE:  runStop,
	}
}

func runStop(cmd *cobra.Command, args []string) error {
	pidPath := pidFilePath()
	pf, err := pidfile.Read(pidPath)
	if err != nil {
		fmt.Fprintln(cmd.OutOrStdout(), "Daemon not running")
		return nil
	}
	if !pidfile.PIDAlive(pf.RustPID) {
		_ = pidfile.Remove(pidPath)
		fmt.Fprintln(cmd.OutOrStdout(), "Daemon not running (stale pid file removed)")
		return nil
	}
	if err := pidfile.TerminateGracefully(pf.RustPID, daemonStopGrace); err != nil {
		return fmt.Errorf("stop daemon (pid %d): %w", pf.RustPID, err)
	}
	_ = pidfile.Remove(pidPath)
	fmt.Fprintf(cmd.OutOrStdout(), "Daemon stopped (pid %d)\n", pf.RustPID)
	return nil
}

func newStatusCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Show tunnel API daemon status",
		RunE:  runStatus,
	}
}

func runStatus(cmd *cobra.Command, args []string) error {
	pf, err := pidfile.Read(pidFilePath())
	if err != nil {
		fmt.Fprintln(cmd.OutOrStdout(), "Daemon: not running")
		return nil
	}
	alive := pidfile.PIDAlive(pf.RustPID)
	status := "Stopped"
	if alive {
		status = "Running"
	}
	fmt.Fprintf(cmd.OutOrStdout(), "Daemon:  %s\n", status)
	fmt.Fprintf(cmd.OutOrStdout(), "PID:     %d (alive: %v)\n", pf.RustPID, alive)
	fmt.Fprintf(cmd.OutOrStdout(), "Started: %s\n", pf.StartTime.Format(time.RFC3339))
	fmt.Fprintf(cmd.OutOrStdout(), "Binary:  %s\n", pf.BinaryPath)
	return nil
}
