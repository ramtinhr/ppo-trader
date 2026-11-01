package engine

import (
	"os"
	"path/filepath"
	"testing"
)

func TestFindPrefersDTEngineDir(t *testing.T) {
	dir := t.TempDir()
	bin := filepath.Join(dir, "simulate")
	if err := os.WriteFile(bin, []byte("#!/bin/sh\necho ok\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DT_ENGINE_DIR", dir)
	got, err := Find("simulate")
	if err != nil {
		t.Fatal(err)
	}
	abs, _ := filepath.Abs(bin)
	if got != abs && got != bin {
		t.Fatalf("got %q want %q", got, abs)
	}
}

func TestFindMissing(t *testing.T) {
	t.Setenv("DT_ENGINE_DIR", t.TempDir())
	// Clear PATH influence for this nonsense name.
	if _, err := Find("dt-engine-bin-that-does-not-exist-xyz"); err == nil {
		t.Fatal("expected error")
	}
}
