// Package api provides thin HTTP client commands for the datum-connect-daemon
// local API (see tunnel/daemon and API-PLAN.md). Proof-of-concept only: no
// table rendering (the daemon's raw TunnelSummary JSON doesn't match
// internal/output.RenderTable's CLI-specific enriched shape — see the
// connect-lib daemon's list/create handlers), just pretty-printed JSON.
package api

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/spf13/cobra"

	"go.datum.net/datumctl-plugins/connect/internal/env"
)

const defaultPort = 47780

var httpClient = &http.Client{Timeout: 15 * time.Second}

// NewCmd returns the `tunnel api` command group — HTTP client commands
// against a running `datumctl connect tunnel daemon`.
func NewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "api",
		Short: "Call the local tunnel API daemon (proof-of-concept)",
	}
	cmd.PersistentFlags().Int("port", defaultPort, "Port the daemon is listening on")
	cmd.PersistentFlags().String("token", "", "Bearer token to authenticate with the daemon (defaults to DATUM_CONNECT_TOKEN, or the ambient setup token for interactive use)")

	cmd.AddCommand(newCreateCmd())
	cmd.AddCommand(newListCmd())
	cmd.AddCommand(newGetCmd())
	cmd.AddCommand(newProgressCmd())
	cmd.AddCommand(newStartCmd())
	cmd.AddCommand(newStopCmd())
	cmd.AddCommand(newDeleteCmd())
	cmd.AddCommand(newTrafficCmd())
	cmd.AddCommand(newReplayCmd())
	cmd.AddCommand(newTokenCmd())
	cmd.AddCommand(newAuditCmd())
	cmd.AddCommand(newPeerCmd())
	cmd.AddCommand(newViewerTokenCmd())
	cmd.AddCommand(newLogCmd())
	return cmd
}

func baseURL(cmd *cobra.Command) string {
	port, _ := cmd.Flags().GetInt("port")
	return fmt.Sprintf("http://127.0.0.1:%d", port)
}

// isInteractiveStdout reports whether stdout looks like a real terminal
// rather than a pipe/redirect/non-interactive capture (a script, an
// agent-wrapper's captured output, etc).
func isInteractiveStdout() bool {
	fi, err := os.Stdout.Stat()
	if err != nil {
		return false
	}
	return (fi.Mode() & os.ModeCharDevice) != 0
}

func readSetupToken() (string, error) {
	if err := env.RequireConnectDir(); err != nil {
		return "", err
	}
	path := filepath.Join(os.Getenv("DATUM_CONNECT_DIR"), "daemon_auth", "setup.token")
	b, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("read setup token (is the daemon running? see 'tunnel daemon status'): %w", err)
	}
	return strings.TrimSpace(string(b)), nil
}

// resolveToken picks the bearer credential for this call: an explicit
// --token flag or DATUM_CONNECT_TOKEN env var always wins. Absent either,
// interactive use falls back to the daemon's full-access setup token (read
// straight off disk — same OS user, same trust boundary the daemon already
// relies on via loopback binding), exactly as frictionless as before this
// auth layer existed. A non-interactive caller (a script, an agent) that
// didn't pass a token is refused rather than silently handed that same
// full-access credential — the whole point of a narrower, revocable
// operate token is defeated if forgetting to pass one just means "you get
// setup-tier access instead."
func resolveToken(cmd *cobra.Command) (string, error) {
	if t, _ := cmd.Flags().GetString("token"); t != "" {
		return t, nil
	}
	if t := os.Getenv("DATUM_CONNECT_TOKEN"); t != "" {
		return t, nil
	}
	if !isInteractiveStdout() {
		return "", fmt.Errorf("refusing to use ambient setup-tier credentials from a non-interactive context — pass --token or set DATUM_CONNECT_TOKEN")
	}
	return readSetupToken()
}

// doRequest performs an authenticated request against the daemon and
// returns the raw response body and status line, without printing anything
// — shared by call() (which pretty-prints) and callers that need the
// parsed response for their own logic, e.g. showing a plain-language
// token-scope summary before minting (see newTokenCreateCmd).
func doRequest(cmd *cobra.Command, method, path string, body any) ([]byte, string, int, error) {
	var reqBody io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return nil, "", 0, fmt.Errorf("encode request: %w", err)
		}
		reqBody = bytes.NewReader(b)
	}

	req, err := http.NewRequest(method, baseURL(cmd)+path, reqBody)
	if err != nil {
		return nil, "", 0, fmt.Errorf("build request: %w", err)
	}
	if reqBody != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	token, err := resolveToken(cmd)
	if err != nil {
		return nil, "", 0, err
	}
	req.Header.Set("Authorization", "Bearer "+token)

	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, "", 0, fmt.Errorf("call daemon (is it running? see 'tunnel daemon status'): %w", err)
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, "", 0, fmt.Errorf("read response: %w", err)
	}
	return respBody, resp.Status, resp.StatusCode, nil
}

// call makes an HTTP request against the daemon and pretty-prints the JSON
// response body to cmd's stdout. A non-2xx response is surfaced as an error
// (the daemon returns {"error": "..."} bodies).
func call(cmd *cobra.Command, method, path string, body any) error {
	respBody, status, statusCode, err := doRequest(cmd, method, path, body)
	if err != nil {
		return err
	}

	var pretty bytes.Buffer
	if err := json.Indent(&pretty, respBody, "", "  "); err != nil {
		// Not JSON — print raw.
		fmt.Fprintln(cmd.OutOrStdout(), string(respBody))
	} else {
		fmt.Fprintln(cmd.OutOrStdout(), pretty.String())
	}

	if statusCode < 200 || statusCode >= 300 {
		return fmt.Errorf("daemon returned %s", status)
	}
	return nil
}

// tunnelSummary is the subset of GET /v1/tunnels/:id this package needs for
// the token-mint consent summary — deliberately not the full shape (which
// also carries connector/programming detail this command doesn't use).
type tunnelSummary struct {
	Label     string   `json:"label"`
	Endpoint  string   `json:"endpoint"`
	Hostnames []string `json:"hostnames"`
}

func fetchTunnelSummary(cmd *cobra.Command, id string) (*tunnelSummary, error) {
	body, status, statusCode, err := doRequest(cmd, http.MethodGet, "/v1/tunnels/"+id, nil)
	if err != nil {
		return nil, err
	}
	if statusCode < 200 || statusCode >= 300 {
		return nil, fmt.Errorf("could not look up tunnel %q to describe what this token would grant: daemon returned %s", id, status)
	}
	var t tunnelSummary
	if err := json.Unmarshal(body, &t); err != nil {
		return nil, fmt.Errorf("parse tunnel summary: %w", err)
	}
	return &t, nil
}

func (t *tunnelSummary) target() string {
	if len(t.Hostnames) > 0 {
		return t.Hostnames[0]
	}
	return "(no public hostname yet)"
}

func newCreateCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "create",
		Short: "Create a tunnel profile (does not start it — see 'tunnel api start')",
		RunE: func(cmd *cobra.Command, args []string) error {
			label, _ := cmd.Flags().GetString("label")
			origin, _ := cmd.Flags().GetString("origin")
			if endpoint, _ := cmd.Flags().GetString("endpoint"); origin == "" && endpoint != "" {
				origin = endpoint
			}
			if origin == "" {
				return fmt.Errorf("--origin is required")
			}
			if label == "" {
				label = origin
			}
			return call(cmd, http.MethodPost, "/v1/tunnels", map[string]string{
				"label":    label,
				"endpoint": origin,
			})
		},
	}
	cmd.Flags().String("label", "", "Display name for the tunnel (defaults to --origin)")
	cmd.Flags().String("origin", "", "Local address to expose (host:port, required)")
	cmd.Flags().String("endpoint", "", "Local address to expose (host:port, required)")
	cmd.Flags().MarkDeprecated("endpoint", "use --origin instead")
	return cmd
}

func newListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List tunnels known to the daemon",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/tunnels", nil)
		},
	}
}

func newGetCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "get <id>",
		Short: "Show one tunnel",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/tunnels/"+args[0], nil)
		},
	}
}

func newProgressCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "progress <id>",
		Short: "Show setup-pipeline progress for a tunnel (proxy accepted, certs, connector ready, DNS published, ...)",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/tunnels/"+args[0]+"/progress", nil)
		},
	}
}

func newStartCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "start <id>",
		Short: "Turn a tunnel on",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/start", nil)
		},
	}
}

func newStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "stop <id>",
		Short: "Turn a tunnel off",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/stop", nil)
		},
	}
}

func newTrafficCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "traffic <id> [exchange-id]",
		Short: "List captured HTTP traffic for a tunnel, or show one exchange in full",
		Args:  cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			path := "/v1/tunnels/" + args[0] + "/traffic"
			if len(args) == 2 {
				path += "/" + args[1]
			}
			return call(cmd, http.MethodGet, path, nil)
		},
	}
}

func newReplayCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "replay <id> <exchange-id>",
		Short: "Re-send a captured request to the tunnel's real local target",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/traffic/"+args[1]+"/replay", nil)
		},
	}
}

func newDeleteCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "delete <id>",
		Short: "Delete a tunnel profile",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/tunnels/"+args[0], nil)
		},
	}
}

// newTokenCmd groups the operate-tier token lifecycle commands. Minting
// requires setup-tier access (an interactive session, or an explicit
// --token for a setup token) — an operate token can never mint another
// token, only start/stop/read-progress on the one tunnel it's scoped to.
func newTokenCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "token",
		Short: "Manage operate-tier tokens (each scoped to one tunnel's start/stop/progress only)",
	}
	cmd.AddCommand(newTokenCreateCmd())
	cmd.AddCommand(newTokenListCmd())
	cmd.AddCommand(newTokenRevokeCmd())
	return cmd
}

func newTokenCreateCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "create <tunnel-id>",
		Short: "Mint an operate token for a tunnel — hand this to an agent instead of full setup-tier access",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ttl, _ := cmd.Flags().GetDuration("ttl")
			skipConfirm, _ := cmd.Flags().GetBool("yes")

			// This is the moment someone is most likely to reach for a
			// setup token out of habit instead of minting a scoped one —
			// so name exactly what's being granted, in plain language,
			// before it's granted, rather than trusting a bare token
			// string. See NOTES.md's "agent-friendly, human stays in
			// control" discussion, 2026-09-08.
			tunnel, err := fetchTunnelSummary(cmd, args[0])
			if err != nil {
				return err
			}
			ttlDesc := "never expires"
			if ttl > 0 {
				ttlDesc = fmt.Sprintf("expires in %s", ttl)
			}
			fmt.Fprintf(cmd.OutOrStdout(),
				"About to grant: start/stop access to %q (%s -> %s), nothing else — not other tunnels, not creating new ones. %s.\n\n",
				tunnel.Label, tunnel.Endpoint, tunnel.target(), ttlDesc)

			if !skipConfirm && isInteractiveStdout() {
				fmt.Fprint(cmd.OutOrStdout(), "Continue? [y/N] ")
				var response string
				fmt.Fscanln(cmd.InOrStdin(), &response)
				response = strings.ToLower(strings.TrimSpace(response))
				if response != "y" && response != "yes" {
					fmt.Fprintln(cmd.OutOrStdout(), "Aborted — no token was created.")
					return nil
				}
				fmt.Fprintln(cmd.OutOrStdout())
			}

			var ttlSeconds *int64
			if ttl > 0 {
				s := int64(ttl.Seconds())
				ttlSeconds = &s
			}
			if err := call(cmd, http.MethodPost, "/v1/tunnels/"+args[0]+"/tokens", map[string]any{
				"ttl_seconds": ttlSeconds,
			}); err != nil {
				return err
			}
			fmt.Fprintln(cmd.OutOrStdout(), "\nSave the \"bearer\" value above now — it will not be shown again.")
			return nil
		},
	}
	cmd.Flags().Duration("ttl", 24*time.Hour, "How long the token stays valid (e.g. 1h, 24h). 0 = never expires.")
	cmd.Flags().BoolP("yes", "y", false, "Skip the confirmation prompt (for scripts/automation)")
	return cmd
}

func newTokenListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list <tunnel-id>",
		Short: "List a tunnel's operate tokens (metadata only — secrets are never shown again after creation)",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/tunnels/"+args[0]+"/tokens", nil)
		},
	}
}

func newTokenRevokeCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "revoke <tunnel-id> <token-id>",
		Short: "Revoke an operate token immediately — the human kill switch",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodDelete, "/v1/tunnels/"+args[0]+"/tokens/"+args[1], nil)
		},
	}
}

func newAuditCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "audit",
		Short: "Show the daemon's append-only audit log (create/delete/start/stop/auto_expired/token events)",
		RunE: func(cmd *cobra.Command, args []string) error {
			return call(cmd, http.MethodGet, "/v1/audit", nil)
		},
	}
}
