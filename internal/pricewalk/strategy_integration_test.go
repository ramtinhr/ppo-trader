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
		if note == "" || !strings.Contains(note, "train_score") || !strings.Contains(note, "re-find N=") {
			t.Fatalf("note should mention re-find N + train_score, got %q", note)
		}
		if _, ok := tr["train_pnl"]; !ok {
			t.Fatalf("trade missing train_pnl: %+v", tr)
		}
		eb, _ := tr["entry_bar"].(int)
		eh, _ := tr["exit_h"].(int)
		if eb < prevEnd {
			t.Fatalf("overlap entry %d < prevEnd %d", eb, prevEnd)
		}
		prevEnd = eb + eh
		src, _ := tr["path_source"].(string)
		if src != "train_score_tp_sl_lev" {
			t.Fatalf("path_source=%q want train_score_tp_sl_lev", src)
		}
	}
	sumSkips, _ := out["skips"].(map[string]any)
	nRefind, _ := sumSkips["n_rediscover"].(int)
	if nRefind < int(n) {
		t.Fatalf("expected ≥1 N re-find per trade, n_rediscover=%d trades=%d", nRefind, n)
	}
}

func TestEachTradeRepredictsFreshEntry(t *testing.T) {
	// After trade 1, next entry must use a later predict_at / entry_bar (fresh path).
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
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+40*barMs, start+200*barMs, 0.0005, 0.0002, 40)
	if err != nil {
		t.Fatal(err)
	}
	trades, _ := out["trades"].([]map[string]any)
	if len(trades) < 2 {
		t.Fatalf("need ≥2 trades to prove re-predict, got %d skips=%v", len(trades), out["skips"])
	}
	prevPred := ""
	prevEntry := 0
	for i, tr := range trades {
		predAt, _ := tr["predict_at"].(string)
		eb, _ := tr["entry_bar"].(int)
		ep, _ := tr["entry_price"].(float64)
		pc, _ := tr["predict_close"].(float64)
		if predAt == "" || eb < 2 || !(ep > 0) || !(pc > 0) {
			t.Fatalf("trade %d missing fresh predict/entry: %+v", i, tr)
		}
		if predAt == prevPred {
			t.Fatalf("trade %d reused predict_at %s — must re-predict", i, predAt)
		}
		if eb <= prevEntry {
			t.Fatalf("trade %d entry_bar %d ≤ prior %d", i, eb, prevEntry)
		}
		prevPred, prevEntry = predAt, eb
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
	if sum["exits_mode"] != "train_score" {
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

func TestExitsFromPredictedPathGeometry(t *testing.T) {
	e := exitsFromPredictedPath(1.5, 0.05, 0.01, 0.0005, 0.0002)
	tp := e.Targets[0][0]
	wantTP := 1.5 * PathTakeFrac
	if math.Abs(tp-wantTP) > 0.05 {
		t.Fatalf("TP want ≈%.2f got %v", wantTP, tp)
	}
	if e.StopPct+1e-12 < PathMinStopPct {
		t.Fatalf("SL below floor: %.3f", e.StopPct)
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

func TestPickExitsByTrainGainMostPnL(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	// Strong up-drift: train scoring should prefer a reachable TP + moderate SL/lev.
	minutes := synthMinutes(160, barMs, start)
	bars := candles.Aggregate(minutes, barMs)
	h1 := pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{8.0, 0, 0, 0}})
	}
	origin := 80
	if origin >= len(bars) {
		t.Fatalf("need bars, got %d", len(bars))
	}
	feat, ok := pricemodel.CandleFeatures(bars[origin-1], bars[origin-2].Close)
	if !ok {
		t.Fatal("features")
	}
	entry := bars[origin].Open
	raw := predictedCloses(models, bars[origin-1].Close, feat)
	pickPath := truncatePath(raw, 20)
	long, peak := pathDirection(pickPath, entry)
	if peak < 0.05 {
		t.Fatalf("need directional peak, got %.4f", peak)
	}
	best := pickExitsByTrainGain(
		bars, models, 10, origin, 20,
		long, entry, pickPath,
		[]float64{0.5, 1.0, 1.5, 2.0},
		[]float64{0.1, 0.5, 1.0, 1.5},
		[]uint32{1, 2, 3, 4, 5},
		500, 5, 0.08, 0.0005, 0.0002,
	)
	if best == nil {
		t.Fatal("expected train-scored combo")
	}
	if best.TrainN < 1 {
		t.Fatalf("train_n=%d", best.TrainN)
	}
	if best.Lev < 1 {
		t.Fatalf("lev=%.0f", best.Lev)
	}
	if best.RiskPct > PathTargetRiskPct+1e-9 {
		t.Fatalf("risk %.2f%% > %.2f%%", best.RiskPct, PathTargetRiskPct)
	}
	t.Logf("pick %s lev=%.0f train_pnl=%.2f n=%d mid=%.2f",
		best.Exits.Label(), best.Lev, best.TrainPnL, best.TrainN, best.PathPnL)
}

func TestScoreComboPrefersWinningTP(t *testing.T) {
	// Up to ~0.7% then dump through −1% — reachable 0.5 TP wins; unreachable 5% TP stops out.
	samples := []trainSample{
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.4, 100.7, 99.2, 99.0}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.3, 100.6, 99.1, 98.9}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.5, 100.65, 99.3, 99.0}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.35, 100.55, 99.0, 98.8}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.45, 100.7, 99.2, 99.1}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.5, 100.8, 99.0, 98.7}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.2, 100.6, 99.15, 99.0}},
		{Long: true, Entry: 100, Peak: 0.2, Act: []float64{100.4, 100.75, 99.05, 98.9}},
	}
	good := exits.Exits{StopPct: 1.0, Targets: [][2]float64{{0.5, 1}}}
	bad := exits.Exits{StopPct: 1.0, Targets: [][2]float64{{5.0, 1}}}
	gs := scoreComboOnSamples(samples, good, 3, 500, 5, 0.0005, 0.0002)
	bs := scoreComboOnSamples(samples, bad, 3, 500, 5, 0.0005, 0.0002)
	if gs.N < 5 || bs.N < 5 {
		t.Fatalf("coverage good=%d bad=%d", gs.N, bs.N)
	}
	if gs.PnL <= bs.PnL {
		t.Fatalf("reachable TP should score higher: good=%.4f bad=%.4f", gs.PnL, bs.PnL)
	}
	if trainWR(gs) <= trainWR(bs) {
		t.Fatalf("reachable TP should win more often: goodWR=%.2f badWR=%.2f", trainWR(gs), trainWR(bs))
	}
}

func TestTrainScoreGridIncludesAnalyzeBand(t *testing.T) {
	// Default CLI 0.1-10 stride used to skip TP0.7 / SL1.0 — must always be present.
	stops, _ := ParseRange("0.1-10/0.1")
	targets, _ := ParseRange("0.1-10/0.1")
	ss, tt := trainScoreGrid(stops, targets)
	has := func(xs []float64, want float64) bool {
		for _, x := range xs {
			if math.Abs(x-want) < 1e-9 {
				return true
			}
		}
		return false
	}
	for _, want := range []float64{0.7, 0.9, 1.0, 1.2} {
		if !has(ss, want) {
			t.Fatalf("score stops missing %.1f (got %v)", want, ss)
		}
	}
	for _, want := range []float64{0.5, 0.6, 0.7, 0.8} {
		if !has(tt, want) {
			t.Fatalf("score tps missing %.1f (got %v)", want, tt)
		}
	}
}

func TestBetterTrainPickPrefersMostPnL(t *testing.T) {
	// Most train PnL wins even if WR is lower (analyze objective = gain).
	fat := &trainPick{TrainPnL: 100, TrainWR: 0.40, TrainN: 20, pathPick: pathPick{Lev: 5, PathPnL: 1}}
	thin := &trainPick{TrainPnL: 80, TrainWR: 0.90, TrainN: 20, pathPick: pathPick{Lev: 5, PathPnL: 1}}
	if !betterTrainPick(fat, thin) {
		t.Fatal("higher train PnL must win")
	}
	tiedHiWR := &trainPick{TrainPnL: 100, TrainWR: 0.90, TrainN: 20, pathPick: pathPick{Lev: 5, PathPnL: 1}}
	if !betterTrainPick(tiedHiWR, fat) {
		t.Fatal("tied PnL → higher WR")
	}
	hiLev := &trainPick{TrainPnL: 100, TrainWR: 0.90, TrainN: 20, pathPick: pathPick{Lev: 5, PathPnL: 1}}
	loLev := &trainPick{TrainPnL: 100, TrainWR: 0.90, TrainN: 20, pathPick: pathPick{Lev: 3, PathPnL: 1}}
	if !betterTrainPick(hiLev, loLev) {
		t.Fatal("tied gain → prefer higher lev")
	}
}

func TestPredEntryOKFiltersWeakAndAdverse(t *testing.T) {
	if !predEntryOK(0.40, 0.20, 0.25) {
		t.Fatal("strong peak vs adverse should pass")
	}
	if predEntryOK(0.20, 0.05, 0.25) {
		t.Fatal("peak below entry min must sit out")
	}
	if predEntryOK(0.30, 0.40, 0.25) {
		t.Fatal("adverse > peak must sit out")
	}
}

func TestPickExitsRequiresPositiveTrainPnL(t *testing.T) {
	// Paths that only dump → no combo should clear +train PnL gate.
	samples := make([]trainSample, 0, 12)
	for i := 0; i < 12; i++ {
		samples = append(samples, trainSample{
			Long: true, Entry: 100, Peak: 0.4,
			Act: []float64{99.7, 99.4, 99.0, 98.8},
		})
	}
	goodBand := scoreComboOnSamples(samples, exits.Exits{StopPct: 1.0, Targets: [][2]float64{{0.7, 1}}}, 5, 500, 5, 0.0005, 0.0002)
	if goodBand.PnL > 0 {
		t.Fatalf("dump paths should not print +train PnL, got %.2f", goodBand.PnL)
	}
}

func TestPickExitsFindsAnalyzeLikeCombo(t *testing.T) {
	// Most paths hit ~0.75% then fade a bit (still above −1% SL). Tight SL0.5×TP2 gets stopped.
	samples := make([]trainSample, 0, 20)
	for i := 0; i < 16; i++ {
		entry := 100.0
		act := []float64{entry * 1.004, entry * 1.0075, entry * 1.006, entry * 1.005}
		if i%4 == 0 {
			// Occasional deep dip — tight SL dies, 1% SL survives then TP.
			act = []float64{entry * 1.003, entry * 0.996, entry * 1.007, entry * 1.008}
		}
		samples = append(samples, trainSample{Long: true, Entry: entry, Peak: 0.2, Act: act})
	}
	good := scoreComboOnSamples(samples, exits.Exits{StopPct: 1.0, Targets: [][2]float64{{0.7, 1}}}, 5, 500, 5, 0.0005, 0.0002)
	bad := scoreComboOnSamples(samples, exits.Exits{StopPct: 0.5, Targets: [][2]float64{{2.0, 1}}}, 5, 500, 5, 0.0005, 0.0002)
	if good.N < 10 {
		t.Fatalf("good coverage %d", good.N)
	}
	if trainWR(good) < 0.85 {
		t.Fatalf("analyze-like combo WR=%.2f want ≥0.85 pnl=%.2f", trainWR(good), good.PnL)
	}
	if good.PnL <= bad.PnL {
		t.Fatalf("0.7/1.0/5 should dominate sucker: good pnl=%.2f wr=%.2f bad pnl=%.2f wr=%.2f",
			good.PnL, trainWR(good), bad.PnL, trainWR(bad))
	}
}
