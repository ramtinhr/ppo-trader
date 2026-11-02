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

	// IS check: last candle BEFORE trade_start (end of TRAIN). Discover lookback N
	// and how many forward candles H stay under maxRelErr — this is the skill proof,
	// not the OOS Forecasts dump (those are every TRADE bar's h=1 chart point).
	pathExample, pathUnder := buildForwardPath(bars, models, barMs, tradeStartMs, fitN, oosHorizon, maxRelErr)
	if pathExample != nil {
		originIdx := -1
		for t := 1; t < len(bars); t++ {
			if bars[t].TimeMs < tradeStartMs {
				originIdx = t
			}
		}
		lookbackN, horizonH := 0, pathUnder
		if originIdx >= 2 {
			lookbackN, horizonH = discoverLookbackNH(models, bars, originIdx, maxRelErr, oosHorizon)
		}
		pathExample["kind"] = "is_end_under_1pct_check"
		pathExample["phase"] = "IS"
		pathExample["lookback_n"] = lookbackN
		pathExample["horizon_h"] = horizonH
		pathExample["max_rel_err"] = maxRelErr
		pathExample["note"] = "IS end: from last TRAIN candle, lookback N → forward H while |pred−actual|/actual < max_rel_err. OOS trades re-find N/H the same way. Forecasts tab ≠ this path."
		logx.Infof("    IS check @%s: lookback N=%d → under-%.2f%% forward H=%d (path_under_1pct steps=%d)",
			pathExample["origin"], lookbackN, maxRelErr*100, horizonH, pathUnder)
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

// under1PctHorizonLookback is the longest h whose mean |pred−spot|/spot on the
// lookback window [originIdx−lookbackN, originIdx) stays under maxRelErr.
// Uses only past bars (no future peek). lookbackN/H come from data, not fixed constants.
func under1PctHorizonLookback(
	models []pricemodel.Model,
	bars []candles.Candle,
	originIdx, lookbackN int,
	maxRelErr float64,
) int {
	if maxRelErr <= 0 {
		maxRelErr = 0.01
	}
	hMax := len(models) - 1
	if hMax < 1 || originIdx < 3 || lookbackN < 8 {
		return 0
	}
	lo := originIdx - lookbackN
	if lo < 1 {
		lo = 1
	}
	// Target must also lie inside the lookback (past-only validation).
	best := 0
	for h := 1; h <= hMax; h++ {
		m := models[h]
		if m.ColdStart {
			break
		}
		var sum float64
		n := 0
		// origin t with t+h < originIdx and t >= lo
		hi := originIdx - h
		if hi <= lo {
			break
		}
		for t := lo; t < hi; t++ {
			if t < 1 || t+h >= len(bars) {
				continue
			}
			feat, ok := pricemodel.CandleFeatures(bars[t], bars[t-1].Close)
			if !ok {
				continue
			}
			ref := bars[t].Close
			spot := bars[t+h].Close
			if !(ref > 0) || math.IsNaN(ref) || math.IsInf(ref, 0) ||
				!(spot > 0) || math.IsNaN(spot) || math.IsInf(spot, 0) {
				continue
			}
			pred := m.PredictClose(ref, feat)
			if !(pred > 0) || math.IsNaN(pred) || math.IsInf(pred, 0) {
				continue
			}
			sum += math.Abs(pred-spot) / spot
			n++
		}
		if n < 4 {
			break
		}
		if sum/float64(n) >= maxRelErr {
			break
		}
		best = h
	}
	return best
}

// discoverLookbackNH walks lookback size classes ending at originIdx until
// under-1% H is found. Called at the first origin and again after every closed
// trade (appended actuals change the past) so the next entry re-finds N then
// re-predicts. Numbers come from past data only.
func discoverLookbackNH(
	models []pricemodel.Model,
	bars []candles.Candle,
	originIdx int,
	maxRelErr float64,
	hintH int,
) (lookbackN, horizonH int) {
	available := originIdx - 2
	if available < 16 {
		h := under1PctHorizonLookback(models, bars, originIdx, available, maxRelErr)
		if h < 1 && hintH > 0 {
			h = hintH
		}
		if h > available {
			h = max(available, 1)
		}
		return max(available, 8), max(h, 1)
	}
	// Expanding size classes; pick the smallest N that yields a usable H,
	// else the N with the best H (stable class — do not re-search every trade).
	candidates := []int{64, 128, 256, 512, 1024, 2048, 4096, available}
	bestN, bestH := 0, 0
	minUsable := 4
	if hintH > 0 && hintH < minUsable {
		minUsable = max(hintH, 1)
	}
	for _, n := range candidates {
		if n > available {
			n = available
		}
		if n < 16 {
			continue
		}
		h := under1PctHorizonLookback(models, bars, originIdx, n, maxRelErr)
		if h > bestH {
			bestH, bestN = h, n
		}
		if h >= minUsable {
			logx.Infof("    discover N/H @%d: lookback N=%d → under-%.2f%% H=%d (first working class)",
				originIdx, n, maxRelErr*100, h)
			return n, h
		}
		if n == available {
			break
		}
	}
	if bestH < 1 {
		bestN = max(available, 16)
		// Cap fallback H so train samples (t+H < origin inside N) can exist.
		capH := max(bestN/2, 1)
		if hintH > 0 && hintH < capH {
			bestH = hintH
		} else {
			bestH = capH
		}
		if bestH < 1 {
			bestH = 1
		}
		logx.Warnf("    discover N/H: no under-%.2f%% H on lookback — fall back N=%d H=%d",
			maxRelErr*100, bestN, bestH)
		return bestN, bestH
	}
	logx.Infof("    discover N/H: lookback N=%d → under-%.2f%% H=%d (best available)",
		bestN, maxRelErr*100, bestH)
	return bestN, bestH
}

// widenLookbackN grows the lookback size-class when under-1% H collapsed.
func widenLookbackN(currentN, originIdx, availableExtra int) int {
	available := originIdx - 2
	if availableExtra > 0 && available > availableExtra {
		// Cap at how far back the expanding window can reach.
		available = availableExtra
	}
	next := currentN * 2
	if next < currentN+64 {
		next = currentN + 64
	}
	if next > available {
		next = available
	}
	if next <= currentN {
		return currentN
	}
	return next
}
