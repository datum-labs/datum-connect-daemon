// Package pidfile provides functions to manage PID files.
//
// PID file format (DAEMON-02):
//
//	<go-monitor-pid>
//	<rust-child-pid>
//	<start-time-rfc3339>
//	<binary-path>
package pidfile

import (
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// PidFile represents the contents of a PID file.
// Format: go-pid, rust-pid, start-time-rfc3339, binary-path — one per line.
type PidFile struct {
	GoPID      int
	RustPID    int
	StartTime  time.Time
	BinaryPath string
}

// Write creates a PID file at path. Creates parent directories if needed.
func Write(path string, goPID, rustPID int, startTime time.Time, binaryPath string) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create pid dir: %w", err)
	}
	content := fmt.Sprintf("%d\n%d\n%s\n%s\n", goPID, rustPID, startTime.Format(time.RFC3339), binaryPath)
	return os.WriteFile(path, []byte(content), 0644)
}

// Read parses a PID file and returns its contents.
// Returns an error if the file doesn't exist, is unreadable, or is malformed.
func Read(path string) (*PidFile, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read pid file: %w", err)
	}
	return Parse(data)
}

// Parse parses PID file content without reading from disk.
func Parse(data []byte) (*PidFile, error) {
	lines := splitLines(string(data))
	if len(lines) < 4 {
		return nil, fmt.Errorf("malformed pid file: expected 4 lines, got %d", len(lines))
	}

	var p PidFile
	if _, err := fmt.Sscanf(lines[0], "%d", &p.GoPID); err != nil {
		return nil, fmt.Errorf("malformed go pid: %w", err)
	}
	if _, err := fmt.Sscanf(lines[1], "%d", &p.RustPID); err != nil {
		return nil, fmt.Errorf("malformed rust pid: %w", err)
	}
	startTime, err := time.Parse(time.RFC3339, lines[2])
	if err != nil {
		return nil, fmt.Errorf("malformed start time: %w", err)
	}
	p.StartTime = startTime
	p.BinaryPath = lines[3]
	return &p, nil
}

// Exists checks if a PID file exists at path.
func Exists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// Remove deletes the PID file at path. Returns nil if file doesn't exist.
func Remove(path string) error {
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return err
	}
	return nil
}

func splitLines(s string) []string {
	var lines []string
	var current []byte
	for i := 0; i < len(s); i++ {
		if s[i] == '\n' {
			lines = append(lines, string(current))
			current = nil
		} else {
			current = append(current, s[i])
		}
	}
	if len(current) > 0 {
		lines = append(lines, string(current))
	}
	return lines
}
