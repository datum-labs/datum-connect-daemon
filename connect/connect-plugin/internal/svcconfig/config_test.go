package svcconfig

import (
	"path/filepath"
	"strings"
	"testing"
)

func TestConfigDir_NotEmpty(t *testing.T) {
	d := ConfigDir()
	if d == "" {
		t.Fatal("ConfigDir() returned empty string")
	}
	if !strings.Contains(d, "datumctl") {
		t.Errorf("ConfigDir() should contain 'datumctl', got %q", d)
	}
}

func TestConfigFilePath(t *testing.T) {
	p := ConfigFilePath("test-tun")
	if !strings.HasSuffix(p, "test-tun.yaml") {
		t.Errorf("ConfigFilePath should end with 'test-tun.yaml', got %q", p)
	}
}

func TestSaveAndLoad(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "test.yaml")

	cfg := TunnelConfig{
		Name:     "test-tun",
		Label:    "test",
		Endpoint: "localhost:8080",
		Session:  "my-session",
		Project:  "my-project",
		Org:      "my-org",
		APIHost:  "https://api.datum.net",
	}

	if err := Save(cfg, path); err != nil {
		t.Fatalf("Save() failed: %v", err)
	}

	loaded, err := Load(path)
	if err != nil {
		t.Fatalf("Load() failed: %v", err)
	}

	if loaded.Name != cfg.Name {
		t.Errorf("Name = %q, want %q", loaded.Name, cfg.Name)
	}
	if loaded.Endpoint != cfg.Endpoint {
		t.Errorf("Endpoint = %q, want %q", loaded.Endpoint, cfg.Endpoint)
	}
	if loaded.Session != cfg.Session {
		t.Errorf("Session = %q, want %q", loaded.Session, cfg.Session)
	}
}

func TestExists(t *testing.T) {
	dir := t.TempDir()
	// Override ConfigDir for testing
	orig := ConfigDir
	ConfigDir = func() string { return filepath.Join(dir, "config") }
	defer func() { ConfigDir = orig }()

	exists, err := Exists("noexist")
	if err != nil {
		t.Fatalf("Exists() failed: %v", err)
	}
	if exists {
		t.Error("Exists() should be false for non-existent config")
	}

	cfg := TunnelConfig{Name: "mytun", Endpoint: "localhost:8080", Session: "sess"}
	if err := Save(cfg, ConfigFilePath("mytun")); err != nil {
		t.Fatalf("Save() failed: %v", err)
	}

	exists, err = Exists("mytun")
	if err != nil {
		t.Fatalf("Exists() failed: %v", err)
	}
	if !exists {
		t.Error("Exists() should be true after Save")
	}
}

func TestRemove(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "remove.yaml")
	cfg := TunnelConfig{Name: "remove-tun", Endpoint: "localhost:8080", Session: "sess"}
	Save(cfg, path)

	if err := Remove("remove-tun"); err != nil {
		t.Fatalf("Remove() failed: %v", err)
	}

	exists, _ := Exists("remove-tun")
	if exists {
		t.Error("config should not exist after Remove")
	}
}
