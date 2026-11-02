// Package pricewalk ports the candle OLS + oracle exit path from
// window_nm_walk_forward --path price.
package pricewalk

import (
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/ramtin/dynamic-trader/internal/score"
)

const (
	PriceHorizonAutoCap      = 500
	PriceSignalConfirmBars   = 4
	PriceMaxBreakevenWin     = 0.75
	PriceMaxImpliedRiskPct   = 5.0
	PriceRangeLookback       = 32
	PriceRangeStopFrac       = 0.5
	StepBTC                  = 0.0001
	ReplayBalance            = 1e9

	// Live predicted bot (mode 2) — confirmed IS/OOS loop:
	//  Origin at latest IS → discover lookback N + under-1% H from data →
	//  SCORE TP×SL×lev on expanding TRAIN lookback (most train PnL on actuals) →
	//  trade AFTER origin → APPEND actual entry→exit candles →
	//  re-validate H + re-score combo (stable expanding/rolling N; widen N only
	//  if H collapses) → repeat until trade_end (~1y with --trade-days 365).
	//  - sit out only when peak cannot clear fees
	//  - no invented / h1-extrapolated / span-floor paths
	//  - exits NOT static — re-scored every origin for most train gain
	PathTakeFrac      = 0.70
	PathMinPeakPct    = 0.08 // fee-area floor for a directional side
	PathMinStopPct    = 0.25
	PathSkewMaxLev    = 1.0
	PathMinTpSlRatio  = 0.50 // reject sucker TP≪SL
	PathLiveMaxLev    = 10.0
	PathWinRateTarget = 0.80
	PathMinHorizonH   = 4
	PathTargetRiskPct = 5.0 // stop×lev cap while scoring
	PathTrainSamples  = 32  // ~N train origins scored per combo
	DefaultTradeDays  = 30
)

var errHelp = errors.New("help")

func IsHelp(err error) bool { return errors.Is(err, errHelp) }

// Args are CLI flags for dt price-walk (scripts/price_iter.sh).
type Args struct {
	Scan             string
	LookbackDays     uint64
	TrainTrades      []int // unused by price path; accepted for flag parity
	TradeDays        uint64
	MinFitPairs      int
	ReserveTradeDays uint64
	FirstChoice      string // YYYY-MM-DD or empty
	RequireFit       bool
	MaxRelErr        float64
	MaxFitPairs      int
	Verify           int // ignored (no Nautilus on price path)
	BalanceUSDT      float64
	Config           string
	Data             string
	HTMLOnly         bool

	PriceBars         []BarSize
	PriceHorizon      int
	PriceHorizonAuto  bool
	// PriceStrategyMode: 0=off, 1=oracle (peek actuals), 2=predicted robust (±max_rel_err).
	PriceStrategyMode int
	PriceStops        []float64
	PriceTargets      []float64 // asymmetric TP grid (paired with every stop)
	PriceRisk         []float64
	PriceLeverage     []uint32
}

type BarSize struct {
	Label string
	BarMs int64
}

func DefaultArgs() Args {
	bars, _ := ParsePriceBars("5m,15m,1h,4h")
	// Full live grid: TP and SL 0.1..10, lev 1..10 (re-tested every trade).
	stops, _ := ParseRange("0.1-10/0.1")
	targets, _ := ParseRange("0.1-10/0.1")
	tt := make([]int, 0, 10)
	for k := 10; k >= 1; k-- {
		tt = append(tt, k*10)
	}
	return Args{
		Scan:             "data/windowscan/2026-09-01_2026-10-01_1d",
		LookbackDays:     30,
		TrainTrades:      tt,
		MinFitPairs:      2000,
		ReserveTradeDays: DefaultTradeDays,
		TradeDays:        DefaultTradeDays,
		RequireFit:       true,
		MaxRelErr:        score.MaxScoreRelErr,
		BalanceUSDT:      500,
		Config:           "config/testnet.toml",
		Data:             "data/okx/BTC-USDT-SWAP/archives",
		PriceBars:        bars,
		PriceHorizon:     PriceHorizonAutoCap,
		PriceHorizonAuto: true,
		PriceStops:       stops,
		PriceTargets:     targets,
		PriceRisk:        []float64{0.25, 0.5, 1, 2, 3, 5},
		// Leverage 1..10 re-tested every trade (0x is not a position).
		PriceLeverage: []uint32{1, 2, 3, 4, 5, 6, 7, 8, 9, 10},
		Verify:           1,
	}
}

// ParseArgs parses price-walk flags (without the program / subcommand name).
func ParseArgs(argv []string) (Args, error) {
	args := DefaultArgs()
	for i := 0; i < len(argv); i++ {
		flag := argv[i]
		if flag == "-h" || flag == "--help" {
			return args, errHelp
		}
		if flag == "--html-only" {
			args.HTMLOnly = true
			continue
		}
		// Score-path-only flags accepted & ignored for script compatibility.
		if flag == "--rerank" || flag == "--path" {
			if flag == "--path" {
				if i+1 >= len(argv) {
					return args, fmt.Errorf("%s needs a value", flag)
				}
				i++
				v := argv[i]
				if v != "price" && v != "both" {
					return args, fmt.Errorf("dt price-walk only supports --path price (got %q)", v)
				}
			}
			continue
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
				return args, err
			}
			args.Scan = v
		case "--lookback-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil || n == 0 {
				return args, fmt.Errorf("bad --lookback-days")
			}
			args.LookbackDays = n
		case "--train-trades":
			v, err := need()
			if err != nil {
				return args, err
			}
			parts := strings.Split(v, ",")
			args.TrainTrades = args.TrainTrades[:0]
			for _, p := range parts {
				p = strings.TrimSpace(p)
				if p == "" {
					continue
				}
				n, err := strconv.Atoi(p)
				if err != nil {
					return args, err
				}
				args.TrainTrades = append(args.TrainTrades, n)
			}
		case "--trade-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil {
				return args, err
			}
			args.TradeDays = n
		case "--min-fit-pairs":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil || n <= 0 {
				return args, fmt.Errorf("bad --min-fit-pairs")
			}
			args.MinFitPairs = n
		case "--reserve-trade-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil || n == 0 {
				return args, fmt.Errorf("bad --reserve-trade-days")
			}
			args.ReserveTradeDays = n
		case "--first-choice":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.FirstChoice = v
		case "--require-fit":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil || n > 1 || n < 0 {
				return args, fmt.Errorf("--require-fit must be 0 or 1")
			}
			args.RequireFit = n == 1
		case "--max-rel-err":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil || x <= 0 || x > 1 {
				return args, fmt.Errorf("--max-rel-err must be in (0, 1]")
			}
			args.MaxRelErr = x
		case "--max-fit-pairs":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return args, err
			}
			args.MaxFitPairs = n
		case "--verify":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return args, err
			}
			args.Verify = n
		case "--balance-usdt":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil {
				return args, err
			}
			args.BalanceUSDT = x
		case "--config":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.Config = v
		case "--data":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.Data = v
		case "--price-bars":
			v, err := need()
			if err != nil {
				return args, err
			}
			bars, err := ParsePriceBars(v)
			if err != nil {
				return args, err
			}
			args.PriceBars = bars
		case "--price-horizon":
			v, err := need()
			if err != nil {
				return args, err
			}
			if v == "auto" {
				args.PriceHorizon = PriceHorizonAutoCap
				args.PriceHorizonAuto = true
			} else {
				n, err := strconv.Atoi(v)
				if err != nil || n < 1 || n > PriceHorizonAutoCap {
					return args, fmt.Errorf("--price-horizon must be auto or 1..%d", PriceHorizonAutoCap)
				}
				args.PriceHorizon = n
				args.PriceHorizonAuto = false
			}
		case "--price-strategy":
			v, err := need()
			if err != nil {
				return args, err
			}
			switch strings.ToLower(strings.TrimSpace(v)) {
			case "0", "off", "none":
				args.PriceStrategyMode = 0
			case "1", "oracle":
				args.PriceStrategyMode = 1
			case "2", "predicted", "robust":
				args.PriceStrategyMode = 2
			default:
				return args, fmt.Errorf("--price-strategy must be 0|1|2 (off|oracle|predicted)")
			}
		case "--price-stops":
			v, err := need()
			if err != nil {
				return args, err
			}
			stops, err := parseStopsList(v)
			if err != nil {
				return args, err
			}
			args.PriceStops = stops
		case "--price-targets", "--price-tps":
			v, err := need()
			if err != nil {
				return args, err
			}
			tgts, err := parseStopsList(v)
			if err != nil {
				return args, fmt.Errorf("bad --price-targets: %w", err)
			}
			for _, t := range tgts {
				if t <= 0 || t >= 50 {
					return args, fmt.Errorf("--price-targets must be in (0, 50)")
				}
			}
			args.PriceTargets = tgts
		case "--price-risk":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.PriceRisk = args.PriceRisk[:0]
			for _, p := range strings.Split(v, ",") {
				p = strings.TrimSpace(p)
				if p == "" {
					continue
				}
				x, err := strconv.ParseFloat(p, 64)
				if err != nil || x <= 0 || x > 20 {
					return args, fmt.Errorf("--price-risk must be in (0, 20]")
				}
				args.PriceRisk = append(args.PriceRisk, x)
			}
			if len(args.PriceRisk) == 0 {
				return args, fmt.Errorf("--price-risk empty")
			}
		case "--price-leverage":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.PriceLeverage = args.PriceLeverage[:0]
			for _, p := range strings.Split(v, ",") {
				p = strings.TrimSpace(p)
				if p == "" {
					continue
				}
				n, err := strconv.ParseUint(p, 10, 32)
				if err != nil || n < 1 || n > 10 {
					return args, fmt.Errorf("--price-leverage must be whole numbers 1..10")
				}
				args.PriceLeverage = append(args.PriceLeverage, uint32(n))
			}
			if len(args.PriceLeverage) == 0 {
				return args, fmt.Errorf("--price-leverage empty")
			}
		// Ignored score-path flags (scripts may pass them).
		case "--max-finalists", "--memory-gb", "--threads", "--refine-exits",
			"--ladders", "--shares", "--breakeven", "--exit-alts", "--size-btc":
			if _, err := need(); err != nil {
				return args, err
			}
		default:
			return args, fmt.Errorf("unknown option %s", flag)
		}
	}
	if len(args.PriceBars) == 0 {
		return args, fmt.Errorf("--price-bars needs at least one bar size")
	}
	return args, nil
}

// ParsePriceBars parses "5m,15m,1h,4h" into unique (label, bar_ms).
func ParsePriceBars(s string) ([]BarSize, error) {
	var out []BarSize
	for _, raw := range strings.Split(s, ",") {
		t := strings.ToLower(strings.TrimSpace(raw))
		if t == "" {
			continue
		}
		var label string
		var minutes int64
		switch {
		case strings.HasSuffix(t, "h"):
			h, err := strconv.ParseInt(strings.TrimSuffix(t, "h"), 10, 64)
			if err != nil || h <= 0 {
				return nil, fmt.Errorf("bad bar %q", raw)
			}
			label = fmt.Sprintf("%dh", h)
			minutes = h * 60
		case strings.HasSuffix(t, "m"):
			m, err := strconv.ParseInt(strings.TrimSuffix(t, "m"), 10, 64)
			if err != nil || m <= 0 {
				return nil, fmt.Errorf("bad bar %q", raw)
			}
			if m%60 == 0 {
				label = fmt.Sprintf("%dh", m/60)
			} else {
				label = fmt.Sprintf("%dm", m)
			}
			minutes = m
		default:
			m, err := strconv.ParseInt(t, 10, 64)
			if err != nil || m <= 0 {
				return nil, fmt.Errorf("bad bar %q (use 15m or 1h)", raw)
			}
			if m%60 == 0 {
				label = fmt.Sprintf("%dh", m/60)
			} else {
				label = fmt.Sprintf("%dm", m)
			}
			minutes = m
		}
		if minutes < 1 || minutes > 24*60 {
			return nil, fmt.Errorf("bar %q out of range (1m … 24h)", raw)
		}
		barMs := minutes * 60_000
		dup := false
		for _, b := range out {
			if b.BarMs == barMs {
				dup = true
				break
			}
		}
		if dup {
			continue
		}
		out = append(out, BarSize{Label: label, BarMs: barMs})
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("no bar sizes in %q", s)
	}
	return out, nil
}

func parseStopsList(v string) ([]float64, error) {
	var stops []float64
	var err error
	if strings.Contains(v, "/") {
		stops, err = ParseRange(v)
		if err != nil {
			return nil, err
		}
	} else {
		for _, p := range strings.Split(v, ",") {
			p = strings.TrimSpace(p)
			if p == "" {
				continue
			}
			x, e := strconv.ParseFloat(p, 64)
			if e != nil {
				return nil, fmt.Errorf("bad --price-stops: %w", e)
			}
			stops = append(stops, x)
		}
	}
	for _, s := range stops {
		if s <= 0 || s >= 50 {
			return nil, fmt.Errorf("--price-stops must be in (0, 50)")
		}
	}
	if len(stops) == 0 {
		return nil, fmt.Errorf("--price-stops empty")
	}
	return stops, nil
}

// ParseRange parses "0.5-5/0.5" → [0.5, 1.0, …, 5.0].
func ParseRange(text string) ([]float64, error) {
	parts := strings.SplitN(text, "/", 2)
	if len(parts) != 2 {
		return nil, fmt.Errorf("expected from-to/step")
	}
	rangePart := parts[0]
	step, err := strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
	if err != nil {
		return nil, err
	}
	ab := strings.SplitN(rangePart, "-", 2)
	if len(ab) != 2 {
		return nil, fmt.Errorf("expected from-to/step")
	}
	from, err1 := strconv.ParseFloat(strings.TrimSpace(ab[0]), 64)
	to, err2 := strconv.ParseFloat(strings.TrimSpace(ab[1]), 64)
	if err1 != nil || err2 != nil {
		return nil, fmt.Errorf("bad range %q", text)
	}
	if !(0 < from && from <= to && step > 0) {
		return nil, fmt.Errorf("bad range %q", text)
	}
	n := int(((to - from) / step) + 0.5)
	out := make([]float64, 0, n+1)
	for k := 0; k <= n; k++ {
		v := ((from + float64(k)*step) * 1e6)
		out = append(out, float64(int64(v+0.5))/1e6)
	}
	return out, nil
}
