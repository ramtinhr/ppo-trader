// Command dt is the product CLI for dynamic-trader: Rust engine wrappers + Go browse / walk-forward / report.
package main

import (
	"fmt"
	"os"
	"strconv"

	"github.com/ramtin/dynamic-trader/internal/browse"
	"github.com/ramtin/dynamic-trader/internal/engine"
	"github.com/ramtin/dynamic-trader/internal/report"
	"github.com/ramtin/dynamic-trader/internal/walkforward"
)

const usage = `dt — dynamic-trader product CLI (Go) over the Rust engine.

Engine passthrough (finds binaries via $DT_ENGINE_DIR, ./target/relfast|release|debug, PATH):
  dt download [args...]
  dt scan [args...]
  dt simulate [args...]
  dt verify [args...]
  dt backtest [args...]
  dt tv-check [args...]

Product:
  dt browse --scan DIR [--port N] [--layout window|month|auto] [--min-drop USDT]
  dt walk-forward --scan DIR [window_nm score-path flags…]
  dt report --json PATH
  dt price-walk [args…]   # wraps window_nm_walk_forward --path price

Build:  go build -o dt ./cmd/dt
Engine: cargo build --profile relfast --bin scan --bin simulate --bin download …
`

func main() {
	if len(os.Args) < 2 || os.Args[1] == "-h" || os.Args[1] == "--help" {
		fmt.Print(usage)
		return
	}
	cmd := os.Args[1]
	args := os.Args[2:]
	var err error
	switch cmd {
	case "download":
		err = engine.Run("download", []string{"download_okx_trades"}, args)
	case "scan":
		err = engine.Run("scan", nil, args)
	case "simulate":
		err = engine.Run("simulate", nil, args)
	case "verify":
		err = engine.Run("verify", nil, args)
	case "backtest":
		err = engine.Run("backtest", nil, args)
	case "tv-check":
		err = engine.Run("tv_check", nil, args)
	case "browse":
		err = runBrowse(args)
	case "walk-forward":
		err = runWalkForward(args)
	case "report":
		err = runReport(args)
	case "price-walk":
		err = engine.Run("window_nm_walk_forward", nil, append([]string{"--path", "price"}, args...))
	default:
		err = fmt.Errorf("unknown command %q\n\n%s", cmd, usage)
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "dt: %v\n", err)
		os.Exit(1)
	}
}

func runBrowse(argv []string) error {
	opts := browse.Options{Layout: browse.LayoutAuto, Port: 0, MinDrop: 5}
	for i := 0; i < len(argv); i++ {
		flag := argv[i]
		if flag == "-h" || flag == "--help" {
			fmt.Println("Usage: dt browse --scan DIR [--port N] [--layout window|month|auto] [--min-drop USDT]")
			return nil
		}
		need := func() (string, error) {
			if i+1 >= len(argv) {
				return "", fmt.Errorf("%s needs a value", flag)
			}
			i++
			return argv[i], nil
		}
		switch flag {
		case "--scan":
			v, err := need()
			if err != nil {
				return err
			}
			opts.ScanDir = v
		case "--port":
			v, err := need()
			if err != nil {
				return err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return err
			}
			opts.Port = n
		case "--layout":
			v, err := need()
			if err != nil {
				return err
			}
			switch v {
			case "window":
				opts.Layout = browse.LayoutWindow
			case "month":
				opts.Layout = browse.LayoutMonth
			case "auto":
				opts.Layout = browse.LayoutAuto
			default:
				return fmt.Errorf("--layout must be window|month|auto")
			}
		case "--min-drop":
			v, err := need()
			if err != nil {
				return err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil {
				return err
			}
			opts.MinDrop = x
		default:
			return fmt.Errorf("unknown option %s", flag)
		}
	}
	if opts.ScanDir == "" {
		return fmt.Errorf("--scan is required")
	}
	return browse.Serve(opts)
}

func runWalkForward(argv []string) error {
	args, err := walkforward.ParseArgs(argv)
	if walkforward.IsHelp(err) {
		fmt.Println(`Usage: dt walk-forward --scan DIR [flags]

Score-path N/M walk-forward (Go). Calls Rust simulate for finalist closed trades.
Flags match window_nm_walk_forward score path: --lookback-days, --train-trades,
--min-fit-pairs, --reserve-trade-days, --require-fit, --max-rel-err, --max-finalists,
--memory-gb, --threads, --config, --data, --balance-usdt, --html-only, --rerank, …
For the price path use: dt price-walk …`)
		return nil
	}
	if err != nil {
		return err
	}
	return walkforward.RunScorePath(args)
}

func runReport(argv []string) error {
	var jsonPath string
	for i := 0; i < len(argv); i++ {
		flag := argv[i]
		if flag == "-h" || flag == "--help" {
			fmt.Println("Usage: dt report --json PATH")
			return nil
		}
		if flag == "--json" {
			if i+1 >= len(argv) {
				return fmt.Errorf("--json needs a value")
			}
			i++
			jsonPath = argv[i]
			continue
		}
		return fmt.Errorf("unknown option %s", flag)
	}
	if jsonPath == "" {
		return fmt.Errorf("--json is required")
	}
	return report.FromJSON(jsonPath)
}
