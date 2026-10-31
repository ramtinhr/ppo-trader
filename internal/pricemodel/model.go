// Package pricemodel ports the candle OLS price path from src/price_model.rs.
package pricemodel

import (
	"fmt"
	"math"

	"github.com/ramtin/dynamic-trader/internal/candles"
)

const (
	Features       = 4
	MinFitSamples  = 8
)

var FeatureNames = [Features]string{"prev_return", "range", "body", "typical_dev"}

type Sample struct {
	Features [Features]float64
	Spot     float64
	RefPx    float64
	AtMs     int64
	OriginMs int64
}

type Model struct {
	Alphas         [Features]float64
	MAE            float64
	RelErr         float64
	PriceRelErr    float64
	BaselineRelErr float64
	Samples        int
	ColdStart      bool
}

func ColdStart() Model {
	return Model{ColdStart: true}
}

func (m Model) PredictReturn(x [Features]float64) float64 {
	var s float64
	for i := 0; i < Features; i++ {
		s += m.Alphas[i] * x[i]
	}
	return s
}

func (m Model) PredictClose(ref float64, x [Features]float64) float64 {
	return math.Max(0, ref*(1+m.PredictReturn(x)))
}

func (m Model) Ok(maxRelErr float64) bool {
	return !m.ColdStart && m.RelErr < maxRelErr && m.PriceRelErr+1e-12 < m.BaselineRelErr
}

func evalMetrics(samples []Sample, alphas [Features]float64) (ret, price, base, mae float64) {
	var retSum, priceSum, baseSum, absSum float64
	var n int
	for _, s := range samples {
		if !finite(s.Spot) || s.Spot <= 0 || !finite(s.RefPx) || s.RefPx <= 0 {
			continue
		}
		var rhat float64
		for i := 0; i < Features; i++ {
			rhat += alphas[i] * s.Features[i]
		}
		y := s.Spot/s.RefPx - 1
		pred := s.RefPx * (1 + rhat)
		retSum += math.Abs(y - rhat)
		priceSum += math.Abs(pred-s.Spot) / s.Spot
		baseSum += math.Abs(s.RefPx-s.Spot) / s.Spot
		absSum += math.Abs(pred - s.Spot)
		n++
	}
	if n == 0 {
		inf := math.Inf(1)
		return inf, inf, inf, inf
	}
	nf := float64(n)
	return retSum / nf, priceSum / nf, baseSum / nf, absSum / nf
}

func (m Model) RelativeError(samples []Sample) float64 {
	r, _, _, _ := evalMetrics(samples, m.Alphas)
	return r
}

func Fit(samples []Sample, warm *Model) (Model, error) {
	if len(samples) < MinFitSamples {
		m := ColdStart()
		if warm != nil {
			m = *warm
		}
		if len(samples) > 0 {
			ret, price, base, mae := evalMetrics(samples, m.Alphas)
			m.RelErr, m.PriceRelErr, m.BaselineRelErr, m.MAE = ret, price, base, mae
			m.Samples = len(samples)
		}
		return m, nil
	}
	var ata [Features][Features]float64
	var aty [Features]float64
	var n int
	for _, s := range samples {
		if !finite(s.Spot) || s.Spot <= 0 || !finite(s.RefPx) || s.RefPx <= 0 {
			continue
		}
		y := s.Spot/s.RefPx - 1
		x := s.Features
		for i := 0; i < Features; i++ {
			aty[i] += x[i] * y
			for j := 0; j < Features; j++ {
				ata[i][j] += x[i] * x[j]
			}
		}
		n++
	}
	if n < MinFitSamples {
		return Model{}, fmt.Errorf("not enough candle samples")
	}
	for i := 0; i < Features; i++ {
		ata[i][i] += 1e-6
	}
	alphas, ok := solve4(ata, aty)
	if !ok {
		return Model{}, fmt.Errorf("price OLS singular")
	}
	ret, price, base, mae := evalMetrics(samples, alphas)
	model := Model{
		Alphas: alphas, MAE: mae, RelErr: ret, PriceRelErr: price,
		BaselineRelErr: base, Samples: len(samples), ColdStart: false,
	}
	if warm != nil && !warm.ColdStart && warm.RelativeError(samples) < model.RelErr {
		ret, price, base, mae = evalMetrics(samples, warm.Alphas)
		model.Alphas = warm.Alphas
		model.RelErr, model.PriceRelErr, model.BaselineRelErr, model.MAE = ret, price, base, mae
	}
	return model, nil
}

func CandleFeatures(prev candles.Candle, prevPrevClose float64) ([Features]float64, bool) {
	var z [Features]float64
	if prev.Open <= 0 || prev.Close <= 0 || prevPrevClose <= 0 {
		return z, false
	}
	if !finite(prev.High) || !finite(prev.Low) {
		return z, false
	}
	prevReturn := prev.Close/prevPrevClose - 1
	rng := (prev.High - prev.Low) / prev.Close
	body := (prev.Close - prev.Open) / prev.Open
	typical := (prev.High + prev.Low + prev.Close) / 3
	typicalDev := typical/prev.Close - 1
	out := [Features]float64{prevReturn, rng, body, typicalDev}
	for _, v := range out {
		if !finite(v) {
			return z, false
		}
	}
	return out, true
}

func SamplesFromCandlesHorizon(bars []candles.Candle, horizon int) []Sample {
	var out []Sample
	if horizon == 0 || len(bars) <= horizon+1 {
		return out
	}
	for t := 1; t < len(bars)-horizon; t++ {
		feat, ok := CandleFeatures(bars[t], bars[t-1].Close)
		if !ok {
			continue
		}
		target := bars[t+horizon]
		out = append(out, Sample{
			Features: feat, Spot: target.Close, RefPx: bars[t].Close,
			AtMs: target.TimeMs, OriginMs: bars[t].TimeMs,
		})
	}
	return out
}

func SamplesFromCandles(bars []candles.Candle) []Sample {
	return SamplesFromCandlesHorizon(bars, 1)
}

func solve4(a [Features][Features]float64, b [Features]float64) ([Features]float64, bool) {
	for col := 0; col < Features; col++ {
		piv := col
		for r := col + 1; r < Features; r++ {
			if math.Abs(a[r][col]) > math.Abs(a[piv][col]) {
				piv = r
			}
		}
		if math.Abs(a[piv][col]) < 1e-18 {
			return [Features]float64{}, false
		}
		if piv != col {
			a[piv], a[col] = a[col], a[piv]
			b[piv], b[col] = b[col], b[piv]
		}
		diag := a[col][col]
		for r := col + 1; r < Features; r++ {
			f := a[r][col] / diag
			for c := col; c < Features; c++ {
				a[r][c] -= f * a[col][c]
			}
			b[r] -= f * b[col]
		}
	}
	var x [Features]float64
	for i := Features - 1; i >= 0; i-- {
		s := b[i]
		for j := i + 1; j < Features; j++ {
			s -= a[i][j] * x[j]
		}
		x[i] = s / a[i][i]
	}
	return x, true
}

func finite(x float64) bool { return !math.IsNaN(x) && !math.IsInf(x, 0) }
