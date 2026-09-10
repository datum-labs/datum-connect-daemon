package delete

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/binary"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	"go.datum.net/datumctl-plugins/connect/internal/exec"
	"go.datum.net/datumctl-plugins/connect/internal/output"
	"go.datum.net/datumctl/plugin"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "delete [flags]",
		Short: "Delete a tunnel",
		RunE:  runDelete,
	}
	cmd.Flags().String("id", "", "Tunnel ID to delete (required)")
	cmd.Flags().StringP("output", "o", "table", "Output format: table, json, yaml")
	return cmd
}

func runDelete(cmd *cobra.Command, args []string) error {
	id, _ := cmd.Flags().GetString("id")

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

	// Run: --json delete --id X
	result, err := exec.Run(context.Background(), binaryPath, []string{"--json", "delete", "--id", id}, childEnv, exec.OutputModeJSON)
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
		// Table mode: render human-readable output for delete
		var deleteResult map[string]interface{}
		if err := json.Unmarshal(result.Stdout, &deleteResult); err == nil {
			id, _ := deleteResult["id"].(string)
			fmt.Fprintf(cmd.OutOrStdout(), "Deleted tunnel %s\n", id)
			if resources, ok := deleteResult["resources"].([]interface{}); ok {
				for _, r := range resources {
					if res, ok := r.(map[string]interface{}); ok {
						typ, _ := res["type"].(string)
						name, _ := res["name"].(string)
						fmt.Fprintf(cmd.OutOrStdout(), "  %s %s\n", typ, name)
					}
				}
			}
		} else {
			fmt.Fprint(cmd.OutOrStdout(), string(result.Stdout))
		}
	}

	return nil
}
