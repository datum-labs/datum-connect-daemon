// fake-credentials-helper emulates datumctl auth get-token for testing.
//
// Modes (controlled by FAKE_HELPER_MODE env var):
//   (default): prints a static JWT with exp=1h in future
//   expired-token: prints JWT with exp in the past
//   refuses-token: exits 1 with error message
//   slow-to-respond: sleeps 5s then prints JWT
//   session-dependent: only succeeds if --session matches "test-session"
//
// Flags: --session <name>
package main

import (
	"fmt"
	"os"
	"strings"
	"time"
)

func main() {
	args := os.Args[1:]

	mode := os.Getenv("FAKE_HELPER_MODE")

	// Parse --session flag
	session := ""
	for i, arg := range args {
		if arg == "--session" && i+1 < len(args) {
			session = args[i+1]
			break
		}
		if strings.HasPrefix(arg, "--session=") {
			session = arg[len("--session="):]
			break
		}
	}

	// Handle refuses-token mode
	if mode == "refuses-token" {
		fmt.Fprintln(os.Stderr, "error: token retrieval refused")
		os.Exit(1)
	}

	// Handle slow-to-respond mode
	if mode == "slow-to-respond" {
		time.Sleep(5 * time.Second)
	}

	// Handle session-dependent mode
	if mode == "session-dependent" && session != "test-session" {
		fmt.Fprintf(os.Stderr, "error: session %q not found\n", session)
		os.Exit(1)
	}

	// Default: print a static JWT
	if mode == "expired-token" {
		// Expired token (exp in the past)
		fmt.Println("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ0ZXN0LXVzZXIiLCJleHAiOjEwMDAwMDAwMDAsImlhdCI6MTAwMDAwMDAwMH0.expired-signature")
	} else {
		// Valid token (exp 1 hour from now)
		exp := time.Now().Add(1 * time.Hour).Unix()
		payload := fmt.Sprintf(`{"sub":"test-user","exp":%d,"iat":%d}`, exp, time.Now().Unix())
		// Simple base64-like encoding for demo (not real JWT)
		fmt.Println("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9." + encodeToBase64(payload) + ".fake-signature")
	}
}

func encodeToBase64(s string) string {
	// Simplified base64-like encoding for demo purposes
	// In a real JWT this would be proper base64url encoding
	result := ""
	for _, r := range s {
		result += string(r)
	}
	return result
}
