package ps

import (
	"encoding/json"
	"fmt"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/pidfile"
	"go.datum.net/datumctl-plugins/connect/internal/state"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "ps [--prune] [--json]",
		Short: "List running tunnels",
		RunE:  runPS,
	}
	cmd.Flags().Bool("prune", false, "Remove stale PID files")
	cmd.Flags().Bool("json", false, "Output JSON format")
	return cmd
}

func runPS(cmd *cobra.Command, args []string) error {
	prune, _ := cmd.Flags().GetBool("prune")
	jsonOut, _ := cmd.Flags().GetBool("json")

	tunnels, err := pidfile.ListRunningTunnels(state.Dir())
	if err != nil {
		return fmt.Errorf("list tunnels: %w", err)
	}

	// Prune stale entries if requested
	if prune {
		var remaining []pidfile.RunningTunnel
		for _, t := range tunnels {
			if t.Status == "Zombie" {
				path := state.PidFilePath(t.Name)
				_ = pidfile.Remove(path)
			} else {
				remaining = append(remaining, t)
			}
		}
		tunnels = remaining
	}

	if jsonOut {
		return outputJSON(cmd, tunnels)
	}
	return outputTable(cmd, tunnels)
}

func outputTable(cmd *cobra.Command, tunnels []pidfile.RunningTunnel) error {
	w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 3, ' ', 0)
	fmt.Fprintln(w, "NAME\tPID\tRUST\tSTATUS\tUPTIME\tENDPOINT")
	fmt.Fprintln(w, "----\t---\t----\t------\t------\t--------")

	if len(tunnels) == 0 {
		fmt.Fprintln(w, "(no running tunnels)")
		w.Flush()
		return nil
	}

	for _, t := range tunnels {
		uptime := formatUptime(t.StartTime)
		endpoint := t.BinaryPath
		if endpoint == "" {
			endpoint = "\u2014"
		}
		fmt.Fprintf(w, "%s\t%d\t%d\t%s\t%s\t%s\n",
			t.Name, t.GoPID, t.RustPID, t.Status, uptime, endpoint)
	}
	return w.Flush()
}

func outputJSON(cmd *cobra.Command, tunnels []pidfile.RunningTunnel) error {
	data, err := json.MarshalIndent(tunnels, "", "  ")
	if err != nil {
		return fmt.Errorf("json marshal: %w", err)
	}
	fmt.Fprintln(cmd.OutOrStdout(), string(data))
	return nil
}

func formatUptime(startTime time.Time) string {
	if startTime.IsZero() {
		return "\u2014"
	}
	d := time.Since(startTime).Round(time.Second)
	if d < time.Minute {
		return fmt.Sprintf("%ds", int(d.Seconds()))
	}
	if d < time.Hour {
		return fmt.Sprintf("%dm %ds", int(d.Minutes()), int(d.Seconds())%60)
	}
	return fmt.Sprintf("%dh %dm", int(d.Hours()), int(d.Minutes())%60)
}
