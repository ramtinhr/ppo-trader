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
	// Strong predicted rally; 1.5% TP should stay profitable on mid and adverse−1%.
	e := exits.Exits{StopPct: 2, Targets: [][2]float64{{1.5, 1}}}
	pred := []float64{101, 102, 103, 104}
	taker, maker := 0.0005, 0.0002
	steps := int64(10_000)
	mid := simulateExitsOnPath(true, 100, e, steps, pred, taker, maker)
	adv := simulateExitsOnPath(true, 100, e, steps, adversePath(pred, true, 0.01), taker, maker)
	if mid.PnL <= 0 || adv.PnL < 0 {
		t.Fatalf("mid=%v adv=%v reason mid=%s adv=%s", mid.PnL, adv.PnL, mid.Reason, adv.Reason)
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
