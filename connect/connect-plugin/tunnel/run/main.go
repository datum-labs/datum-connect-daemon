package run

import (
	"context"
	"fmt"
	"os"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/daemon"
	"go.datum.net/datumctl-plugins/connect/internal/state"
	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "run",
		Short: "(internal) Run tunnel supervisor",
		Long: `Start the tunnel supervisor process. This is the internal entry point
used by the daemon background process (--detach). It is also called by
systemd/launchd service units in Phase 6.`,
		RunE: runRun,
	}
	cmd.Flags().String("name", "", "Tunnel name (required)")
	cmd.Flags().String("project", "", "Project ID (checked against persisted config)")
	return cmd
}

func runRun(cmd *cobra.Command, args []string) error {
	// Server-of-truth (Phase 13 D-04, resolution table Item #11):
	// The Rust binary resolves the tunnel's label and endpoint from the
	// server (HTTPProxy resource) via get_active_by_endpoint. The values
	// passed through from the YAML snapshot are startup hints only — the
	// binary overrides them with the server's live state.

	name, _ := cmd.Flags().GetString("name")
	projectFlag, _ := cmd.Flags().GetString("project")

	if name == "" {
		fmt.Fprintln(os.Stderr, "Error: --name is required")
		os.Exit(64)
	}

	// Load persisted config
	cfgPath := svcconfig.ConfigFilePath(name)
	svcCfg, err := svcconfig.Load(cfgPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: load config for '%s': %v\n", name, err)
		os.Exit(1)
	}

	// Check --project mismatch (install-time project is authoritative for services)
	if projectFlag != "" && projectFlag != svcCfg.Project {
		fmt.Fprintf(os.Stderr, "Error: --project '%s' does not match installed project '%s'. Reinstall to change project.\n", projectFlag, svcCfg.Project)
		os.Exit(64)
	}

	// Set session in env for the child Rust binary
	if svcCfg.Session != "" {
		os.Setenv("DATUM_SESSION", svcCfg.Session)
	}

	logFile := state.LogFilePath(name)

	cfg := daemon.Config{
		Name:     name,
		Label:    svcCfg.Label,
		Endpoint: svcCfg.Endpoint,
		LogFile:  logFile,
	}

	ctx := context.Background()
	if err := daemon.RunSupervisor(ctx, cfg); err != nil {
		fmt.Fprintf(os.Stderr, "supervisor: %v\n", err)
		os.Exit(1)
	}
	return nil
}


