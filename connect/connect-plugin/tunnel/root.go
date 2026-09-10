package tunnel

import (
	"github.com/spf13/cobra"

	tunnelapi "go.datum.net/datumctl-plugins/connect/tunnel/api"
	tunneldaemon "go.datum.net/datumctl-plugins/connect/tunnel/daemon"
	deletecmd "go.datum.net/datumctl-plugins/connect/tunnel/delete"
	"go.datum.net/datumctl-plugins/connect/tunnel/install"
	"go.datum.net/datumctl-plugins/connect/tunnel/list"
	"go.datum.net/datumctl-plugins/connect/tunnel/listen"
	"go.datum.net/datumctl-plugins/connect/tunnel/logs"
	"go.datum.net/datumctl-plugins/connect/tunnel/ps"
	"go.datum.net/datumctl-plugins/connect/tunnel/run"
	"go.datum.net/datumctl-plugins/connect/tunnel/start"
	"go.datum.net/datumctl-plugins/connect/tunnel/status"
	"go.datum.net/datumctl-plugins/connect/tunnel/stop"
	"go.datum.net/datumctl-plugins/connect/tunnel/uninstall"
	"go.datum.net/datumctl-plugins/connect/tunnel/update"
)

// NewCmd returns the tunnel root command with all subcommands.
func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "tunnel",
		Short: "Manage tunnels",
		Long:  "Manage tunnels to local services via Datum Connect",
	}

	cmd.AddCommand(list.NewCmd())
	cmd.AddCommand(listen.NewCmd())
	cmd.AddCommand(update.NewCmd())
	cmd.AddCommand(deletecmd.NewCmd())
	cmd.AddCommand(ps.NewCmd())
	cmd.AddCommand(stop.NewCmd())
	cmd.AddCommand(logs.NewCmd())
	cmd.AddCommand(status.NewCmd())
	cmd.AddCommand(install.NewCmd())
	cmd.AddCommand(uninstall.NewCmd())
	cmd.AddCommand(start.NewCmd())
	cmd.AddCommand(run.NewCmd())
	cmd.AddCommand(tunneldaemon.NewCmd())
	cmd.AddCommand(tunnelapi.NewCmd())

	return cmd
}
