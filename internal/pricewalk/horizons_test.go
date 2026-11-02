package pricewalk

import (
	"math"
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

func TestUnder1PctHorizonLookbackPastOnly(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	n := 300
	var bars []candles.Candle
	px := 100_000.0
	for i := 0; i < n; i++ {
		c := px * (1 + 0.0001)
		bars = append(bars, candles.Candle{
			TimeMs: start + int64(i)*barMs,
			Open: px, High: math.Max(px, c) * 1.0001, Low: math.Min(px, c) * 0.9999, Close: c,
		})
		px = c
	}
	// Near-zero alphas → predictions ≈ ref → under-1% for many horizons on calm path.
	models := []pricemodel.Model{pricemodel.ColdStart()}
	for h := 1; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{0.01, 0, 0, 0}})
	}
	origin := 200
	h := under1PctHorizonLookback(models, bars, origin, 128, 0.01)
	if h < 1 {
		t.Fatalf("expected under-1%% H≥1 on calm lookback, got %d", h)
	}
	nDisc, hDisc := discoverLookbackNH(models, bars, origin, 0.01, 20)
	if nDisc < 16 || hDisc < 1 {
		t.Fatalf("discover N/H got N=%d H=%d", nDisc, hDisc)
	}
	wider := widenLookbackN(nDisc, origin, origin-1)
	if wider < nDisc {
		t.Fatalf("widen must not shrink: %d → %d", nDisc, wider)
	}
}

func TestTradeModelsKeepPathUnder1Pct(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	start = start - (start % barMs)
	n := 400
	var bars []candles.Candle
	px := 100_000.0
	for i := 0; i < n; i++ {
		c := px * (1 + 0.0002)
		if i%7 == 0 {
			c = px * (1 - 0.0002)
		}
		bars = append(bars, candles.Candle{
			TimeMs: start + int64(i)*barMs,
			Open: px, High: math.Max(px, c) * 1.0002, Low: math.Min(px, c) * 0.9998, Close: c,
		})
		px = c
	}
	trainCutoff := start + 250*barMs
	tradeStart := trainCutoff
	tradeEnd := tradeStart + 80*barMs
	h1 := pricemodel.Model{Alphas: [4]float64{0.1, 0, 0, 0}}
	ev, err := evalPriceHorizons(h1, bars, 120, barMs, trainCutoff, tradeStart, tradeEnd, 0.01, true)
	if err != nil {
		t.Fatal(err)
	}
	if ev.PathUnder1Pct < 1 {
		t.Fatalf("expected path_under_1pct≥1, got %d", ev.PathUnder1Pct)
	}
	if ev.PathExample == nil {
		t.Fatal("expected IS path_example")
	}
	if ev.PathExample["kind"] != "is_end_under_1pct_check" {
		t.Fatalf("kind=%v", ev.PathExample["kind"])
	}
	if ev.PathExample["phase"] != "IS" {
		t.Fatalf("phase=%v", ev.PathExample["phase"])
	}
	if _, ok := ev.PathExample["lookback_n"].(int); !ok {
		t.Fatalf("lookback_n missing: %v", ev.PathExample["lookback_n"])
	}
	want := ev.PathUnder1Pct
	if ev.Horizon > want {
		want = ev.Horizon
	}
	got := len(ev.Models) - 1
	if got < want {
		t.Fatalf("trade models H=%d < keepTo=%d (oosHorizon=%d pathUnder=%d)",
			got, want, ev.Horizon, ev.PathUnder1Pct)
	}
}
