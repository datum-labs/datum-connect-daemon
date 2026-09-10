package update

import (
	"context"
	"fmt"
	"os"
	"strings"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/binary"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	"go.datum.net/datumctl-plugins/connect/internal/exec"
	"go.datum.net/datumctl-plugins/connect/internal/output"
	"go.datum.net/datumctl/plugin"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "update [flags]",
		Short: "Update a tunnel",
		RunE:  runUpdate,
	}
	cmd.Flags().String("id", "", "Tunnel ID to update (required)")
	cmd.Flags().String("label", "", "New display name")
	cmd.Flags().String("endpoint", "", "New local address (host:port)")
	cmd.Flags().StringP("output", "o", "table", "Output format: table, json, yaml")
	return cmd
}

func runUpdate(cmd *cobra.Command, args []string) error {
	// Server-of-truth (Phase 13 D-04, resolution table Item #11):
	// This function delegates to the Rust binary which mutates the server-side
	// HTTPProxy resource. It does NOT rewrite the local YAML config — the YAML
	// is an install-time snapshot only. Runtime values (label, endpoint) come
	// from the server.

	id, _ := cmd.Flags().GetString("id")
	label, _ := cmd.Flags().GetString("label")
	endpoint, _ := cmd.Flags().GetString("endpoint")

	if id == "" {
		fmt.Fprintln(os.Stderr, "Error: --id is required")
		os.Exit(64) // POSIX: semantic rejection (EXIT-02)
	}

	// Discover binary
	binaryPath, err := binary.Discover()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	// Get plugin context
	pluginCtx := plugin.Context()

	// Build env (no DATUM_ACCESS_TOKEN — binary obtains token via credentials helper)
	childEnv := env.Build(pluginCtx)

	// Build args: --json update --id X [--label Y] [--endpoint Z]
	rustArgs := []string{"--json", "update", "--id", id}
	if label != "" {
		rustArgs = append(rustArgs, "--label", label)
	}
	if endpoint != "" {
		rustArgs = append(rustArgs, "--endpoint", endpoint)
	}

	// Run
	result, err := exec.Run(context.Background(), binaryPath, rustArgs, childEnv, exec.OutputModeJSON)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	if result.ExitCode != 0 {
		if len(result.Stderr) > 0 {
			fmt.Fprintln(os.Stderr, strings.TrimSpace(string(result.Stderr)))
		}
		os.Exit(result.ExitCode)
	}

	// Output: JSON mode passes through, YAML converts, table renders
	outputFlag, _ := cmd.Flags().GetString("output")
	switch outputFlag {
	case "json":
		fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
	case "yaml":
		yaml, err := output.ConvertJSONToYAML(result.Stdout)
		if err != nil {
			fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
		} else {
			fmt.Fprint(cmd.OutOrStdout(), string(yaml))
		}
	default:
		// Table mode for single object — render as single-row table
		w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
		if err := output.RenderTable(result.Stdout, w); err != nil {
			fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
		}
	}

	return nil
}
