// Registered log-tail sources for the dashboard — see LOG-TAIL-PLAN.md and
// daemon/src/logs.rs (the implementation these commands call into).
// Registering a source is setup-only (it decides what becomes readable at
// all, same as tunnel create/peer advertise); listing and tailing are
// setup-or-viewer, same tier as tunnel traffic.
package api

import (
	"fmt"
	"net/http"

	"github.com/spf13/cobra"
)

func newLogCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "log",
		Short: "Manage log sources the dashboard can tail",
	}
	cmd.AddCommand(newLogAddCmd())
	cmd.AddCommand(newLogListCmd())
	cmd.AddCommand(newLogRemoveCmd())
	cmd.AddCommand(newLogTailCmd())
	return cmd
}

func newLogAddCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "add <name> <path>",
		Short: "Register a file as a tailable log source (does not require the file to exist yet)",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodPost, "/v1/logs", map[string]string{
				"name": args[0],
				"path": args[1],
			})
		},
	}
}

func newLogListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List registered log sources",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/logs", nil)
		},
	}
}

func newLogRemoveCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "remove <name>",
		Short: "Unregister a log source (never deletes the underlying file)",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/logs/"+args[0], nil)
		},
	}
}

func newLogTailCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "tail <name>",
		Short: "Show the last N lines of a registered log source",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			lines, _ := cmd.Flags().GetInt("lines")
			if lines <= 0 {
				return fmt.Errorf("--lines must be a positive number")
			}
			return call(cmd, http.MethodGet, fmt.Sprintf("/v1/logs/%s/tail?lines=%d", args[0], lines), nil)
		},
	}
	// Default/cap is operator-configurable (--log-tail-max-lines on the
	// daemon, default 1000) — 100 here is just this command's own default
	// ask, independent of the server's ceiling.
	cmd.Flags().Int("lines", 100, "Number of lines to show (server caps this, default 1000, operator-configurable)")
	return cmd
}
