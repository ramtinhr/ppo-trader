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

// Even flat multi-h + tiny h1 must still trade via span floor (TP≥err after −err).
func TestPredictedTradesWithSpanFloor(t *testing.T) {
	const barMs = int64(15 * 60_000)
	start := int64(1_700_000_000_000)
	minutes := synthMinutes(240, barMs, start)
	h1 := pricemodel.Model{Alphas: [4]float64{3.0, 0, 0, 0}} // need signals
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 40; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{0.0001, 0, 0, 0}})
	}
	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.PriceStops = []float64{1.5, 2, 2.5, 3}
	args.PriceLeverage = []uint32{1, 2}
	args.PriceRisk = []float64{5}
	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs,
		start+40*barMs, start+220*barMs, 0.0005, 0.0002)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	sig, _ := out["signals_in_oos"].(int)
	t.Logf("trades=%d signals=%v pnl=%v skips=%v", n, sig, out["total_pnl"], out["skips"])
	if sig == 0 {
		t.Fatal("expected OOS signals")
	}
	if n == 0 {
		t.Fatalf("span floor must allow trades; skips=%v", out["skips"])
	}
}

func TestInBandActualNeverLoses(t *testing.T) {
	entry := 100.0
	errBand := 0.01
	pred := []float64{101.2, 102.0, 102.5, 103.0}
	e := exits.Exits{StopPct: 1.5, Targets: [][2]float64{{1.0, 1}}}
	taker, maker := 0.0005, 0.0002
	steps := sizeForLeverage(500, 1, entry)
	mid := simulateExitsOnPath(true, entry, e, steps, pred, taker, maker)
	advPath := adversePath(pred, true, errBand)
	adv := simulateExitsOnPath(true, entry, e, steps, advPath, taker, maker)
	if mid.PnL <= 0 || adv.PnL < 0 || (mid.Reason != "tp" && mid.Reason != "ladder") ||
		(adv.Reason != "tp" && adv.Reason != "ladder") {
		t.Fatalf("setup mid=%v (%s) adv=%v (%s)", mid.PnL, mid.Reason, adv.PnL, adv.Reason)
	}
	got := simulateExitsOnPath(true, entry, e, steps, advPath, taker, maker)
	if got.PnL < 0 {
		t.Fatalf("in-band actual lost: pnl=%v reason=%s", got.PnL, got.Reason)
	}
}

func TestMinSpanForOnePctTP(t *testing.T) {
	// err=1%, TP=1% → need span ≥ 100*(1.01/0.99 - 1) ≈ 2.0202%
	errBand, minTP := 0.01, 1.0
	minSpan := 100*(1+minTP/100)/(1-errBand) - 100
	if minSpan < 2.02 || minSpan > 2.03 {
		t.Fatalf("minSpan=%g", minSpan)
	}
	// span=1.5% (stop floor only) is NOT enough — explains user's flat_skips.
	maxTP := 100 * ((1+1.5/100)*(1-errBand) - 1)
	if maxTP >= minTP {
		t.Fatalf("1.5%% span should not clear 1%% TP after −err, maxTP=%g", maxTP)
	}
}

func TestH1ExtrapSpan(t *testing.T) {
	p := h1ExtrapolatedPath(10, 100, true, 0.002)
	if len(p) != 10 || p[9] < 101.9 {
		t.Fatalf("%v", p)
	}
}
