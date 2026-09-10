package uninstall

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
	"go.datum.net/datumctl-plugins/connect/internal/svcunit"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "uninstall --name N",
		Short: "Uninstall a tunnel service",
		RunE:  runUninstall,
	}
	cmd.Flags().String("name", "", "Tunnel name (required)")
	return cmd
}

func runUninstall(cmd *cobra.Command, args []string) error {
	name, _ := cmd.Flags().GetString("name")
	if name == "" {
		fmt.Fprintln(os.Stderr, "Error: --name is required")
		os.Exit(64)
	}

	// Check config exists
	exists, err := svcconfig.Exists(name)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: check config: %v\n", err)
		os.Exit(1)
	}
	if !exists {
		fmt.Fprintf(os.Stderr, "Error: tunnel '%s' is not installed\n", name)
		os.Exit(64)
	}

	// Find binary path
	binPath, err := resolveBinaryPath()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: resolve binary: %v\n", err)
		os.Exit(1)
	}

	// Stop and uninstall systemd unit
	if err := svcunit.Uninstall(name, binPath); err != nil {
		fmt.Fprintf(os.Stderr, "Error: uninstall service: %v\n", err)
		os.Exit(1)
	}

	// Delete config
	if err := svcconfig.Remove(name); err != nil {
		fmt.Fprintf(os.Stderr, "Error: remove config: %v\n", err)
		os.Exit(1)
	}

	// Phase 11.5 D-13: remove the per-service state subdirectory
	// (matches the path baked into the unit file by svcunit.buildConfig).
	// Best-effort: report errors but do not fail uninstall — the systemd
	// unit and svcconfig entry are already gone, refusing to consider the
	// tunnel uninstalled because of a directory-permission issue would
	// be surprising.
	if home, err := os.UserHomeDir(); err == nil {
		stateDir := filepath.Join(home, ".datumctl", "connect", "services", name)
		if err := os.RemoveAll(stateDir); err != nil {
			fmt.Fprintf(os.Stderr, "Warning: could not remove %s: %v\n", stateDir, err)
		}
	} else {
		fmt.Fprintf(os.Stderr, "Warning: could not compute service state dir for cleanup: %v\n", err)
	}

	fmt.Fprintf(cmd.OutOrStdout(), "Tunnel '%s' uninstalled\n", name)
	return nil
}

func resolveBinaryPath() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return exec.LookPath("datumctl-connect")
	}
	return path, nil
}
