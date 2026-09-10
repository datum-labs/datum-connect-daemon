// Package svcconfig provides tunnel configuration serialization.
//
// Config files are stored in the platform-appropriate config directory:
//
//	linux:   $XDG_CONFIG_HOME/datumctl/connect/config/  (default ~/.config/...)
//	darwin:  ~/Library/Application Support/datumctl/connect/config/
//	windows: %AppData%/datumctl/connect/config/
//
// Schema (9 fields, Phase 13 D-04):
//   - name, label, endpoint, project, session — required on install
//   - org, api_host, created_at — optional metadata
//   - credentials_helper_path — captured at install time, used at unit run time
package svcconfig

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// TunnelConfig represents a persisted tunnel configuration.
type TunnelConfig struct {
	Name                  string `yaml:"name"`
	Label                 string `yaml:"label"`
	Endpoint              string `yaml:"endpoint"`
	Project               string `yaml:"project"`
	Session               string `yaml:"session"`
	Org                   string `yaml:"org,omitempty"`
	APIHost               string `yaml:"api_host,omitempty"`
	CreatedAt             string `yaml:"created_at,omitempty"`
	CredentialsHelperPath string `yaml:"credentials_helper_path,omitempty"`
}

// ConfigDir returns the plugin config directory path.
// Uses os.UserConfigDir() which follows XDG on Linux, platform conventions
// on macOS and Windows.
// Exposed as a variable for testability (tests can override it).
var ConfigDir = func() string {
	dir, err := os.UserConfigDir()
	if err != nil {
		dir = "."
	}
	return filepath.Join(dir, "datumctl", "connect", "config")
}

// ConfigFilePath returns the config file path for a named tunnel.
func ConfigFilePath(name string) string {
	return filepath.Join(ConfigDir(), name+".yaml")
}

// Save writes a TunnelConfig to the given path as YAML.
// Automatically sets CreatedAt if empty.
func Save(cfg TunnelConfig, path string) error {
	if cfg.CreatedAt == "" {
		cfg.CreatedAt = time.Now().Format(time.RFC3339)
	}
	data, err := yaml.Marshal(cfg)
	if err != nil {
		return fmt.Errorf("marshal config: %w", err)
	}
	dir := path[:strings.LastIndexByte(path, '/')]
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create config dir: %w", err)
	}
	return os.WriteFile(path, data, 0644)
}

// Load reads a TunnelConfig from the given path.
func Load(path string) (TunnelConfig, error) {
	var cfg TunnelConfig
	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, fmt.Errorf("read config: %w", err)
	}
	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse config: %w", err)
	}
	return cfg, nil
}

// Exists checks whether a config file exists for the given tunnel name.
func Exists(name string) (bool, error) {
	path := ConfigFilePath(name)
	_, err := os.Stat(path)
	if err == nil {
		return true, nil
	}
	if os.IsNotExist(err) {
		return false, nil
	}
	return false, err
}

// Remove deletes the config file for the given tunnel name.
func Remove(name string) error {
	path := ConfigFilePath(name)
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return err
	}
	return nil
}
