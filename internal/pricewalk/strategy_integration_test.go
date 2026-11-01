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

func TestRollingRetestsGridEachTrade(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	// Option B: trades start AFTER tradeEnd origin — need bars past tradeEnd.
	minutes := synthMinutes(320, barMs, start)
	h1 := pricemodel.Model{Alphas: [4]float64{3.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{2.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{1.0, 1.5, 2.0} // must be ≥ err_band 1%
	args.PriceTargets = []float64{0.1, 0.5, 1.0}
	args.PriceLeverage = []uint32{1, 2, 3}
	args.PriceRisk = []float64{5}
	tradeEnd := start + 200*barMs
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+40*barMs, tradeEnd, 0.0005, 0.0002, 40)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	sum, _ := out["summary"].(map[string]any)
	t.Logf("trades=%d pnl=%v mode=%v combos=%v", n, out["total_pnl"], sum["pick_mode"], sum["grid_combos"])
	if sum["pick_mode"] != "rolling-predicted" {
		t.Fatalf("mode=%v", sum["pick_mode"])
	}
	if sum["exits_mode"] != "rolling_retest_grid" {
		t.Fatalf("exits_mode=%v", sum["exits_mode"])
	}
	if n == 0 {
		t.Fatalf("expected FORWARD trades after origin; skips=%v", out["skips"])
	}
	trades, _ := out["trades"].([]map[string]any)
	prevEnd := 0
	labels := map[string]int{}
	minEntryBar := 0
	if oos, ok := out["oos_bars"].([]int); ok && len(oos) == 2 {
		minEntryBar = oos[0]
	}
	for _, tr := range trades {
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		// Option B: only after FORWARD origin (oos_lo).
		if eb < minEntryBar {
			t.Fatalf("option B must not trade before FORWARD oos_lo=%d; entry_bar=%d", minEntryBar, eb)
		}
		if eb < prevEnd {
			t.Fatalf("overlap: entry %d < prevEnd %d", eb, prevEnd)
		}
		prevEnd = eb + eh
		lab, _ := tr["oracle_exits"].(string)
		labels[lab]++
		note, _ := tr["oracle_note"].(string)
		if note == "" || len(note) < 10 {
			t.Fatalf("expected re-test note, got %q", note)
		}
	}
	t.Logf("exit labels used: %v", labels)
}

func TestRollingSkipsWhenNoProfitableCombo(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	var minutes []candles.Candle
	px := 100_000.0
	for i := 0; i < 80; i++ {
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open: px, High: px * 1.0001, Low: px * 0.9999, Close: px,
			})
		}
	}
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 20; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.PriceStops = []float64{0.5, 1.0}
	args.PriceTargets = []float64{0.5, 1.0} // TP too far for flat path
	args.PriceLeverage = []uint32{1}
	// tradeEnd mid-range; remaining bars are FORWARD segment (flat → 0 trades).
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+10*barMs, start+40*barMs, 0.0005, 0.0002, 20)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	skips, _ := out["skips"].(map[string]any)
	t.Logf("flat: trades=%d skips=%v", n, skips)
	if n != 0 {
		t.Fatalf("flat path must not invent profitable combos, got %d", n)
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

func TestExitsFromPredictedPathHighWRShape(t *testing.T) {
	e := exitsFromPredictedPath(0.40, 0.05, 0.01, 0.0005, 0.0002)
	tp := e.Targets[0][0]
	if tp < 0.2 || tp > 0.40 {
		t.Fatalf("TP should be ~70%% of 0.40 peak, got %v", tp)
	}
	if e.StopPct > 1.05+1e-9 {
		t.Fatalf("SL capped near err band, got %v", e.StopPct)
	}
}

func TestH1ExtrapSpan(t *testing.T) {
	p := h1ExtrapolatedPath(10, 100, true, 0.002)
	if len(p) != 10 || p[9] < 101.9 {
		t.Fatalf("%v", p)
	}
}

func TestRollingGridFindsTinyTP(t *testing.T) {
	// Small predicted peak (0.15%) — grid with 0.1 TP should still find mid>0.
	entry := 100.0
	path := make([]float64, 20)
	for i := range path {
		path[i] = entry * 1.0015 // +0.15% flat level
	}
	grid := exitCandidatesGrid([]float64{0.5, 1.0}, []float64{0.1, 0.5})
	steps := sizeForLeverage(500, 1, entry)
	found := false
	for _, ex := range grid {
		mid := simulateExitsOnPath(true, entry, ex, steps, path, 0.0005, 0.0002)
		if mid.PnL > 0 && mid.Reason == "tp" {
			found = true
			t.Logf("winner %s pnl=%.4f", ex.Label(), mid.PnL)
			break
		}
	}
	if !found {
		t.Fatal("0.1 TP should win on +0.15% predicted level")
	}
}

func TestRejectsStopInsideErrBand(t *testing.T) {
	// The screenshot bug: SL 0.1% × lev 10 wins absolute $ on a tiny pred move,
	// then actual noise stops out (−9.99). With err_band=1%, stop < 1% must lose.
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	minutes := synthMinutes(200, barMs, start) // bars after tradeEnd for option B
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 30; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{2.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{0.1, 0.5, 1.0, 1.5} // includes illegal 0.1 under err band
	args.PriceTargets = []float64{0.1, 0.5, 1.0}
	args.PriceLeverage = []uint32{1, 5, 10}
	args.PriceRisk = []float64{5}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+20*barMs, start+100*barMs, 0.0005, 0.0002, 30)
	if err != nil {
		t.Fatal(err)
	}
	trades, _ := out["trades"].([]map[string]any)
	for _, tr := range trades {
		sl, _ := tr["stop_pct"].(float64)
		lev, _ := tr["leverage"].(float64)
		if sl+1e-9 < 1.0 {
			t.Fatalf("stop %.2f%% < err_band 1%% (lev=%v) — screenshot bug regress", sl, lev)
		}
		if lev > 5+1e-9 && sl >= 1.0-1e-9 {
			// 1%×10 = 10% risk > max_risk 5% should already be blocked; belt+suspenders
			risk := sl * lev
			if risk > 5+1e-9 {
				t.Fatalf("risk %.2f%% > 5%% with lev=%v sl=%v", risk, lev, sl)
			}
		}
	}
}
