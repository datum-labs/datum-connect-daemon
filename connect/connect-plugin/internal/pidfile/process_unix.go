//go:build !windows

package pidfile

import "syscall"

// PIDAlive checks whether a process with the given PID is currently
// running, by signaling PID 0 — which doesn't actually send a signal, just
// checks existence. Isolated in its own build-tag-gated file (paired with
// process_windows.go) because syscall.Kill doesn't exist in Go's Windows
// syscall package — a single cross-platform PIDAlive that references both
// implementations unconditionally breaks native Windows compilation
// entirely, which is exactly the bug this split fixes.
// Returns false for invalid PIDs, errors, and non-existent processes.
func PIDAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	return syscall.Kill(pid, 0) == nil
}
