package pricewalk

import (
	"math"
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/exits"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

func TestParsePriceBars(t *testing.T) {
	bars, err := ParsePriceBars("15m,1h,60m,4h")
	if err != nil {
		t.Fatal(err)
	}
	// 60m collapses to 1h; duplicate bar_ms skipped.
	if len(bars) != 3 {
		t.Fatalf("got %v", bars)
	}
	if bars[0].Label != "15m" || bars[0].BarMs != 15*60_000 {
		t.Fatalf("15m: %#v", bars[0])
	}
	if bars[1].Label != "1h" || bars[1].BarMs != 3_600_000 {
		t.Fatalf("1h: %#v", bars[1])
	}
}

func TestParseRange(t *testing.T) {
	v, err := ParseRange("0.5-2/0.5")
	if err != nil {
		t.Fatal(err)
	}
	want := []float64{0.5, 1, 1.5, 2}
	if len(v) != len(want) {
		t.Fatalf("%v", v)
	}
	for i := range want {
		if math.Abs(v[i]-want[i]) > 1e-9 {
			t.Fatalf("%v vs %v", v, want)
		}
	}
}

func TestSimulateExitsStop(t *testing.T) {
	e := exits.Exits{StopPct: 1, Targets: [][2]float64{{1, 1}}}
	// Long entry 100; path drops to 98 → stop at 99.
	sim := simulateExitsOnPath(true, 100, e, 10_000, []float64{99.5, 98}, 0.0005, 0.0002)
	if sim.Reason != "stop" {
		t.Fatalf("reason=%s", sim.Reason)
	}
	if sim.ExitH != 2 {
		t.Fatalf("exit_h=%d", sim.ExitH)
	}
}

func TestScalePath(t *testing.T) {
	p := []float64{100, 101, 102}
	hi := scalePath(p, 1.01)
	if math.Abs(hi[1]-102.01) > 1e-9 {
		t.Fatalf("%v", hi)
	}
}

func TestFirstTargetPct(t *testing.T) {
	e := exits.Exits{StopPct: 1, Targets: [][2]float64{{1.5, 1}}}
	if firstTargetPct(e) != 1.5 {
		t.Fatal(firstTargetPct(e))
	}
}

func TestAdversePathLongIsCheaper(t *testing.T) {
	p := []float64{100, 101, 102}
	adv := adversePath(p, true, 0.01)
	if !(adv[1] < p[1]) {
		t.Fatalf("%v", adv)
	}
}

func TestAdverseRobustAllowsClearWinner(t *testing.T) {
	// Front-loaded +2.5% path; 1.5% TP should win on mid and survive adverse−1%.
	e := exits.Exits{StopPct: 2, Targets: [][2]float64{{1.5, 1}}}
	raw := []float64{100.1, 100.1, 100.2, 100.2} // flat OLS-like
	pred := shapedPredictedPath(raw, 100, true, 2.5)
	if len(pred) == 0 || math.Abs(pred[0]-102.5) > 1e-9 {
		t.Fatalf("expected front-loaded 102.5, got %v", pred)
	}
	taker, maker := 0.0005, 0.0002
	steps := int64(10_000)
	mid := simulateExitsOnPath(true, 100, e, steps, pred, taker, maker)
	adv := simulateExitsOnPath(true, 100, e, steps, adversePath(pred, true, 0.01), taker, maker)
	if mid.PnL <= 0 || adv.PnL < 0 {
		t.Fatalf("mid=%v adv=%v reason mid=%s adv=%s path=%v", mid.PnL, adv.PnL, mid.Reason, adv.Reason, pred)
	}
}

func TestFlatOLSPathCannotReachOnePctTP(t *testing.T) {
	// Explains the old 0-trade bug: flat forecast never hits TP≥1%.
	e := exits.Exits{StopPct: 2, Targets: [][2]float64{{1.0, 1}}}
	flat := []float64{100.05, 100.08, 100.02, 100.1}
	sim := simulateExitsOnPath(true, 100, e, 10_000, flat, 0.0005, 0.0002)
	if sim.Reason == "tp" {
		t.Fatalf("flat path should not hit 1%% TP, got %#v", sim)
	}
}

func TestLinearRampStillFalseStopsOnAdverse(t *testing.T) {
	// Slow ramp + uniform −1% shock → bar1 ≈ −1% vs entry → stop≤1% fires.
	// Kept as a regression lock: we must NOT shape with a near-entry ramp.
	n := 160
	entry := 100.0
	span := 2.020202
	ramp := make([]float64, n)
	for i := 1; i <= n; i++ {
		ramp[i-1] = entry * (1 + (span/100)*(float64(i)/float64(n)))
	}
	// 0.5% stop + 1% TP is a real grid combo (2R); ramp+adverse dies on stop.
	e := exits.Exits{StopPct: 0.5, Targets: [][2]float64{{1, 1}}}
	adv := simulateExitsOnPath(true, entry, e, 10_000, adversePath(ramp, true, 0.01), 0.0005, 0.0002)
	if adv.Reason != "stop" {
		t.Fatalf("expected ramp+adverse to hit stop, got %#v", adv)
	}
	shaped := shapedPredictedPath(ramp, entry, true, span*1.001)
	mid := simulateExitsOnPath(true, entry, e, 10_000, shaped, 0.0005, 0.0002)
	ok := simulateExitsOnPath(true, entry, e, 10_000, adversePath(shaped, true, 0.01), 0.0005, 0.0002)
	if mid.PnL <= 0 || ok.PnL < 0 {
		t.Fatalf("front-loaded should clear: mid=%v (%s) adv=%v (%s)", mid.PnL, mid.Reason, ok.PnL, ok.Reason)
	}
}

func TestRawPathFavorableSpanFlat(t *testing.T) {
	flat := []float64{100.05, 100.08, 100.02, 100.1}
	if s := rawPathFavorableSpanPct(flat, 100, true); s > 0.2 {
		t.Fatalf("flat OLS span should be tiny, got %g", s)
	}
}

func TestDebounceSignals(t *testing.T) {
	// Constant positive-return model via features that produce +r.
	model := pricemodel.Model{Alphas: [4]float64{1, 0, 0, 0}}
	var bars []candles.Candle
	c := 100.0
	for i := 0; i < 20; i++ {
		bars = append(bars, candles.Candle{
			TimeMs: int64(i) * 60_000, Open: c, High: c * 1.01, Low: c * 0.99, Close: c * 1.01,
		})
		c *= 1.01
	}
	sigs := priceDirSignalsDebounced(model, bars, 2)
	if len(sigs) == 0 {
		t.Fatal("expected at least one debounced signal")
	}
}

