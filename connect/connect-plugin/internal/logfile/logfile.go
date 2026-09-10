// Package logfile provides functions to resolve log file paths.
package logfile

import (
	"os"
	"os/user"
	"path/filepath"
	"runtime"
)

// ResolveLogPath returns the log file path for a named tunnel.
// linux: $XDG_STATE_HOME/datumctl/connect/logs/<name>.log
// darwin: ~/Library/Logs/datumctl/connect/<name>.log
// windows: %LOCALAPPDATA%\datumctl\connect\logs\<name>.log
func ResolveLogPath(name string) string {
	switch runtime.GOOS {
	case "windows":
		return filepath.Join(os.Getenv("LOCALAPPDATA"), "datumctl", "connect", "logs", name+".log")
	case "darwin":
		u, err := user.Current()
		if err != nil {
			return filepath.Join(".", "logs", name+".log")
		}
		return filepath.Join(u.HomeDir, "Library", "Logs", "datumctl", "connect", name+".log")
	default:
		xdg := os.Getenv("XDG_STATE_HOME")
		if xdg == "" {
			xdg = filepath.Join(os.Getenv("HOME"), ".local", "state")
		}
		return filepath.Join(xdg, "datumctl", "connect", "logs", name+".log")
	}
}
