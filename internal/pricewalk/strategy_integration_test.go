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
	h1 := pricemodel.Model{Alphas: [4]float64{3.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{2.0, 0, 0, 0}})
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
		if note == "" || !strings.Contains(note, "append") || !strings.Contains(note, "N=") {
			t.Fatalf("note should mention N + append, got %q", note)
		}
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		if eb < prevEnd {
			t.Fatalf("overlap entry %d < prevEnd %d", eb, prevEnd)
		}
		prevEnd = eb + eh
		src, _ := tr["path_source"].(string)
		if src != "raw_ols_under1pct_H" {
			t.Fatalf("path_source=%q want raw_ols_under1pct_H", src)
		}
	}
}

func TestRollingRetestsGridEachTrade(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	minutes := synthMinutes(240, barMs, start)
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
	if sum["exits_mode"] != "live_retest_grid" {
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

func TestDynamicComboRejectsSuckerPayoff(t *testing.T) {
	// SL is no longer frozen at err_band 1%. Combos must keep TP/SL ≥ PathMinTpSlRatio
	// and risk≤5%; micro-TP vs fat SL is rejected (sit out), not forced to SL=1%.
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	minutes := synthMinutes(160, barMs, start)
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 30; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{2.0, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{0.1, 0.5, 1.0, 1.5, 2.0}
	args.PriceTargets = []float64{0.1, 0.5, 1.0}
	args.PriceLeverage = []uint32{1, 5, 10}
	args.PriceRisk = []float64{5}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+20*barMs, start+140*barMs, 0.0005, 0.0002, 30)
	if err != nil {
		t.Fatal(err)
	}
	trades, _ := out["trades"].([]map[string]any)
	stopsSeen := map[float64]int{}
	for _, tr := range trades {
		sl, _ := tr["stop_pct"].(float64)
		lev, _ := tr["leverage"].(float64)
		stopsSeen[sl]++
		if sl*lev > 5+1e-9 {
			t.Fatalf("risk %.2f%% > 5%% with lev=%v sl=%v", sl*lev, lev, sl)
		}
		tp := 0.0
		if ex, ok := tr["exits"].(map[string]any); ok {
			if tgts, ok := ex["targets"].([][2]float64); ok && len(tgts) > 0 {
				tp = tgts[0][0]
			}
		}
		if tp > 0 && sl > 0 && tp/sl+1e-12 < PathMinTpSlRatio {
			t.Fatalf("sucker TP/SL %.3f (tp=%.3f sl=%.3f) must be rejected", tp/sl, tp, sl)
		}
	}
	t.Logf("trades=%d stop_pct histogram=%v skips=%v", len(trades), stopsSeen, out["skips"])
}

func TestPickPrefersHigherGainOverMinStop(t *testing.T) {
	// On a strong predicted path, picker must not collapse every stop to the floor:
	// higher TP with matching SL can win on mid PnL.
	entry := 100.0
	path := make([]float64, 20)
	for i := range path {
		path[i] = entry * 1.025 // +2.5% peak
	}
	long := true
	peak := 2.5
	errBand := 0.01
	taker, maker := 0.0005, 0.0002
	balance := 500.0
	usable := []float64{0.5, 1.0, 1.5, 2.0}
	levs := []uint32{1, 2, 3}
	advShock := adversePath(path, long, errBand)
	advPct := pathAdversePct(path, entry, long)
	minStop := PathMinStopPct
	if adv := advPct*1.25 + 0.05; adv > minStop {
		minStop = adv
	}
	var best *pathPick
	for _, tp := range []float64{round2(peak * PathTakeFrac), round2(peak * 0.95)} {
		for _, stop := range usable {
			if stop+1e-12 < minStop || tp/stop+1e-12 < PathMinTpSlRatio {
				continue
			}
			ex := exits.Exits{StopPct: stop, Targets: [][2]float64{{tp, 1}}}
			for _, levU := range levs {
				lev := float64(levU)
				risk := impliedRiskPct(stop, lev)
				if risk > 5 {
					continue
				}
				steps := sizeForLeverage(balance, lev, entry)
				if steps <= 0 {
					continue
				}
				mid := simulateExitsOnPath(long, entry, ex, steps, path, taker, maker)
				if mid.PnL <= 0 || mid.Reason != "tp" {
					continue
				}
				advSim := simulateExitsOnPath(long, entry, ex, steps, advShock, taker, maker)
				cand := pathPick{
					Exits: ex, RiskPct: risk, Steps: steps, Lev: lev,
					PathPnL: mid.PnL, WorstPnL: advSim.PnL, PathR: mid.PnL / math.Max(balance*risk/100, 1e-9),
					LiquidDist: liquidDistancePct(stop, lev), ExitH: mid.ExitH,
				}
				if best == nil || mid.PnL > best.PathPnL+1e-9 ||
					(math.Abs(mid.PnL-best.PathPnL) <= 1e-9 && advSim.PnL > best.WorstPnL+1e-9) {
					cp := cand
					best = &cp
				}
			}
		}
	}
	if best == nil {
		t.Fatal("expected a combo on +2.5% path")
	}
	tp := firstTargetPct(best.Exits)
	if tp < 1.0 {
		t.Fatalf("expected a meaningful TP on +2.5%% peak, got tp=%.2f sl=%.2f", tp, best.Exits.StopPct)
	}
	if best.Exits.StopPct < PathMinStopPct-1e-9 {
		t.Fatalf("stop below floor: %v", best.Exits.StopPct)
	}
	t.Logf("picked %s lev=%.0f mid=%.2f adv=%.2f", best.Exits.Label(), best.Lev, best.PathPnL, best.WorstPnL)
}
