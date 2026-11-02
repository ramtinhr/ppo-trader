package pricewalk

import (
	"math"
	"strings"
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/exits"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

func synthMinutes(nBars int, barMs, start int64) []candles.Candle {
	// Steady up-drift with tight wicks so recent_range stays small vs a strong OLS peak
	// (formula sits out when tape noise ≫ predicted peak).
	var minutes []candles.Candle
	px := 100_000.0
	for i := 0; i < nBars; i++ {
		o := px
		c := px * 1.004 // +0.4%/bar trend
		hi, lo := c*1.0002, o*0.9998
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open:   o, High: hi, Low: lo, Close: c,
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

func TestTimeoutLossCappedAtStop(t *testing.T) {
	// Path never hits TP 0.5% nor quite the old MTM-only timeout; ends underwater.
	// Must close at stop (1%), not a worse mark-to-market.
	entry := 100.0
	path := make([]float64, 10)
	for i := range path {
		path[i] = entry * 1.005 // adverse for short… use long underwater:
		path[i] = entry * 0.995 // −0.5% — inside 1% stop, old code timed out at −0.5%*lev
	}
	ex := exits.Exits{StopPct: 1.0, Targets: [][2]float64{{0.5, 1}}}
	steps := sizeForLeverage(500, 5, entry)
	sim := simulateExitsOnPath(true, entry, ex, steps, path, 0.0005, 0.0002)
	if sim.Reason != "stop" {
		t.Fatalf("losing timeout must become stop, got %s pnl=%.2f", sim.Reason, sim.PnL)
	}
	// Stop PnL roughly −1% * notional; should not be a naked timeout bleed.
	if sim.PnL >= 0 {
		t.Fatalf("expected stop loss, pnl=%.2f", sim.PnL)
	}
}

func TestTruncatePathUntilRelErr(t *testing.T) {
	pred := []float64{100, 100.5, 101, 110} // last blows 1%
	act := []float64{100, 100.4, 100.8, 100.9}
	got := truncatePathUntilRelErr(pred, act, 0.01)
	if len(got) != 3 {
		t.Fatalf("want 3 under 1%%, got %d %v", len(got), got)
	}
	if len(truncatePathUntilRelErr(pred[:1], []float64{102}, 0.01)) != 0 {
		t.Fatal("immediate breach must yield empty path")
	}
}

func TestRollingMonthPredictTradeRefresh(t *testing.T) {
	// Confirmed loop: multi-trade append/refresh + N/H fields + under-1% path source.
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	minutes := synthMinutes(240, barMs, start)
	h1 := pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{1.0, 1.5, 2.0}
	args.PriceTargets = []float64{0.1, 0.5, 1.0}
	args.PriceLeverage = []uint32{1, 2, 3}
	args.PriceRisk = []float64{5}
	tradeStart := start + 40*barMs
	tradeEnd := start + 200*barMs
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		tradeStart, tradeEnd, 0.0005, 0.0002, 40)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	if n < 3 {
		t.Fatalf("append/re-validate loop must refresh after each exit, got %d trades skips=%v", n, out["skips"])
	}
	sum, _ := out["summary"].(map[string]any)
	if _, ok := sum["lookback_n"]; !ok {
		t.Fatalf("summary missing lookback_n: %v", sum)
	}
	trades, _ := out["trades"].([]map[string]any)
	prevEnd := 0
	for _, tr := range trades {
		predAt, _ := tr["predict_at"].(string)
		if predAt == "" {
			t.Fatalf("missing predict_at on trade %+v", tr)
		}
		if _, ok := tr["lookback_n"]; !ok {
			t.Fatalf("trade missing lookback_n: %+v", tr)
		}
		if _, ok := tr["horizon_h"]; !ok {
			t.Fatalf("trade missing horizon_h: %+v", tr)
		}
		note, _ := tr["oracle_note"].(string)
		if note == "" || !strings.Contains(note, "formula") || !strings.Contains(note, "N=") {
			t.Fatalf("note should mention N + formula, got %q", note)
		}
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		if eb < prevEnd {
			t.Fatalf("overlap entry %d < prevEnd %d", eb, prevEnd)
		}
		prevEnd = eb + eh
		src, _ := tr["path_source"].(string)
		if src != "path_formula_tp_sl_lev" {
			t.Fatalf("path_source=%q want path_formula_tp_sl_lev", src)
		}
	}
}

func TestRollingRetestsGridEachTrade(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	minutes := synthMinutes(240, barMs, start)
	h1 := pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{1.0, 1.5, 2.0} // must be ≥ err_band 1%
	args.PriceTargets = []float64{0.1, 0.5, 1.0}
	args.PriceLeverage = []uint32{1, 2, 3}
	args.PriceRisk = []float64{5}
	tradeStart := start + 40*barMs
	tradeEnd := start + 200*barMs
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		tradeStart, tradeEnd, 0.0005, 0.0002, 40)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	sum, _ := out["summary"].(map[string]any)
	t.Logf("trades=%d pnl=%v mode=%v combos=%v", n, out["total_pnl"], sum["pick_mode"], sum["grid_combos"])
	if sum["pick_mode"] != "rolling-predicted" {
		t.Fatalf("mode=%v", sum["pick_mode"])
	}
	if sum["exits_mode"] != "path_formula" {
		t.Fatalf("exits_mode=%v", sum["exits_mode"])
	}
	if n == 0 {
		t.Fatalf("expected OOS TRADE-month trades; skips=%v", out["skips"])
	}
	trades, _ := out["trades"].([]map[string]any)
	prevEnd := 0
	labels := map[string]int{}
	oos, _ := out["oos_bars"].([]int)
	for _, tr := range trades {
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		if len(oos) == 2 && (eb < oos[0] || eb >= oos[1]) {
			t.Fatalf("trade outside TRADE window: entry_bar=%d oos=%v", eb, oos)
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

func TestLiveSitsOutFlatRawPath(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	var minutes []candles.Candle
	px := 100_000.0
	for i := 0; i < 120; i++ {
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open: px, High: px * 1.0001, Low: px * 0.9999, Close: px,
			})
		}
	}
	// Zero alphas → flat raw path. Live bot must sit out (no h1 invent).
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 30; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.PriceStops = []float64{1.0, 1.5}
	args.PriceTargets = []float64{0.1, 0.5}
	args.PriceLeverage = []uint32{1, 2, 10}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+10*barMs, start+80*barMs, 0.0005, 0.0002, 20)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	skips, _ := out["skips"].(map[string]any)
	t.Logf("flat live: trades=%d skips=%v", n, skips)
	if n != 0 {
		t.Fatalf("live bot must sit out flat raw OLS, got %d trades", n)
	}
	flat, _ := skips["flat"].(int)
	if flat == 0 {
		t.Fatalf("expected flat skips, got %v", skips)
	}
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
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+10*barMs, start+70*barMs, 0.0005, 0.0002, 20)
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
	// peak 1.0% with quiet adverse → TP≈0.70, SL from min_stop/adverse (formula).
	e := exitsFromPredictedPath(1.0, 0.05, 0.01, 0.0005, 0.0002)
	tp := e.Targets[0][0]
	if tp < 0.6 || tp > 1.0 {
		t.Fatalf("TP should be ~70%% of 1.0 peak, got %v", tp)
	}
	if e.StopPct < PathMinStopPct-1e-9 || e.StopPct > tp/PathMinTpSlRatio+0.2 {
		t.Fatalf("SL out of formula band: tp=%.3f sl=%.3f", tp, e.StopPct)
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

func TestPathFormulaTPAndSLFromPeakAndNoise(t *testing.T) {
	// rangePct here = mean per-bar range (noise), not window high−low.
	ex, ok := exitsFromPathFormula(2.5, 0.05, 0.80, 0.0005, 0.0002)
	if !ok {
		t.Fatal("expected formula ok on 2.5% peak")
	}
	tp := firstTargetPct(ex)
	if math.Abs(tp-2.5*PathTakeFrac) > 0.05 {
		t.Fatalf("TP should track peak×take_frac, got %.3f (sl=%.3f)", tp, ex.StopPct)
	}
	// mean bar range 0.80 × 0.5 = 0.40 → SL ≈ 0.40
	if ex.StopPct < 0.30 || ex.StopPct > 0.55 {
		t.Fatalf("SL should follow bar-range noise ~0.40, got %.3f", ex.StopPct)
	}
	ex2, ok2 := exitsFromPathFormula(2.5, 0.05, 1.60, 0.0005, 0.0002)
	if !ok2 {
		t.Fatal("expected formula ok on wider bar range")
	}
	if math.Abs(ex2.StopPct-ex.StopPct) < 0.1 {
		t.Fatalf("SL must change with bar-range noise: %.3f vs %.3f", ex.StopPct, ex2.StopPct)
	}
	if _, ok := exitsFromPathFormula(0.20, 0, 1.5, 0.0005, 0.0002); ok {
		t.Fatal("sucker peak vs fat bar-range must sit out")
	}
}

func TestBestLevCapsRiskNotMaxMidPnL(t *testing.T) {
	entry := 100.0
	path := make([]float64, 16)
	for i := range path {
		path[i] = entry * 1.02
	}
	ex := exits.Exits{StopPct: 0.25, Targets: [][2]float64{{1.4, 1}}}
	adv := adversePath(path, true, 0.01)
	// maxRisk 5% would allow 0.25×20 — but PathTargetRiskPct=1% → lev ≤ 4.
	best := bestLevOnPath(true, entry, ex, path, adv, []uint32{1, 2, 3, 4, 5, 6, 7, 8, 9, 10}, 500, 5, 0.0005, 0.0002)
	if best == nil {
		t.Fatal("expected lev pick")
	}
	if best.RiskPct > PathTargetRiskPct+1e-9 {
		t.Fatalf("risk %.2f%% > target %.2f%% (lev=%.0f) — this was the −$17 stop bug", best.RiskPct, PathTargetRiskPct, best.Lev)
	}
	stopLossUSD := 500 * best.RiskPct / 100
	if stopLossUSD > 6 {
		t.Fatalf("stop $ loss budget too fat: $%.2f with lev=%.0f sl=%.2f", stopLossUSD, best.Lev, best.Exits.StopPct)
	}
	t.Logf("lev=%.0f risk=%.2f%% mid=%.2f stop≈$%.2f", best.Lev, best.RiskPct, best.PathPnL, stopLossUSD)
}
