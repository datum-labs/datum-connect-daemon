package listen

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/binary"
	"go.datum.net/datumctl-plugins/connect/internal/daemon"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	rexec "go.datum.net/datumctl-plugins/connect/internal/exec"
	"go.datum.net/datumctl/plugin"
)

const (
	// startupTimeout is the maximum time to wait for the first typed message
	// (ready or error) from the Rust binary.
	startupTimeout = 10 * time.Minute
	// gracePeriod is the time to wait for clean shutdown after sending SIGINT.
	gracePeriod = 30 * time.Second
)

// TunnelReady represents the ready message from the Rust binary.
type TunnelReady struct {
	ID        string   `json:"id"`
	Label     string   `json:"label"`
	Endpoint  string   `json:"endpoint"`
	Hostnames []string `json:"hostnames"`
	Status    string   `json:"status"`
}

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:          "listen [flags]",
		Short:        "Start a tunnel and block",
		SilenceUsage: true,
		RunE:         runListen,
	}
	cmd.Flags().String("label", "", "Display name for the tunnel")
	cmd.Flags().String("endpoint", "", "Local address to expose (host:port, required)")
	cmd.Flags().String("id", "", "Existing tunnel resource name to resume (mutually inclusive with optional --endpoint)")
	cmd.Flags().Bool("yes", false, "Skip confirmation prompt")
	cmd.Flags().Bool("detach", false, "Run in background (daemon mode)")
	cmd.Flags().String("name", "", "Tunnel name (required with --detach)")
	cmd.Flags().String("log-file", "", "Path for Rust debug log output")
	cmd.Flags().StringP("output", "o", "table", "Output format: table, json, yaml")
	return cmd
}

func runListen(cmd *cobra.Command, args []string) error {
	label, _ := cmd.Flags().GetString("label")
	endpoint, _ := cmd.Flags().GetString("endpoint")
	id, _ := cmd.Flags().GetString("id")
	yes, _ := cmd.Flags().GetBool("yes")
	detach, _ := cmd.Flags().GetBool("detach")
	name, _ := cmd.Flags().GetString("name")
	logFile, _ := cmd.Flags().GetString("log-file")

	if endpoint == "" && id == "" {
		// Neither flag given — semantic rejection (EXIT-02).
		// The Rust binary requires at least one of --endpoint or --id;
		// when neither is set and stdin is non-interactive the picker
		// also can't run, so reject here for a faster, clearer error.
		fmt.Fprintln(os.Stderr, "Error: --endpoint or --id is required")
		os.Exit(64) // POSIX: semantic rejection (EXIT-02)
	}

	// Detach mode: spawn background daemon and exit
	if detach {
		if id != "" {
			fmt.Fprintln(os.Stderr, "Error: --id is not supported with --detach. Use 'tunnel run --name N' for detached named tunnels")
			os.Exit(64)
		}
		if name == "" {
			fmt.Fprintln(os.Stderr, "Error: --name is required with --detach")
			os.Exit(64)
		}
		exe := daemon.SelfExe()
		childArgs := daemon.ForegroundArgs(name, logFile, endpoint, label, yes)
		_, err := daemon.Daemonize(exe, append([]string{exe}, childArgs...))
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error: daemonize: %v\n", err)
			os.Exit(1)
		}
		fmt.Fprintf(cmd.OutOrStdout(), "Tunnel '%s' setting up in background; tunnel status will show progress\n", name)
		return nil
	}

	// Discover binary
	binaryPath, err := binary.Discover()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	// Get plugin context
	pluginCtx := plugin.Context()

	// Build environment (no DATUM_ACCESS_TOKEN — binary obtains token via credentials helper)
	childEnv := env.Build(pluginCtx)

	// Pass tunnel name to the Rust binary so it can construct the
	// per-tunnel key path. Only set when the name is known upfront
	// (detach mode). For --endpoint-only and picker paths, the name
	// comes from the server after tunnel creation (handled in Rust).
	if name != "" {
		childEnv = append(childEnv, "DATUM_CONNECT_TUNNEL_NAME="+name)
	}

	// Build args
	rustArgs := []string{"--json", "--project", pluginCtx.Project, "listen"}
	if endpoint != "" {
		rustArgs = append(rustArgs, "--endpoint", endpoint)
	}
	if id != "" {
		rustArgs = append(rustArgs, "--id", id)
	}
	if label != "" {
		rustArgs = append(rustArgs, "--label", label)
	}
	if yes {
		rustArgs = append(rustArgs, "--yes")
	}

	// Create and start the command
	rustCmd := exec.CommandContext(context.Background(), binaryPath, rustArgs...)
	rustCmd.Env = childEnv

	// Capture stdout for JSON parsing
	stdoutReader, err := rustCmd.StdoutPipe()
	if err != nil {
		return fmt.Errorf("failed to create stdout pipe: %w", err)
	}
	// stderr forwarded transparently to plugin stderr
	rustCmd.Stderr = os.Stderr

	if err := rustCmd.Start(); err != nil {
		return fmt.Errorf("failed to start datum-connect: %w", err)
	}

	// Determine mode
	isJSON := false
	if outputFlag, _ := cmd.Flags().GetString("output"); outputFlag == "json" {
		isJSON = true
	}

	// Read and parse output line by line with startup timeout
	scanner := bufio.NewScanner(stdoutReader)
	var ready TunnelReady
	var gotReady bool

	// Read lines — signals ready via readyCh
	readDone := make(chan struct{})
	readyCh := make(chan struct{})
	go func() {
		for scanner.Scan() {
			line := scanner.Bytes()
			if len(line) == 0 {
				continue
			}
			msg, ok := rexec.ParseTypedMessage(line)
			if !ok {
				// Invalid JSON or missing "type" — fatal error
				rustCmd.Wait()
				fmt.Fprintf(os.Stderr, "malformed message from child: %s\n", line)
				return
			}

			switch msg.Type {
			case "tunnel_ready":
				readyData, _ := json.Marshal(msg.Fields)
				json.Unmarshal(readyData, &ready)
				gotReady = true

				if isJSON {
					// JSON mode: print ready JSON and stop reading.
					// Add newline — the bufio.Scanner stripped it, and
					// pipe-buffered stdout won't flush without one.
					fmt.Fprintln(cmd.OutOrStdout(), string(line))
					close(readyCh)
					return
				}
				// Interactive mode: print hostname
				if len(ready.Hostnames) > 0 {
					fmt.Fprintf(cmd.OutOrStdout(), "Tunnel ready: https://%s\n", ready.Hostnames[0])
				}
				fmt.Fprintln(cmd.OutOrStdout(), "Press Ctrl+C to stop...")
				close(readyCh)
			case "error":
				if msg.Message != "" {
					fmt.Fprintf(os.Stderr, "error: %s\n", msg.Message)
				}
			case "heartbeat", "status":
				// Internal messages — no output
			case "tunnel_progress", "tunnel_verifying", "tunnel_verified":
				// Per-step setup-time status events from the Rust binary's
				// await_tunnel_progress / verify_endpoints (Phase 12-03).
				// Currently no-op at the supervisor layer — the human-friendly
				// ready line is what we surface. Phase 13 may forward these
				// to a future progress UI.
				_ = msg
			case "tunnel_terminal_failure", "tunnel_login_lost", "tunnel_deleted_upstream":
				// Mid-session degradation signals from the Rust binary's runtime
				// poll loop (Phase 12-04). Forward the message field to stderr so
				// the user sees it; the child will exit on its own shortly.
				if msg.Message != "" {
					fmt.Fprintln(os.Stderr, msg.Message)
				}
			case "tunnel_disabled":
				// Emitted by the Rust binary's cleanup block (Phase 12-04).
				// No-op at supervisor layer; the child is about to exit.
				_ = msg
			case "tunnel_created", "tunnel_updated":
				// Lifecycle events from create/update paths. No supervisor
				// action needed in plugin/listen mode; tunnel_ready still
				// drives gotReady.
				_ = msg
			case "tunnel_deleted":
				// Emitted only by the `delete` subcommand. Not seen on the
				// listen path.
				_ = msg
			default:
				// Unknown type — skip
			}
		}
		close(readDone)
	}()

	// Wait for ready message or timeout
	select {
	case <-readyCh:
		// Ready message received
	case <-time.After(startupTimeout):
		_ = rustCmd.Process.Signal(syscall.SIGKILL)
		rustCmd.Wait()
		return fmt.Errorf("timed out waiting for tunnel ready after %v", startupTimeout)
	case <-readDone:
		// Scanner ended — child exited without sending ready message
		return fmt.Errorf("child exited before sending ready message")
	}

	if !gotReady {
		return fmt.Errorf("no ready message received from child")
	}

	// Block until signal (Ctrl+C / SIGINT / SIGTERM)
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	sig := <-sigCh
	// Forward signal to child
	_ = rustCmd.Process.Signal(sig)

	// Wait for child with grace period
	done := make(chan error, 1)
	go func() {
		done <- rustCmd.Wait()
	}()

	select {
	case <-done:
		// Child exited after signal — intentional shutdown, not an error.
		return nil
	case <-time.After(gracePeriod):
		// Grace period expired — force kill
		_ = rustCmd.Process.Signal(syscall.SIGKILL)
		<-done
		return nil
	}
}
