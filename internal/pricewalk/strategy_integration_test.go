package pricewalk

import (
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

// Synthetic OOS with direction flips + flat multi-h OLS (the production failure mode).
func TestPredictedRobustTradesDespiteFlatOLS(t *testing.T) {
	const barMs = int64(15 * 60_000)
	var minutes []candles.Candle
	px := 100_000.0
	start := int64(1_700_000_000_000)
	// Alternate 20-bar up / 20-bar down blocks so debounce fires inside OOS.
	for i := 0; i < 240; i++ {
		block := i / 20
		up := block%2 == 0
		o := px
		var c float64
		if up {
			c = px * 1.002
		} else {
			c = px * 0.998
		}
		h, l := c, o
		if c < o {
			h, l = o, c
		}
		h *= 1.0005
		l *= 0.9995
		for m := 0; m < 15; m++ {
			minutes = append(minutes, candles.Candle{
				TimeMs: start + int64(i)*barMs + int64(m)*60_000,
				Open:   o,
				High:   h,
				Low:    l,
				Close:  c,
			})
		}
		px = c
	}
	tradeStart := start + 40*barMs
	tradeEnd := start + 220*barMs

	// H1 follows prev_return strongly (so debounce tracks regime flips).
	// Higher-h models nearly flat — raw OLS cannot clear TP≥1%.
	h1 := pricemodel.Model{Alphas: [4]float64{3.0, 0, 0, 0}}
	models := []pricemodel.Model{pricemodel.ColdStart(), h1}
	for h := 2; h <= 160; h++ {
		models = append(models, pricemodel.Model{Alphas: [4]float64{0.0001, 0, 0, 0}})
	}

	args := DefaultArgs()
	args.PriceStrategyMode = 2
	args.MaxRelErr = 0.01
	args.BalanceUSDT = 500
	args.PriceStops = []float64{0.5, 1, 1.5, 2, 3}
	args.PriceLeverage = []uint32{1, 2, 3}
	args.PriceRisk = []float64{5}

	out, err := runPriceStrategyPerTrade(args, minutes, models, "15m", barMs, tradeStart, tradeEnd, 0.0005, 0.0002)
	if err != nil {
		t.Fatal(err)
	}
	n, _ := out["closed_trades"].(uint32)
	skips, _ := out["skips"].(map[string]any)
	sig, _ := out["signals_in_oos"].(int)
	sum, _ := out["summary"].(map[string]any)
	t.Logf("trades=%d signals_in_oos=%v skips=%v pnl=%v pick_mode=%v", n, sig, skips, out["total_pnl"], sum["pick_mode"])
	if sig == 0 {
		t.Fatalf("test setup produced no OOS signals")
	}
	if n == 0 {
		t.Fatalf("expected predicted-robust trades>0 with flat OLS; got 0 (signals_in_oos=%v skips=%v)", sig, skips)
	}
}
