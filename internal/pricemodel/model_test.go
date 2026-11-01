package pricemodel

import (
	"testing"

	"github.com/ramtin/dynamic-trader/internal/candles"
)

func TestOLSOnCandleReturns(t *testing.T) {
	trueA := [Features]float64{0.3, 0.1, 0.2, 0.05}
	var bars []candles.Candle
	close := 50_000.0
	for k := 0; k < 50; k++ {
		open := close
		high := open * 1.01
		low := open * 0.99
		close = open * (1 + 0.001*(float64(k%5)-2))
		bars = append(bars, candles.Candle{
			TimeMs: int64(k) * 3_600_000, Open: open, High: high, Low: low, Close: close,
		})
	}
	samples := SamplesFromCandles(bars)
	for i := range samples {
		var r float64
		for j := 0; j < Features; j++ {
			r += trueA[j] * samples[i].Features[j]
		}
		samples[i].Spot = samples[i].RefPx * (1 + r)
	}
	model, err := Fit(samples, nil)
	if err != nil {
		t.Fatal(err)
	}
	if model.ColdStart || model.RelErr >= 0.01 || !model.Ok(0.01) {
		t.Fatalf("bad fit: cold=%v rel=%v", model.ColdStart, model.RelErr)
	}
}

func TestHorizon20Align(t *testing.T) {
	var bars []candles.Candle
	close := 100.0
	for k := 0; k < 40; k++ {
		close += 1
		bars = append(bars, candles.Candle{
			TimeMs: int64(k) * 60_000, Open: close - 0.5, High: close + 1, Low: close - 1, Close: close,
		})
	}
	h20 := SamplesFromCandlesHorizon(bars, 20)
	if len(h20) == 0 {
		t.Fatal("empty")
	}
	if h20[0].OriginMs != bars[1].TimeMs || h20[0].AtMs != bars[21].TimeMs {
		t.Fatalf("misaligned %#v", h20[0])
	}
}

func TestAggregateHourly(t *testing.T) {
	var mins []candles.Candle
	for k := int64(0); k < 120; k++ {
		mins = append(mins, candles.Candle{
			TimeMs: k * 60_000, Open: 100 + float64(k), High: 101 + float64(k),
			Low: 99 + float64(k), Close: 100.5 + float64(k),
		})
	}
	hours := candles.Aggregate(mins, 3_600_000)
	if len(hours) != 2 || hours[0].TimeMs != 0 || hours[1].TimeMs != 3_600_000 {
		t.Fatalf("got %#v", hours)
	}
}
