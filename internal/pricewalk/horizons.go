package pricewalk

import (
	"math"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/logx"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

type horizonEval struct {
	Table           []map[string]any
	PathExample     map[string]any
	Horizon         int
	PathUnder1Pct   int
	Models          []pricemodel.Model
	OOSH1PriceErr   float64
	OOSH1BaseErr    float64
	OOSHMaxPriceErr float64
	OOSHMaxBaseErr  float64
}

func evalPriceHorizons(
	h1Model pricemodel.Model,
	bars []candles.Candle,
	horizonN int,
	barMs, trainCutoffMs, tradeStartMs, tradeEndMs int64,
	maxRelErr float64,
	auto bool,
) (horizonEval, error) {
	table := make([]map[string]any, 0, horizonN)
	oosH1Price, oosH1Base := math.NaN(), math.NaN()
	oosHMaxPrice, oosHMaxBase := math.NaN(), math.NaN()
	models := []pricemodel.Model{pricemodel.ColdStart()}
	oosHorizon := 0
	oosStopped := false

	for h := 1; h <= horizonN; h++ {
		allH := pricemodel.SamplesFromCandlesHorizon(bars, h)
		var train []pricemodel.Sample
		for _, s := range allH {
			if s.OriginMs < trainCutoffMs && s.AtMs < tradeStartMs {
				train = append(train, s)
			}
		}
		var model pricemodel.Model
		if h == 1 && !h1Model.ColdStart {
			model = h1Model
		} else {
			m, err := pricemodel.Fit(train, nil)
			if err != nil {
				model = pricemodel.ColdStart()
			} else {
				model = m
			}
		}
		var oos []pricemodel.Sample
		for _, s := range allH {
			if s.OriginMs >= tradeStartMs && s.AtMs < tradeEndMs {
				oos = append(oos, s)
			}
		}
		retMAE, priceErr, baseErr := oosHorizonErrors(model, oos)
		under1 := !math.IsNaN(priceErr) && !math.IsInf(priceErr, 0) && priceErr < maxRelErr
		beatsBase := !math.IsNaN(priceErr) && !math.IsInf(priceErr, 0) &&
			!math.IsNaN(baseErr) && !math.IsInf(baseErr, 0) && priceErr+1e-12 < baseErr

		if auto && !oosStopped {
			if h > 1 && (model.ColdStart || math.IsNaN(priceErr) || math.IsInf(priceErr, 0) || priceErr >= maxRelErr) {
				logx.Infof("    auto horizon (OOS): stop at h=%d (OOS price_err %.3f%% ≥ %.2f%%) → keep 1..%d",
					h, priceErr*100, maxRelErr*100, h-1)
				oosStopped = true
			} else {
				oosHorizon = h
			}
		}

		keepFitting := !auto || !oosStopped || h <= max(oosHorizon, 1)+200
		if keepFitting {
			alphas := make([]float64, pricemodel.Features)
			for i := 0; i < pricemodel.Features; i++ {
				alphas[i] = round2(model.Alphas[i]*1e6) / 1e6
			}
			table = append(table, map[string]any{
				"h": h, "train_n": len(train), "oos_n": len(oos),
				"train_price_err": model.PriceRelErr, "alphas": alphas,
				"oos_return_mae": retMAE, "oos_price_err": priceErr,
				"oos_baseline_err": baseErr, "under_1pct": under1,
				"beats_baseline": beatsBase,
				"gate_ok":        !model.ColdStart && under1 && beatsBase,
				"in_trading_horizon": !auto || h <= max(oosHorizon, 1),
			})
			if h == 1 {
				oosH1Price, oosH1Base = priceErr, baseErr
			}
			if !auto || h <= max(oosHorizon, 1) {
				oosHMaxPrice, oosHMaxBase = priceErr, baseErr
			}
			models = append(models, model)
		} else {
			break
		}
		if auto && oosStopped && h >= max(oosHorizon, 1)+200 {
			break
		}
	}
	if !auto {
		oosHorizon = max(len(models)-1, 0)
	}
	if oosHorizon == 0 {
		oosHorizon = max(len(models)-1, 1)
	}
	fitN := len(models) - 1

	// Example path from just before TRADE (older tape date) — NOT post-tape / last candle.
	// Strategy look-ahead is per-origin until |pred−actual|≥maxRelErr; keep all fitted models.
	pathExample, pathUnder := buildForwardPath(bars, models, barMs, tradeStartMs, fitN, oosHorizon, maxRelErr)
	if pathExample != nil {
		pathExample["kind"] = "example_from_trade_start"
		pathExample["note"] = "illustrative under-1% path from before TRADE; each real trade recomputes length until err≥1%"
	}

	return horizonEval{
		Table: table, PathExample: pathExample, Horizon: oosHorizon,
		PathUnder1Pct: pathUnder, Models: models,
		OOSH1PriceErr: oosH1Price, OOSH1BaseErr: oosH1Base,
		OOSHMaxPriceErr: oosHMaxPrice, OOSHMaxBaseErr: oosHMaxBase,
	}, nil
}

func buildForwardPath(
	bars []candles.Candle,
	models []pricemodel.Model,
	barMs, tradeEndMs int64,
	fitN, oosHorizon int,
	maxRelErr float64,
) (map[string]any, int) {
	originT := -1
	for t := 1; t < len(bars); t++ {
		if bars[t].TimeMs < tradeEndMs {
			originT = t
		}
	}
	if originT < 1 {
		return nil, 0
	}
	feat, ok := pricemodel.CandleFeatures(bars[originT], bars[originT-1].Close)
	if !ok {
		return nil, 0
	}
	refPx := bars[originT].Close
	originMs := bars[originT].TimeMs
	lo := originMs + barMs
	hi := originMs + int64(fitN)*barMs
	okxCloses := softOfficialCandles(barMs, lo, hi)
	okxAt := map[int64]float64{}
	for _, c := range okxCloses {
		okxAt[c.TimeMs] = c.Close
	}

	spotAt := func(h int, futureMs int64) (float64, string, bool) {
		if originT+h < len(bars) {
			return bars[originT+h].Close, "tape", true
		}
		if px, ok := okxAt[futureMs]; ok {
			return px, "okx", true
		}
		return 0, "none", false
	}

	var steps []map[string]any
	under := 0
	breached := false
	for h := 1; h <= fitN; h++ {
		pred := models[h].PredictClose(refPx, feat)
		futureMs := originMs + int64(h)*barMs
		spot, source, has := spotAt(h, futureMs)
		var errPct, baseErrPct any
		over := false
		if has {
			e := 100 * math.Abs(pred-spot) / math.Max(spot, 1e-12)
			b := 100 * math.Abs(refPx-spot) / math.Max(spot, 1e-12)
			errPct, baseErrPct = round2(e), round2(b)
			over = e >= maxRelErr*100
		}
		if !breached {
			if !has {
				break
			}
			if over {
				breached = true
				logx.Infof("    forward-until-1%%: breach at h=%d err=%v%% (actual %s) → under-1%% path = %d candles",
					h, errPct, source, under)
			} else {
				under = h
			}
		}
		var spotJSON any
		if has {
			spotJSON = round2(spot)
		}
		steps = append(steps, map[string]any{
			"h": h, "at": utc(futureMs), "spot": spotJSON, "predicted": round2(pred),
			"ref": round2(refPx), "err_pct": errPct, "base_err_pct": baseErrPct,
			"beyond_tape": source != "tape", "actual_source": source,
			"over_1pct": over, "in_under_1pct_path": h <= under,
		})
		if breached && h >= under+3 {
			break
		}
	}
	if !breached && under > 0 {
		logx.Infof("    forward-until-1%%: still under %.2f%% for %d candles (no breach in available actuals)",
			maxRelErr*100, under)
	}
	return map[string]any{
		"origin": utc(originMs), "origin_close": round2(refPx),
		"kind": "forward_from_last_candle", "path_under_1pct": under,
		"oos_trading_horizon": oosHorizon, "steps": steps,
	}, under
}

func oosHorizonErrors(model pricemodel.Model, oos []pricemodel.Sample) (ret, price, base float64) {
	if len(oos) == 0 {
		return math.NaN(), math.NaN(), math.NaN()
	}
	var retSum, priceSum, baseSum float64
	n := 0
	for _, s := range oos {
		if math.IsNaN(s.Spot) || math.IsInf(s.Spot, 0) || s.Spot <= 0 ||
			math.IsNaN(s.RefPx) || math.IsInf(s.RefPx, 0) || s.RefPx <= 0 {
			continue
		}
		rhat := model.PredictReturn(s.Features)
		y := s.Spot/s.RefPx - 1
		pred := model.PredictClose(s.RefPx, s.Features)
		retSum += math.Abs(y - rhat)
		priceSum += math.Abs(pred-s.Spot) / s.Spot
		baseSum += math.Abs(s.RefPx-s.Spot) / s.Spot
		n++
	}
	if n == 0 {
		return math.NaN(), math.NaN(), math.NaN()
	}
	nf := float64(n)
	return retSum / nf, priceSum / nf, baseSum / nf
}
