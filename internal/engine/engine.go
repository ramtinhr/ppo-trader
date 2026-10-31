// Package engine finds and execs Rust engine binaries, and helpers for batch simulate.
package engine

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// SearchDirs returns candidate directories for Rust binaries.
func SearchDirs() []string {
	var dirs []string
	if d := os.Getenv("DT_ENGINE_DIR"); d != "" {
		dirs = append(dirs, d)
	}
	dirs = append(dirs,
		filepath.Join(".", "target", "relfast"),
		filepath.Join(".", "target", "release"),
		filepath.Join(".", "target", "debug"),
	)
	return dirs
}

// Find looks for name (and optional aliases) in SearchDirs then PATH.
func Find(name string, aliases ...string) (string, error) {
	names := append([]string{name}, aliases...)
	for _, dir := range SearchDirs() {
		for _, n := range names {
			p := filepath.Join(dir, n)
			if st, err := os.Stat(p); err == nil && !st.IsDir() && st.Mode()&0o111 != 0 {
				abs, err := filepath.Abs(p)
				if err != nil {
					return p, nil
				}
				return abs, nil
			}
			// Some builds lack +x briefly; still accept regular files.
			if st, err := os.Stat(p); err == nil && !st.IsDir() {
				abs, _ := filepath.Abs(p)
				return abs, nil
			}
		}
	}
	for _, n := range names {
		if p, err := exec.LookPath(n); err == nil {
			return p, nil
		}
	}
	return "", fmt.Errorf("rust engine binary %q not found (set DT_ENGINE_DIR or build with cargo)", name)
}

// Run replaces the current process's stdio with the Rust binary.
func Run(name string, aliases []string, args []string) error {
	bin, err := Find(name, aliases...)
	if err != nil {
		return err
	}
	cmd := exec.Command(bin, args...)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	cmd.Env = os.Environ()
	if err := cmd.Run(); err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			os.Exit(ee.ExitCode())
		}
		return err
	}
	return nil
}

// SimulateRequest is one JSONL line for the Rust simulate binary.
type SimulateRequest struct {
	ID          string          `json:"id"`
	Minutes     int             `json:"minutes"`
	Fast        int             `json:"fast"`
	Slow        int             `json:"slow"`
	Exits       json.RawMessage `json:"exits,omitempty"`
	SizeBTC     *float64        `json:"size_btc,omitempty"`
	Leverage    uint32          `json:"leverage,omitempty"`
	FromMs      *int64          `json:"from_ms,omitempty"`
	ToMs        *int64          `json:"to_ms,omitempty"`
	Logged      bool            `json:"logged,omitempty"`
	RecordFills bool            `json:"record_fills,omitempty"`
}

// ClosedTrade is a closed trade from simulate with logged=true.
type ClosedTrade struct {
	ExitMs   int64   `json:"exit_ms"`
	EntryBar uint32  `json:"entry_bar"`
	EntryMs  int64   `json:"entry_ms"`
	Net      float32 `json:"net"`
}

// SimulateResponse is one JSONL response line.
type SimulateResponse struct {
	ID     string        `json:"id"`
	Closed []ClosedTrade `json:"closed"`
	Outcome json.RawMessage `json:"outcome"`
}

// SimulateArgs are CLI flags for the Rust simulate binary.
type SimulateArgs struct {
	Start        string
	End          string
	SizeBTC      float64
	BalanceUSDT  float64
	Config       string
	Data         string
	Jobs         int
}

// BatchSimulate runs Rust simulate once: JSONL requests on stdin → responses.
func BatchSimulate(args SimulateArgs, reqs []SimulateRequest) ([]SimulateResponse, error) {
	bin, err := Find("simulate")
	if err != nil {
		return nil, err
	}
	cli := []string{"--start", args.Start, "--end", args.End}
	if args.SizeBTC > 0 {
		cli = append(cli, "--size-btc", fmt.Sprintf("%g", args.SizeBTC))
	}
	if args.BalanceUSDT > 0 {
		cli = append(cli, "--balance-usdt", fmt.Sprintf("%g", args.BalanceUSDT))
	}
	if args.Config != "" {
		cli = append(cli, "--config", args.Config)
	}
	if args.Data != "" {
		cli = append(cli, "--data", args.Data)
	}
	if args.Jobs > 0 {
		cli = append(cli, "--jobs", fmt.Sprintf("%d", args.Jobs))
	}
	cmd := exec.Command(bin, cli...)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	enc := json.NewEncoder(stdin)
	for _, r := range reqs {
		if err := enc.Encode(r); err != nil {
			_ = stdin.Close()
			_ = cmd.Wait()
			return nil, err
		}
	}
	_ = enc.Encode(map[string]bool{"flush": true})
	_ = stdin.Close()
	if err := cmd.Wait(); err != nil {
		return nil, fmt.Errorf("simulate failed: %w\n%s", err, strings.TrimSpace(stderr.String()))
	}
	var out []SimulateResponse
	sc := bufio.NewScanner(&stdout)
	sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	for sc.Scan() {
		line := bytes.TrimSpace(sc.Bytes())
		if len(line) == 0 {
			continue
		}
		var resp SimulateResponse
		if err := json.Unmarshal(line, &resp); err != nil {
			return nil, fmt.Errorf("bad simulate response: %w (%s)", err, truncate(string(line), 200))
		}
		out = append(out, resp)
	}
	if err := sc.Err(); err != nil {
		return nil, err
	}
	return out, nil
}

// StreamSimulate keeps a long-lived simulate process and sends requests.
type StreamSimulate struct {
	cmd    *exec.Cmd
	stdin  io.WriteCloser
	stdout *bufio.Scanner
	stderr bytes.Buffer
}

func StartSimulate(args SimulateArgs) (*StreamSimulate, error) {
	bin, err := Find("simulate")
	if err != nil {
		return nil, err
	}
	cli := []string{"--start", args.Start, "--end", args.End}
	if args.SizeBTC > 0 {
		cli = append(cli, "--size-btc", fmt.Sprintf("%g", args.SizeBTC))
	}
	if args.BalanceUSDT > 0 {
		cli = append(cli, "--balance-usdt", fmt.Sprintf("%g", args.BalanceUSDT))
	}
	if args.Config != "" {
		cli = append(cli, "--config", args.Config)
	}
	if args.Data != "" {
		cli = append(cli, "--data", args.Data)
	}
	if args.Jobs > 0 {
		cli = append(cli, "--jobs", fmt.Sprintf("%d", args.Jobs))
	}
	cmd := exec.Command(bin, cli...)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return nil, err
	}
	s := &StreamSimulate{cmd: cmd, stdin: stdin}
	cmd.Stderr = &s.stderr
	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	s.stdout = sc
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *StreamSimulate) Batch(reqs []SimulateRequest) ([]SimulateResponse, error) {
	enc := json.NewEncoder(s.stdin)
	for _, r := range reqs {
		if err := enc.Encode(r); err != nil {
			return nil, err
		}
	}
	if err := enc.Encode(map[string]bool{"flush": true}); err != nil {
		return nil, err
	}
	out := make([]SimulateResponse, 0, len(reqs))
	for len(out) < len(reqs) {
		if !s.stdout.Scan() {
			err := s.stdout.Err()
			if err == nil {
				err = fmt.Errorf("simulate closed early (%s)", strings.TrimSpace(s.stderr.String()))
			}
			return nil, err
		}
		line := bytes.TrimSpace(s.stdout.Bytes())
		if len(line) == 0 {
			continue
		}
		var resp SimulateResponse
		if err := json.Unmarshal(line, &resp); err != nil {
			return nil, err
		}
		out = append(out, resp)
	}
	return out, nil
}

func (s *StreamSimulate) Close() error {
	_ = s.stdin.Close()
	return s.cmd.Wait()
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
