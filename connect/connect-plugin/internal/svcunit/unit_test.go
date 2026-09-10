package svcunit

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.datum.net/datumctl-plugins/connect/internal/svcconfig"
)

func TestServiceName(t *testing.T) {
	name := ServiceName("my-tunnel")
	expected := "datumctl-connect-my-tunnel"
	if name != expected {
		t.Errorf("ServiceName = %q, want %q", name, expected)
	}
}

func TestServiceArgs(t *testing.T) {
	cfg := svcconfig.TunnelConfig{
		Name:     "test-tun",
		Label:    "test",
		Endpoint: "localhost:8080",
		Session:  "my-session",
	}
	args := ServiceArgs(cfg)
	joined := strings.Join(args, " ")
	if !strings.Contains(joined, "--name test-tun") {
		t.Errorf("args should contain --name, got: %s", joined)
	}
	if !strings.Contains(joined, "--endpoint localhost:8080") {
		t.Errorf("args should contain --endpoint, got: %s", joined)
	}
	if !strings.Contains(joined, "--session my-session") {
		t.Errorf("args should contain --session, got: %s", joined)
	}
	if !strings.Contains(joined, "--yes") {
		t.Errorf("args should contain --yes, got: %s", joined)
	}
}

func TestServiceArgs_NoLabel(t *testing.T) {
	cfg := svcconfig.TunnelConfig{
		Name:     "minimal",
		Endpoint: "localhost:8080",
		Session:  "sess",
	}
	args := ServiceArgs(cfg)
	joined := strings.Join(args, " ")
	if strings.Contains(joined, "--label") {
		t.Errorf("args should not contain --label for empty label, got: %s", joined)
	}
}

func TestBuildConfig_PopulatesConnectDirEnvVar(t *testing.T) {
	cfg := svcconfig.TunnelConfig{
		Name:     "my-tunnel",
		Endpoint: "localhost:8080",
	}
	sc, err := buildConfig(cfg, "/usr/local/bin/datumctl-connect")
	if err != nil {
		t.Fatalf("buildConfig() error = %v", err)
	}
	if sc.EnvVars == nil {
		t.Fatal("buildConfig() EnvVars is nil; want non-nil map")
	}
	got, ok := sc.EnvVars["DATUM_CONNECT_DIR"]
	if !ok {
		t.Fatalf("EnvVars missing DATUM_CONNECT_DIR; got %v", sc.EnvVars)
	}
	home, _ := os.UserHomeDir()
	want := filepath.Join(home, ".datumctl", "connect", "services", "my-tunnel")
	if got != want {
		t.Errorf("EnvVars[DATUM_CONNECT_DIR] = %q, want %q", got, want)
	}
}

func TestBuildConfig_PerServiceIsolation(t *testing.T) {
	// Two services must get DIFFERENT state subdirs (the whole point of D-12).
	sc1, err := buildConfig(svcconfig.TunnelConfig{Name: "alpha"}, "bin")
	if err != nil {
		t.Fatalf("buildConfig(alpha) error = %v", err)
	}
	sc2, err := buildConfig(svcconfig.TunnelConfig{Name: "beta"}, "bin")
	if err != nil {
		t.Fatalf("buildConfig(beta) error = %v", err)
	}
	if sc1.EnvVars["DATUM_CONNECT_DIR"] == sc2.EnvVars["DATUM_CONNECT_DIR"] {
		t.Errorf("two services share the same DATUM_CONNECT_DIR: %q",
			sc1.EnvVars["DATUM_CONNECT_DIR"])
	}
}

func TestBuildConfig_OnlyDatumConnectDirInEnvVars(t *testing.T) {
	// D-14: 11.5 adds ONLY DATUM_CONNECT_DIR. Other DATUM_* vars are
	// out of scope. If a future plan adds them, update this test.
	sc, err := buildConfig(svcconfig.TunnelConfig{Name: "x"}, "bin")
	if err != nil {
		t.Fatalf("buildConfig() error = %v", err)
	}
	if len(sc.EnvVars) != 1 {
		t.Errorf("EnvVars should have exactly 1 entry in 11.5; got %d: %v",
			len(sc.EnvVars), sc.EnvVars)
	}
}
