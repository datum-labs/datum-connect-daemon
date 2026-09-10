package main

import (
	"fmt"
	"os"
	"os/exec"

	"github.com/spf13/cobra"
	"go.datum.net/datumctl-plugins/connect/internal/binary"
	"go.datum.net/datumctl-plugins/connect/internal/env"
	"go.datum.net/datumctl-plugins/connect/tunnel"
	"go.datum.net/datumctl/plugin"
)

// Overridden at build time via -ldflags "-X main.version=vX.Y.Z".
// See Taskfile.yaml and .goreleaser.yaml.
var version = "v0.1.0-dev"

func main() {
	// Serve manifest before cobra parses anything
	m := plugin.Manifest{
		Name:        "connect",
		Version:     version,
		Description: "Manage Datum Connect tunnels",
		APIVersion:  1,
	}
	plugin.ServeManifest(m)

	// Phase 11.5 D-09/D-10/D-11: refuse to run any tunnel subcommand
	// when DATUM_CONNECT_DIR is unset. ServeManifest above already
	// self-exits for the --plugin-manifest probe, so by this point
	// we are committed to running a real subcommand.
	if err := env.RequireConnectDir(); err != nil {
		env.FailConnectDirUnset(os.Stderr, err)
		os.Exit(64)
	}

	// Create root command with pre-wired flags
	cmd := plugin.NewRootCmd("connect", "Manage Datum Connect tunnels")
	cmd.Version = version
	cmd.AddCommand(tunnel.NewCmd())
	cmd.AddCommand(&cobra.Command{
		Use:   "version",
		Short: "Print the plugin and Rust binary versions",
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Printf("datumctl-connect %s\n", version)
			if binPath, err := binary.Discover(); err == nil {
				out, err := exec.Command(binPath, "--version").Output()
				if err == nil {
					fmt.Printf("datum-connect  %s", string(out))
				}
			}
		},
	})

	if err := cmd.Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
}
