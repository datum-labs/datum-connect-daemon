// Viewer token: a single global, read-only credential for the dashboard —
// see daemon/src/auth.rs's module doc comment for the full design.
package api

import (
	"fmt"
	"net/http"

	"github.com/spf13/cobra"
)

func newViewerTokenCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "viewer-token",
		Short: "Manage the daemon's single global read-only token (used by the dashboard)",
	}
	cmd.AddCommand(newViewerTokenCreateCmd())
	cmd.AddCommand(newViewerTokenStatusCmd())
	cmd.AddCommand(newViewerTokenRevokeCmd())
	return cmd
}

func newViewerTokenCreateCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "create",
		Short: "Mint (or rotate) the viewer token — shown once, paste it into the dashboard",
		RunE: func(cmd *cobra.Command, args []string) error {
			if err := call(cmd, http.MethodPost, "/v1/viewer-token", nil); err != nil {
				return err
			}
			fmt.Fprintln(cmd.OutOrStdout(), "\nSave the \"token\" value above now — it will not be shown again. Running this again replaces it (the old one stops working immediately).")
			fmt.Fprintln(cmd.OutOrStdout(), "\nHeads up: this token also grants read access to captured request/response bodies (the traffic tab) across every tunnel, not just tunnel status — anyone you share it with can see whatever passed through your tunnels. Known-sensitive headers (Authorization, Cookie, etc.) are already redacted before capture, but body content isn't, and can't reliably be. Only share this with someone you'd trust with that content.")
			return nil
		},
	}
}

func newViewerTokenStatusCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Show whether a viewer token currently exists (never shows the value)",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/viewer-token", nil)
		},
	}
}

func newViewerTokenRevokeCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "revoke",
		Short: "Revoke the viewer token immediately",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/viewer-token", nil)
		},
	}
}
