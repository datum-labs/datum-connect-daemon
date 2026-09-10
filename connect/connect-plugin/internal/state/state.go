// Package state provides cross-platform plugin state directory resolution.
//
// State directory contains tunnel PID files, logs, and other runtime data.
// Paths follow platform conventions:
//
//	linux:   $XDG_STATE_HOME/datumctl/connect  (default ~/.local/share/datumctl/connect)
//	darwin:  ~/Library/Application Support/datumctl/connect
//	windows: %LOCALAPPDATA%/datumctl/connect
package state

import (
	"os"
	"os/user"
	"path/filepath"
	"runtime"
)

// Dir returns the plugin state base directory.
func Dir() string {
	switch runtime.GOOS {
	case "windows":
		return filepath.Join(os.Getenv("LOCALAPPDATA"), "datumctl", "connect")
	case "darwin":
		u, err := user.Current()
		if err != nil {
			return filepath.Join(".", "datumctl", "connect")
		}
		return filepath.Join(u.HomeDir, "Library", "Application Support", "datumctl", "connect")
	default:
		xdg := os.Getenv("XDG_STATE_HOME")
		if xdg == "" {
			xdg = filepath.Join(os.Getenv("HOME"), ".local", "state")
		}
		return filepath.Join(xdg, "datumctl", "connect")
	}
}

// TunnelDir returns the tunnels subdirectory.
func TunnelDir() string {
	return filepath.Join(Dir(), "tunnels")
}

// PidFilePath returns the PID file path for a named tunnel.
func PidFilePath(name string) string {
	return filepath.Join(TunnelDir(), name+".pid")
}

// LogDir returns the log directory.
// On macOS uses ~/Library/Logs/ (conventional), others use <state>/logs.
func LogDir() string {
	switch runtime.GOOS {
	case "darwin":
		u, err := user.Current()
		if err != nil {
			return filepath.Join(Dir(), "logs")
		}
		return filepath.Join(u.HomeDir, "Library", "Logs", "datumctl", "connect")
	default:
		return filepath.Join(Dir(), "logs")
	}
}

// LogFilePath returns the log file path for a named tunnel.
func LogFilePath(name string) string {
	return filepath.Join(LogDir(), name+".log")
}
