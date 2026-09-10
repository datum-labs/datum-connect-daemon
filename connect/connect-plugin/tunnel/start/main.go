package start

import (
	"fmt"
	"os"
	"os/exec"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
	"go.datum.net/datumctl-plugins/connect/internal/svcunit"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "start --name N [--project P]",
		Short: "Start a tunnel service",
		RunE:  runStart,
	}
	cmd.Flags().String("name", "", "Tunnel name (required)")
	cmd.Flags().String("project", "", "Project ID (checked against persisted config)")
	return cmd
}

func runStart(cmd *cobra.Command, args []string) error {
	name, _ := cmd.Flags().GetString("name")
	if name == "" {
		fmt.Fprintln(os.Stderr, "Error: --name is required")
		os.Exit(64)
	}

	// Check --project mismatch (install-time project is authoritative for services)
	if projectFlag, _ := cmd.Flags().GetString("project"); projectFlag != "" {
		cfgPath := svcconfig.ConfigFilePath(name)
		cfg, err := svcconfig.Load(cfgPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error: load config for '%s': %v\n", name, err)
			os.Exit(1)
		}
		if projectFlag != cfg.Project {
			fmt.Fprintf(os.Stderr, "Error: --project '%s' does not match installed project '%s'. Reinstall to change project.\n", projectFlag, cfg.Project)
			os.Exit(64)
		}
	}

	// Check installed
	exists, err := svcconfig.Exists(name)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: check config: %v\n", err)
		os.Exit(1)
	}
	if !exists {
		fmt.Fprintf(os.Stderr, "Error: tunnel '%s' is not installed\n", name)
		os.Exit(64)
	}

	binPath, err := resolveBinaryPath()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: resolve binary: %v\n", err)
		os.Exit(1)
	}

	if err := svcunit.Start(name, binPath); err != nil {
		fmt.Fprintf(os.Stderr, "Error: start service: %v\n", err)
		os.Exit(1)
	}

	fmt.Fprintf(cmd.OutOrStdout(), "Tunnel '%s' started\n", name)
	return nil
}

func resolveBinaryPath() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return exec.LookPath("datumctl-connect")
	}
	return path, nil
}
