package pricewalk

import (
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/exits"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

func synthMinutes(nBars int, barMs, start int64) []candles.Candle {
	var minutes []candles.Candle
	px := 100_000.0
	for i := 0; i < nBars; i++ {
		block := i / 20
		up := block%2 == 0
		o := px
		c := px * 0.998
		if up {
			c = px * 1.002
		}
		h, l := c, o
		if c < o {
			h, l = o, c
		}
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open:   o, High: h * 1.0005, Low: l * 0.9995, Close: c,
			})
		}
		px = c
	}
	return minutes
}

func TestExitGridIncludesAsymmetric(t *testing.T) {
	grid := exitCandidatesGrid([]float64{0.9, 1.5}, []float64{0.1, 0.5, 1.0})
	want := map[string]bool{
		exits.Exits{StopPct: 0.9, Targets: [][2]float64{{0.1, 1}}}.Label(): false,
		exits.Exits{StopPct: 0.9, Targets: [][2]float64{{0.5, 1}}}.Label(): false,
		exits.Exits{StopPct: 1.5, Targets: [][2]float64{{0.1, 1}}}.Label(): false,
	}
	for _, e := range grid {
		if _, ok := want[e.Label()]; ok {
			want[e.Label()] = true
		}
	}
	for lab, ok := range want {
		if !ok {
			t.Fatalf("missing combo %s in %d exits", lab, len(grid))
		}
	}
}

func TestRollingPredictedAlwaysTrades(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	minutes := synthMinutes(240, barMs, start)
	h1 := pricemodel.Model{Alphas: [4]float64{3.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		// Mild upward multi-h path so peak > 0.
		models = append(models, pricemodel.Model{Alphas: [4]float64{0.5, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceLeverage = []uint32{1, 2, 3}
	args.PriceRisk = []float64{5}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+40*barMs, start+220*barMs, 0.0005, 0.0002, 40)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	sum, _ := out["summary"].(map[string]any)
	t.Logf("trades=%d pnl=%v mode=%v win_rate=%v", n, out["total_pnl"], sum["pick_mode"], out["win_rate"])
	if n == 0 {
		t.Fatalf("rolling bot must place trades on every free origin; skips=%v", out["skips"])
	}
	if sum["pick_mode"] != "rolling-predicted" {
		t.Fatalf("mode=%v", sum["pick_mode"])
	}
	// Sequential: each trade entry_bar >= previous exit.
	trades, _ := out["trades"].([]map[string]any)
	prevEnd := 0
	for _, tr := range trades {
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		if eb < prevEnd {
			t.Fatalf("overlap: entry %d < prevEnd %d", eb, prevEnd)
		}
		prevEnd = eb + eh
	}
}

func TestExitsFromPredictedPathHighWRShape(t *testing.T) {
	e := exitsFromPredictedPath(0.40, 0.05, 0.01, 0.0005, 0.0002)
	tp := e.Targets[0][0]
	if tp < 0.2 || tp > 0.40 {
		t.Fatalf("TP should be ~70%% of 0.40 peak, got %v", tp)
	}
	// SL capped near err band and not huge vs TP (80% WR design).
	if e.StopPct > 1.05+1e-9 {
		t.Fatalf("SL should be capped near err band, got %v", e.StopPct)
	}
	if e.StopPct/tp > (PathWinRateTarget/(1-PathWinRateTarget))+1e-9 {
		t.Fatalf("SL/TP=%v too wide for %.0f%% WR target", e.StopPct/tp, PathWinRateTarget*100)
	}
}

func TestPathDirectionPicksFavorableSide(t *testing.T) {
	up := []float64{100.2, 100.5, 100.1}
	long, peak := pathDirection(up, 100)
	if !long || peak < 0.4 {
		t.Fatalf("long=%v peak=%v", long, peak)
	}
	dn := []float64{99.8, 99.4, 99.9}
	long, peak = pathDirection(dn, 100)
	if long || peak < 0.5 {
		t.Fatalf("short expected: long=%v peak=%v", long, peak)
	}
}

func TestRollingHighWinRateWhenPathMatchesTape(t *testing.T) {
	// Tape follows a steady uptrend; multi-h models predict up → TP should hit often.
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	var minutes []candles.Candle
	px := 100_000.0
	for i := 0; i < 300; i++ {
		o := px
		c := px * 1.0015 // +0.15%/bar — larger than PathMinTP
		h, l := c*1.0002, o*0.9998
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open:   o, High: h, Low: l, Close: c,
			})
		}
		px = c
	}
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 30; h++ {
		// Strong prev_return → predicted path continues up with the tape.
		models = append(models, pricemodel.Model{Alphas: [4]float64{2.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.BalanceUSDT = 500
	args.PriceLeverage = []uint32{1}
	args.PriceRisk = []float64{5}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+20*barMs, start+280*barMs, 0.0005, 0.0002, 30)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	wr, _ := out["win_rate"].(float64)
	t.Logf("trending tape: trades=%d win_rate=%.1f%% pnl=%v", n, wr, out["total_pnl"])
	if n < 5 {
		t.Fatalf("expected several rolling trades, got %d", n)
	}
	if wr < 80 {
		t.Fatalf("when path matches tape, win rate should be ≥80%%, got %.1f%%", wr)
	}
}

func TestH1ExtrapSpan(t *testing.T) {
	p := h1ExtrapolatedPath(10, 100, true, 0.002)
	if len(p) != 10 || p[9] < 101.9 {
		t.Fatalf("%v", p)
	}
}
