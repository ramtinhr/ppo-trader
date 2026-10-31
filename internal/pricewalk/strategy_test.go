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
