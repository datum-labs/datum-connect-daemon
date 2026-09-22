package api

import (
	"net/http"
	"strings"

	"github.com/spf13/cobra"
)

// newNoteCmd groups CLI-only note commands — a free-text note attached to a
// tunnel so it's still obvious what it's for once there are 10+ running.
// Deliberately CLI-only: the dashboard shows notes but never offers an input
// control for one, same "pure viewer" convention it follows everywhere else.
func newNoteCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "note",
		Short: "Attach a free-text note to a tunnel (CLI-only — shown read-only in the dashboard)",
	}
	cmd.AddCommand(newNoteSetCmd())
	cmd.AddCommand(newNoteClearCmd())
	return cmd
}

func newNoteSetCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "set <id> <text...>",
		Short: "Set (or replace) a tunnel's note",
		Args:  cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			note := strings.Join(args[1:], " ")
			return call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/note", map[string]string{"note": note})
		},
	}
}

func newNoteClearCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "clear <id>",
		Short: "Remove a tunnel's note",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/note", map[string]string{"note": ""})
		},
	}
}
