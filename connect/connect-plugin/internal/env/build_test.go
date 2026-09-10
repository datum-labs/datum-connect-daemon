package env

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.datum.net/datumctl/plugin"
)

func TestBuild_PassesThroughOsEnviron(t *testing.T) {
	t.Setenv("MY_CUSTOM_PASSTHROUGH_VAR_FOR_TEST", "hello-passthrough")
	ctx := plugin.PluginContext{
		APIHost:           "api.example",
		Project:           "proj",
		CredentialsHelper: "helper",
		Session:           "sess",
	}
	got := Build(ctx)
	found := false
	for _, e := range got {
		if e == "MY_CUSTOM_PASSTHROUGH_VAR_FOR_TEST=hello-passthrough" {
			found = true
			break
		}
	}
	if !found {
		t.Errorf("Build should pass os.Environ() through; missing custom var")
	}
}

func TestBuild_DoesNotInjectConnectDir(t *testing.T) {
	t.Setenv("DATUM_CONNECT_DIR", "/tmp/should-be-inherited")
	ctx := plugin.PluginContext{Project: "should-not-appear"}
	got := Build(ctx)
	// Count occurrences of DATUM_CONNECT_DIR= — must be 1 (the one we
	// set via t.Setenv, passed through os.Environ()). If Build appends
	// its own, we'd see 2.
	count := 0
	for _, e := range got {
		if strings.HasPrefix(e, "DATUM_CONNECT_DIR=") {
			count++
		}
	}
	if count != 1 {
		t.Errorf("Build should not inject DATUM_CONNECT_DIR; want 1 entry from os.Environ(), got %d", count)
	}
}

func TestBuild_DoesNotEmitLegacyConnectRepo(t *testing.T) {
	// Legacy DATUM_CONNECT_REPO was the bug; assert it never appears in
	// the produced slice unless the inherited env already had it.
	os.Unsetenv("DATUM_CONNECT_REPO")
	ctx := plugin.PluginContext{Project: "test-project-slug"}
	got := Build(ctx)
	for _, e := range got {
		if strings.HasPrefix(e, "DATUM_CONNECT_REPO=") {
			t.Errorf("Build must not emit DATUM_CONNECT_REPO; got %q", e)
		}
	}
}

func TestBuild_AppendsExactlyThreePluginVars(t *testing.T) {
	// Lock the contract: Build adds 3 vars (api-host, helper, session).
	// DATUM_ACCESS_TOKEN was removed in Phase 13-06 (binary obtains token
	// via credentials helper, not env). DATUM_CONNECT_DIR comes via
	// os.Environ() pass-through.
	os.Unsetenv("DATUM_API_HOST")
	os.Unsetenv("DATUM_CREDENTIALS_HELPER")
	os.Unsetenv("DATUM_SESSION")
	ctx := plugin.PluginContext{
		APIHost:           "h",
		CredentialsHelper: "c",
		Session:           "s",
	}
	got := Build(ctx)
	wantPrefixes := []string{
		"DATUM_API_HOST=h",
		"DATUM_CREDENTIALS_HELPER=c",
		"DATUM_SESSION=s",
	}
	for _, want := range wantPrefixes {
		found := false
		for _, e := range got {
			if e == want {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("Build missing entry %q", want)
		}
	}
}

func TestRequireConnectDir_SetReturnsNil(t *testing.T) {
	t.Setenv("DATUM_CONNECT_DIR", "/some/path")
	if err := RequireConnectDir(); err != nil {
		t.Errorf("RequireConnectDir() with var set = %v, want nil", err)
	}
}

func TestRequireConnectDir_UnsetSetsDefault(t *testing.T) {
	os.Unsetenv("DATUM_CONNECT_DIR")
	err := RequireConnectDir()
	if err != nil {
		t.Fatalf("RequireConnectDir() with var unset = %v, want nil (sets default)", err)
	}
	got := os.Getenv("DATUM_CONNECT_DIR")
	if got == "" {
		t.Fatal("RequireConnectDir() didn't set DATUM_CONNECT_DIR")
	}
	home, _ := os.UserHomeDir()
	want := filepath.Join(home, ".datumctl", "connect")
	if got != want {
		t.Errorf("RequireConnectDir() set DATUM_CONNECT_DIR=%q, want %q", got, want)
	}
}

func TestFailConnectDirUnset_WritesDirectiveMessage(t *testing.T) {
	var buf bytes.Buffer
	FailConnectDirUnset(&buf, fmt.Errorf("test error"))
	out := buf.String()
	required := []string{
		"Error: test error",
		".datumctl/connect",
		"(exit 64)",
	}
	for _, want := range required {
		if !strings.Contains(out, want) {
			t.Errorf("directive message missing %q; got:\n%s", want, out)
		}
	}
}
