package pidfile

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"
)

// TerminateGracefully sends SIGTERM to pid, polls PIDAlive until it exits or
// grace elapses, then sends SIGKILL if it's still alive. Shared by every
// caller that stops a supervised process (`tunnel stop`, `tunnel daemon
// stop`) so the shutdown sequence and grace period can't silently drift
// between them.
func TerminateGracefully(pid int, grace time.Duration) error {
	proc, err := os.FindProcess(pid)
	if err != nil {
		return err
	}

	if runtime.GOOS == "windows" {
		// os.Process.Signal only supports os.Kill on Windows — a SIGTERM
		// send here silently fails, so waiting out the grace period first
		// would just be a pointless delay before the same Kill call below.
		return proc.Kill()
	}

	_ = proc.Signal(syscall.SIGTERM)

	deadline := time.Now().Add(grace)
	for time.Now().Before(deadline) {
		if !PIDAlive(pid) {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	if PIDAlive(pid) {
		return proc.Signal(syscall.SIGKILL)
	}
	return nil
}

// RunningTunnel holds info about a discovered running tunnel process.
type RunningTunnel struct {
	Name       string
	GoPID      int
	RustPID    int
	StartTime  time.Time
	BinaryPath string
	Status     string // "Running", "Starting", "Degraded", "Zombie"
}

// ListRunningTunnels scans the tunnels directory and returns all tunnels
// with their current status based on PID file and process health.
func ListRunningTunnels(stateDir string) ([]RunningTunnel, error) {
	tunnelsDir := filepath.Join(stateDir, "tunnels")
	entries, err := os.ReadDir(tunnelsDir)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}

	var tunnels []RunningTunnel
	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != ".pid" {
			continue
		}
		name := strings.TrimSuffix(entry.Name(), ".pid")
		path := filepath.Join(tunnelsDir, entry.Name())

		pf, err := Read(path)
		if err != nil {
			continue
		}

		t := RunningTunnel{
			Name:       name,
			GoPID:      pf.GoPID,
			RustPID:    pf.RustPID,
			StartTime:  pf.StartTime,
			BinaryPath: pf.BinaryPath,
			Status:     computeTunnelStatus(pf),
		}
		tunnels = append(tunnels, t)
	}
	return tunnels, nil
}

// computeTunnelStatus determines the tunnel status from a PidFile.
func computeTunnelStatus(pf *PidFile) string {
	goAlive := PIDAlive(pf.GoPID)
	rustAlive := PIDAlive(pf.RustPID)

	switch {
	case !goAlive && !rustAlive:
		return "Zombie"
	case goAlive && rustAlive:
		return "Running"
	case goAlive && !rustAlive:
		return "Degraded"
	case !goAlive && rustAlive:
		return "Zombie"
	default:
		return "Unknown"
	}
}
