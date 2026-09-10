// Package env provides functions to build the child process environment
// and to gate the plugin on required env-var contracts.
package env

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"go.datum.net/datumctl/plugin"
)

// Build returns the child process environment for the Rust binary.
//
// Phase 11.5: DATUM_CONNECT_DIR is OWNED by datumctl and arrives via
// os.Environ() pass-through; this function MUST NOT compute it, default
// it, or override it. The legacy DATUM_CONNECT_REPO=ctx.Project line is
// removed — it was the root cause of stray ./<project-slug>/ listen_key
// dirs (Phase 12 plan 12-02 scenario 6).
//
// Phase 13-06: DATUM_ACCESS_TOKEN line removed — the Rust binary now
// obtains its token from the credentials helper at startup. The helper
// receives DATUM_SESSION from the child env.
//
// Caller responsibility: check RequireConnectDir() before calling Build;
// if it returns an error, write FailConnectDirUnset() to stderr and
// os.Exit(64).
//
// Overrides, not appends: os.Environ() already contains datumctl's own
// DATUM_API_HOST/DATUM_CREDENTIALS_HELPER/DATUM_SESSION — datumctl sets
// these (sometimes empty, e.g. when it couldn't resolve a context) before
// exec-replacing into this plugin. A plain append() adds a second entry
// for each key rather than replacing the first, and most platforms'
// getenv-equivalent resolves duplicate keys by first occurrence — so the
// value this function is supposed to set silently loses to datumctl's own
// (possibly empty) one. WithOverrides removes any existing entry for each
// key before appending the authoritative value.
func Build(ctx plugin.PluginContext) []string {
	return WithOverrides(os.Environ(), map[string]string{
		"DATUM_API_HOST":           ctx.APIHost,
		"DATUM_CREDENTIALS_HELPER": ctx.CredentialsHelper,
		"DATUM_SESSION":            ctx.Session,
	})
}

// WithOverrides returns a copy of env with any existing entries for each
// key in overrides removed, then the given key=value pairs appended — so
// the appended value is the only one present, immune to first-occurrence-
// wins duplicate-key lookup semantics. See Build's doc comment for why a
// plain append() is not sufficient here.
func WithOverrides(env []string, overrides map[string]string) []string {
	result := make([]string, 0, len(env)+len(overrides))
	for _, kv := range env {
		key, _, found := strings.Cut(kv, "=")
		if found {
			if _, overridden := overrides[key]; overridden {
				continue
			}
		}
		result = append(result, kv)
	}
	for key, value := range overrides {
		result = append(result, key+"="+value)
	}
	return result
}

// RequireConnectDir ensures DATUM_CONNECT_DIR is set. When the env var
// is already present (e.g., injected by datumctl), it is left unchanged.
// Otherwise, the canonical default $HOME/.datumctl/connect is computed
// and exported into the current process environment so child processes
// inherit it.
//
// Returns an error only when the env var is missing AND the home
// directory cannot be determined — a truly broken environment.
func RequireConnectDir() error {
	if os.Getenv("DATUM_CONNECT_DIR") != "" {
		return nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return fmt.Errorf("DATUM_CONNECT_DIR is not set and cannot determine home directory: %w", err)
	}
	def := filepath.Join(home, ".datumctl", "connect")
	os.Setenv("DATUM_CONNECT_DIR", def)
	return nil
}

// FailConnectDirUnset writes a diagnostic error to w explaining that
// the home directory could not be determined. The caller is responsible
// for os.Exit(64).
func FailConnectDirUnset(w io.Writer, err error) {
	fmt.Fprintf(w, `Error: %v

The connect plugin normally stores its state at $HOME/.datumctl/connect/.
Could not determine your home directory to compute this path.

(exit 64)
`, err)
}
