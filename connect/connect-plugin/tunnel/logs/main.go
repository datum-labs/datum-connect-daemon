package logs

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/state"
)

func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "logs --name N [--follow]",
		Short: "View tunnel logs",
		RunE:  runLogs,
	}
	cmd.Flags().String("name", "", "Tunnel name (required)")
	cmd.Flags().BoolP("follow", "f", false, "Follow log output")
	return cmd
}

func runLogs(cmd *cobra.Command, args []string) error {
	name, _ := cmd.Flags().GetString("name")
	follow, _ := cmd.Flags().GetBool("follow")

	if name == "" {
		fmt.Fprintln(os.Stderr, "Error: --name is required")
		os.Exit(64)
	}

	logPath := state.LogFilePath(name)

	// Check if log file exists
	if _, err := os.Stat(logPath); err != nil {
		if os.IsNotExist(err) {
			return fmt.Errorf("no log file for tunnel '%s' (try running with --log-file)", name)
		}
		return fmt.Errorf("access log file: %w", err)
	}

	if follow {
		return followLogs(cmd, logPath)
	}
	return printLogs(cmd, logPath)
}

func printLogs(cmd *cobra.Command, logPath string) error {
	data, err := os.ReadFile(logPath)
	if err != nil {
		return fmt.Errorf("read log file: %w", err)
	}
	fmt.Fprint(cmd.OutOrStdout(), string(data))
	return nil
}

func followLogs(cmd *cobra.Command, logPath string) error {
	f, err := os.Open(logPath)
	if err != nil {
		return fmt.Errorf("open log file: %w", err)
	}
	defer f.Close()

	// Seek to end to start following from new content
	_, _ = f.Seek(0, io.SeekEnd)
	reader := bufio.NewReader(f)

	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			if err == io.EOF {
				// Wait for new content
				time.Sleep(100 * time.Millisecond)
				continue
			}
			return fmt.Errorf("read: %w", err)
		}
		fmt.Fprint(cmd.OutOrStdout(), line)
	}
}
