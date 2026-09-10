// App-to-app (peer-to-peer) tunnels: connect two instances of this daemon
// directly over iroh, no Datum Cloud involvement at any layer. See
// NOTES.md's "App-to-app (peer-to-peer) tunnels" section for the design;
// daemon/src/peer.rs is the implementation these commands call into.
package api

import (
	"fmt"
	"net/http"

	"github.com/spf13/cobra"
)

func newPeerCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "peer",
		Short: "App-to-app tunnels: connect directly to another instance of this daemon, no Datum Cloud involved",
	}
	cmd.AddCommand(newPeerAdvertiseCmd())
	cmd.AddCommand(newPeerConnectCmd())
	cmd.AddCommand(newPeerListCmd())
	cmd.AddCommand(newPeerRevokeCmd())
	cmd.AddCommand(newPeerDisconnectCmd())
	return cmd
}

func newPeerAdvertiseCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "advertise",
		Short: "Advertise a local target for direct peer access — prints a ticket to hand to the other side",
		RunE: func(cmd *cobra.Command, args []string) error {
			endpoint, _ := cmd.Flags().GetString("endpoint")
			if endpoint == "" {
				return fmt.Errorf("--endpoint is required")
			}
			label, _ := cmd.Flags().GetString("label")
			var labelPtr *string
			if label != "" {
				labelPtr = &label
			}
			if err := call(cmd, http.MethodPost, "/v1/peers/advertise", map[string]any{
				"endpoint": endpoint,
				"label":    labelPtr,
			}); err != nil {
				return err
			}
			fmt.Fprintln(cmd.OutOrStdout(), "\nSend the \"ticket\" value above to whoever should be able to reach this target — possessing it is the entire credential.")
			return nil
		},
	}
	cmd.Flags().String("endpoint", "", "Local address to expose, host:port (required)")
	cmd.Flags().String("label", "", "Display name for this advertisement")
	return cmd
}

func newPeerConnectCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "connect",
		Short: "Consume a ticket and bind a local port that forwards directly to the advertising peer",
		RunE: func(cmd *cobra.Command, args []string) error {
			ticket, _ := cmd.Flags().GetString("ticket")
			if ticket == "" {
				return fmt.Errorf("--ticket is required")
			}
			bind, _ := cmd.Flags().GetString("bind")
			return call(cmd, http.MethodPost, "/v1/peers/connect", map[string]any{
				"ticket": ticket,
				"bind":   bind,
			})
		},
	}
	cmd.Flags().String("ticket", "", "Ticket string from the other side's 'peer advertise' (required)")
	cmd.Flags().String("bind", "127.0.0.1:0", "Local address to bind and forward from (0 = ephemeral port)")
	return cmd
}

func newPeerListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List this daemon's peer advertisements and active outbound peer connections",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/peers", nil)
		},
	}
}

func newPeerRevokeCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "revoke <resource-id>",
		Short: "Revoke an advertisement — every ticket issued for it stops working immediately",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/peers/advertise/"+args[0], nil)
		},
	}
}

func newPeerDisconnectCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "disconnect <connection-id>",
		Short: "Stop an active outbound peer connection (from 'peer connect')",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/peers/connections/"+args[0], nil)
		},
	}
}
