package list

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
		Use:   "list",
		Short: "List active tunnels",
		RunE:  runList,
	}
	cmd.Flags().StringP("output", "o", "table", "Output format: table, json, yaml")
	return cmd
}

func runList(cmd *cobra.Command, args []string) error {
	// Discover the datum-connect binary
	binaryPath, err := binary.Discover()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	// Get plugin context
	pluginCtx := plugin.Context()

	// Build environment (no DATUM_ACCESS_TOKEN — binary obtains token via credentials helper)
	childEnv := env.Build(pluginCtx)

	// Determine output mode
	outputFlag, _ := cmd.Flags().GetString("output")
	var outputMode exec.OutputMode
	switch outputFlag {
	case "json":
		outputMode = exec.OutputModeJSON
	case "yaml":
		outputMode = exec.OutputModeYAML
	default:
		outputMode = exec.OutputModeTable
	}

	// Run the Rust binary
	result, err := exec.Run(context.Background(), binaryPath, []string{"--json", "list"}, childEnv, outputMode)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	// Exit code propagation (EXIT-01: propagate verbatim)
	if result.ExitCode != 0 {
		if len(result.Stderr) > 0 {
			fmt.Fprintln(os.Stderr, strings.TrimSpace(string(result.Stderr)))
		}
		os.Exit(result.ExitCode)
	}

	// Format and print output
	switch outputMode {
	case exec.OutputModeTable:
		w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
		if err := output.RenderTable(result.Stdout, w); err != nil {
			// Fallback to raw output if parsing fails
			fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
		}
	case exec.OutputModeJSON:
		fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
	case exec.OutputModeYAML:
		fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
	}

	return nil
}
