// Package rbaccheck provides RBAC permission checks via SelfSubjectAccessReview.
//
// Used at install time (Phase 13 D-05) to validate the service-account session
// has the necessary Kubernetes permissions before writing config and unit files.
package rbaccheck

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os/exec"
	"strings"
)

// SSARCheck represents a single SelfSubjectAccessReview check item.
type SSARCheck struct {
	Resource string // K8s resource name (e.g., "httpproxies")
	Verb     string // K8s verb (e.g., "create", "update")
	Group    string // API group (e.g., "datum-cloud.github.io")
}

// DefaultChecks returns the SSAR checks required for tunnel installation (D-05).
func DefaultChecks() []SSARCheck {
	return []SSARCheck{
		{Resource: "httpproxies", Verb: "create", Group: "datum-cloud.github.io"},
		{Resource: "httpproxies", Verb: "update", Group: "datum-cloud.github.io"},
		{Resource: "connectors", Verb: "create", Group: "datum-cloud.github.io"},
	}
}

// CheckAll runs all specified SSAR checks using the given K8s API server and token.
// Returns nil if all checks pass, or an error describing the first failure.
func CheckAll(apiServer, token string, checks []SSARCheck) error {
	if apiServer == "" {
		return fmt.Errorf("K8s API server URL is required (set DATUM_K8S_API)")
	}
	if token == "" {
		return fmt.Errorf("bearer token is required")
	}

	for _, c := range checks {
		allowed, err := checkAccess(apiServer, token, c)
		if err != nil {
			return fmt.Errorf("SSAR check failed for %s %s: %w", c.Verb, c.Resource, err)
		}
		if !allowed {
			return fmt.Errorf("service-account lacks permission to '%s' %s in group %s. Verify RBAC bindings and try again",
				c.Verb, c.Resource, c.Group)
		}
	}
	return nil
}

// checkAccess performs a single SelfSubjectAccessReview request.
func checkAccess(apiServer, token string, check SSARCheck) (bool, error) {
	ssar := map[string]interface{}{
		"apiVersion": "authorization.k8s.io/v1",
		"kind":       "SelfSubjectAccessReview",
		"spec": map[string]interface{}{
			"resourceAttributes": map[string]interface{}{
				"verb":     check.Verb,
				"resource": check.Resource,
				"group":    check.Group,
			},
		},
	}

	body, err := json.Marshal(ssar)
	if err != nil {
		return false, fmt.Errorf("marshal SSAR: %w", err)
	}

	url := apiServer + "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews"
	req, err := http.NewRequest("POST", url, bytes.NewReader(body))
	if err != nil {
		return false, fmt.Errorf("create SSAR request: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return false, fmt.Errorf("SSAR request failed: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusCreated && resp.StatusCode != http.StatusOK {
		return false, fmt.Errorf("SSAR API returned status %d", resp.StatusCode)
	}

	var result struct {
		Status struct {
			Allowed bool `json:"allowed"`
		} `json:"status"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return false, fmt.Errorf("parse SSAR response: %w", err)
	}

	return result.Status.Allowed, nil
}

// GetToken execs the credentials helper to obtain a bearer token for SSAR queries.
func GetToken(helper, session string) (string, error) {
	out, err := exec.Command(helper, "auth", "get-token", "--session", session).Output()
	if err != nil {
		return "", fmt.Errorf("credentials helper exec: %w", err)
	}
	token := strings.TrimSpace(string(out))
	if token == "" {
		return "", fmt.Errorf("empty token from credentials helper")
	}
	return token, nil
}
