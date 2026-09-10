// Package binary provides functions to locate the datum-connect binary.
package binary

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
)

// Discover locates the datum-connect binary.
// Search order: (1) FAKE_DATUM_CONNECT env var (test mode, absolute path),
// (2) same directory as the running plugin binary, (3) PATH lookup.
// Returns error if not found.
func Discover() (string, error) {
	// (1) Test override: FAKE_DATUM_CONNECT set to absolute path
	if path := os.Getenv("FAKE_DATUM_CONNECT"); path != "" {
		if _, err := os.Stat(path); err == nil {
			return path, nil
		}
	}
	// (2) Same directory as the running binary
	if path := findNextToSelf(); path != "" {
		return path, nil
	}
	// (3) PATH lookup
	if path := findInPath(); path != "" {
		return path, nil
	}
	return "", fmt.Errorf("datum-connect binary not found: not next to plugin binary and not in PATH")
}

// binaryName returns the platform-appropriate binary name. When
// FAKE_DATUM_CONNECT is set (test mode), returns the fake binary name.
//
// Phase 11.5 D-07: the legacy switch on DATUM_CONNECT_REPO was dead
// (both arms returned "datum-connect") and is removed.
func binaryName() string {
	name := "datum-connect"
	if os.Getenv("FAKE_DATUM_CONNECT") != "" {
		name = "fake-datum-connect"
	}
	return withExeSuffix(name)
}

// withExeSuffix appends the platform executable extension. os.Stat (used by
// findNextToSelf/findDaemonNextToSelf) does no extension resolution of its
// own — unlike exec.LookPath, which already handles PATHEXT — so same-
// directory discovery needs the real filename on Windows.
func withExeSuffix(name string) string {
	if runtime.GOOS == "windows" {
		return name + ".exe"
	}
	return name
}

// findNextToSelf returns the path to datum-connect in the same
// directory as the running binary, or "" if not found.
func findNextToSelf() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	path := filepath.Join(filepath.Dir(exe), binaryName())
	if _, err := os.Stat(path); err == nil {
		return path
	}
	return ""
}

// findInPath looks up datum-connect in PATH.
func findInPath() string {
	path, err := exec.LookPath(binaryName())
	if err != nil {
		return ""
	}
	return path
}

// DiscoverDaemon locates the datum-connect-daemon binary. Same search
// order as Discover, for the daemon binary instead of the per-tunnel
// datum-connect binary.
func DiscoverDaemon() (string, error) {
	if path := os.Getenv("FAKE_DATUM_CONNECT_DAEMON"); path != "" {
		if _, err := os.Stat(path); err == nil {
			return path, nil
		}
	}
	if path := findDaemonNextToSelf(); path != "" {
		return path, nil
	}
	if path, err := exec.LookPath(daemonBinaryName()); err == nil {
		return path, nil
	}
	return "", fmt.Errorf("datum-connect-daemon binary not found: not next to plugin binary and not in PATH")
}

func daemonBinaryName() string {
	name := "datum-connect-daemon"
	if os.Getenv("FAKE_DATUM_CONNECT_DAEMON") != "" {
		name = "fake-datum-connect-daemon"
	}
	return withExeSuffix(name)
}

func findDaemonNextToSelf() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	path := filepath.Join(filepath.Dir(exe), daemonBinaryName())
	if _, err := os.Stat(path); err == nil {
		return path
	}
	return ""
}
