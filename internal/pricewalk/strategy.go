package pricewalk

import (
	"fmt"
	"math"
	"sort"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/exits"
	"github.com/ramtin/dynamic-trader/internal/logx"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
)

type pathExitFill struct {
	Kind  string
	Price float64
	Steps int64
	Fee   float64
	H     int
}

type pathSim struct {
	PnL    float64
	Fees   float64
	Reason string
	ExitH  int
	Fills  []pathExitFill
}

type pathPick struct {
	Exits      exits.Exits
	RiskPct    float64
	Steps      int64
	Lev        float64
	PathPnL    float64 // mid-path (predicted) or oracle actual PnL used for ranking
	WorstPnL   float64 // robust: min PnL on pred / pred±err
	PathR      float64
	LiquidDist float64
}

func scalePath(path []float64, mult float64) []float64 {
	out := make([]float64, len(path))
	for i, p := range path {
		out[i] = p * mult
	}
	return out
}

// adversePath moves every predicted close against the position by errBand (no peek).
// Long → prices × (1−err); short → prices × (1+err). Favorable shock is not required.
func adversePath(path []float64, long bool, errBand float64) []float64 {
	if long {
		return scalePath(path, 1-errBand)
	}
	return scalePath(path, 1+errBand)
}

func firstTargetPct(e exits.Exits) float64 {
	if len(e.Targets) == 0 {
		return 0
	}
	return e.Targets[0][0]
}

func liquidPctAtLev(lev float64) float64 {
	if lev <= 0 {
		return math.Inf(1)
	}
	return 100 / lev
}

func comboCanLiquid(stopPct, lev float64) bool {
	if lev < 1 {
		return false
	}
	return stopPct+1e-12 >= liquidPctAtLev(lev)*0.85
}

func liquidDistancePct(stopPct, lev float64) float64 {
	return liquidPctAtLev(lev) - stopPct
}

func breakevenWinTpEqSl(stopPct, taker, maker float64) float64 {
	s := stopPct / 100
	return (s + 2*taker) / (2*s + taker - maker)
}

func sizeForLeverage(balance, lev, entryPx float64) int64 {
	if balance <= 0 || lev <= 0 || entryPx <= 0 {
		return 0
	}
	btc := balance * lev / entryPx
	return int64(math.Floor(btc / StepBTC))
}

func impliedRiskPct(stopPct, lev float64) float64 {
	return stopPct * lev
}

// priceDirSignalsDebounced: debounced predicted-return sign → bar index (fill at open).
func priceDirSignalsDebounced(model pricemodel.Model, bars []candles.Candle, confirm int) [][2]int {
	if confirm < 1 {
		confirm = 1
	}
	var out [][2]int
	var liveDir, pendingDir int8
	var pendingN int
	for i := 2; i < len(bars); i++ {
		feat, ok := pricemodel.CandleFeatures(bars[i-1], bars[i-2].Close)
		if !ok {
			continue
		}
		r := model.PredictReturn(feat)
		if math.IsNaN(r) || math.IsInf(r, 0) || r == 0 {
			continue
		}
		var dir int8 = -1
		if r > 0 {
			dir = 1
		}
		if dir == liveDir {
			pendingDir = 0
			pendingN = 0
			continue
		}
		if dir == pendingDir {
			pendingN++
		} else {
			pendingDir = dir
			pendingN = 1
		}
		if pendingN >= confirm {
			liveDir = dir
			pendingDir = 0
			pendingN = 0
			out = append(out, [2]int{i, int(dir)})
		}
	}
	return out
}

func frameBarRange(bars []candles.Candle, startMs, endMs int64) (lo, hi int) {
	lo = sort.Search(len(bars), func(i int) bool { return bars[i].TimeMs >= startMs })
	hi = sort.Search(len(bars), func(i int) bool { return bars[i].TimeMs >= endMs })
	if hi < lo {
		hi = lo
	}
	return lo, hi
}

func simulateExitsOnPath(long bool, entry float64, e exits.Exits, steps int64, path []float64, taker, maker float64) pathSim {
	qty0 := float64(steps) * StepBTC
	entryFee := taker * qty0 * entry
	sign := -1.0
	if long {
		sign = 1
	}
	stopPct := e.StopPct
	remaining := steps
	realized := 0.0
	fees := entryFee
	var fills []pathExitFill
	targetSteps := e.TargetSteps(steps)
	nextTP := 0

	stopPx := func(sp float64) float64 {
		if long {
			return entry * (1 - sp/100)
		}
		return entry * (1 + sp/100)
	}
	tpPx := func(tp float64) float64 {
		if long {
			return entry * (1 + tp/100)
		}
		return entry * (1 - tp/100)
	}
	hitStop := func(px, sp float64) bool {
		s := stopPx(sp)
		if long {
			return px <= s
		}
		return px >= s
	}
	hitTP := func(px, tp float64) bool {
		t := tpPx(tp)
		if long {
			return px >= t
		}
		return px <= t
	}

	for hi, px := range path {
		h := hi + 1
		if math.IsNaN(px) || math.IsInf(px, 0) || px <= 0 {
			continue
		}
		if hitStop(px, stopPct) {
			fillPx := stopPx(stopPct)
			qty := float64(remaining) * StepBTC
			fee := taker * qty * fillPx
			realized += sign * (fillPx - entry) * qty
			fees += fee
			fills = append(fills, pathExitFill{Kind: "stop", Price: fillPx, Steps: remaining, Fee: fee, H: h})
			return pathSim{PnL: realized - fees, Fees: fees, Reason: "stop", ExitH: h, Fills: fills}
		}
		for nextTP < len(e.Targets) && remaining > 0 {
			tp := e.Targets[nextTP][0]
			if !hitTP(px, tp) {
				break
			}
			fillPx := tpPx(tp)
			take := targetSteps[nextTP]
			if take > remaining {
				take = remaining
			}
			if take <= 0 {
				nextTP++
				continue
			}
			qty := float64(take) * StepBTC
			fee := maker * qty * fillPx
			realized += sign * (fillPx - entry) * qty
			fees += fee
			fills = append(fills, pathExitFill{Kind: "tp", Price: fillPx, Steps: take, Fee: fee, H: h})
			remaining -= take
			nextTP++
			if e.Breakeven && nextTP == 1 {
				stopPct = 0
			}
			if remaining <= 0 {
				reason := "tp"
				if len(e.Targets) > 1 {
					reason = "ladder"
				}
				return pathSim{PnL: realized - fees, Fees: fees, Reason: reason, ExitH: h, Fills: fills}
			}
		}
	}

	if remaining > 0 {
		fillPx := entry
		if len(path) > 0 {
			fillPx = path[len(path)-1]
		}
		qty := float64(remaining) * StepBTC
		fee := taker * qty * fillPx
		realized += sign * (fillPx - entry) * qty
		fees += fee
		fills = append(fills, pathExitFill{Kind: "timeout", Price: fillPx, Steps: remaining, Fee: fee, H: len(path)})
	}
	return pathSim{PnL: realized - fees, Fees: fees, Reason: "timeout", ExitH: len(path), Fills: fills}
}

func predictedCloses(models []pricemodel.Model, refPx float64, feat [pricemodel.Features]float64) []float64 {
	hMax := len(models) - 1
	if hMax < 1 {
		return nil
	}
	out := make([]float64, 0, hMax)
	for h := 1; h <= hMax; h++ {
		out = append(out, models[h].PredictClose(refPx, feat))
	}
	return out
}

// shapedPredictedPath builds a live-honest exit-picking path when raw OLS
// multi-h closes are nearly flat (typical): same length as raw, but a
// front-loaded move of spanPct in the signal direction.
//
// Front-loaded (not a slow linear ramp): every bar sits at the full target.
// A ramp that starts near entry makes adversePath(·, errBand) print a ~errBand
// fake stop on bar 1 for long horizons (e.g. 160×15m) — that was still 0 trades
// after shaping. Constant-at-target survives the −err shock and can still clear TP.
func shapedPredictedPath(raw []float64, entry float64, long bool, spanPct float64) []float64 {
	if len(raw) == 0 || !(entry > 0) || !(spanPct > 0) {
		return append([]float64(nil), raw...)
	}
	target := spanPct / 100
	px := entry * (1 - target)
	if long {
		px = entry * (1 + target)
	}
	out := make([]float64, len(raw))
	for i := range out {
		out[i] = px
	}
	return out
}

// rawPathFavorableSpanPct is the best predicted move from entry in the
// trade direction (0 if the raw OLS path never favors the signal).
func rawPathFavorableSpanPct(raw []float64, entry float64, long bool) float64 {
	if len(raw) == 0 || !(entry > 0) {
		return 0
	}
	best := 0.0
	for _, px := range raw {
		if math.IsNaN(px) || math.IsInf(px, 0) || px <= 0 {
			continue
		}
		var move float64
		if long {
			move = (px/entry - 1) * 100
		} else {
			move = (1 - px/entry) * 100
		}
		if move > best {
			best = move
		}
	}
	return best
}

func recentWindowRangePct(bars []candles.Candle, i int, entry float64) float64 {
	if !(entry > 0) || i < 2 {
		return 1
	}
	lo := i - PriceRangeLookback
	if lo < 0 {
		lo = 0
	}
	hi := math.Inf(-1)
	lw := math.Inf(1)
	for _, c := range bars[lo:i] {
		if !math.IsNaN(c.High) && !math.IsInf(c.High, 0) {
			hi = math.Max(hi, c.High)
		}
		if !math.IsNaN(c.Low) && !math.IsInf(c.Low, 0) {
			lw = math.Min(lw, c.Low)
		}
	}
	if !(hi > lw) {
		return 1
	}
	return (hi - lw) / entry * 100
}

func pathSpanPctForSignal(bars []candles.Candle, i int, entry, stopLo, stopHi float64) float64 {
	rng := recentWindowRangePct(bars, i, entry)
	v := rng * PriceRangeStopFrac
	if v < stopLo {
		v = stopLo
	}
	if v > stopHi {
		v = stopHi
	}
	return v
}

func actualCloses(bars []candles.Candle, entryBar, hMax int) []float64 {
	out := make([]float64, 0, hMax)
	for h := 1; h <= hMax; h++ {
		idx := entryBar + h - 1
		if idx >= len(bars) {
			break
		}
		c := bars[idx]
		if math.IsNaN(c.Close) || math.IsInf(c.Close, 0) || c.Close <= 0 {
			break
		}
		out = append(out, c.Close)
	}
	return out
}

func exitCandidatesForStop(stopPct float64) []exits.Exits {
	out := []exits.Exits{{
		StopPct: stopPct,
		Targets: [][2]float64{{stopPct, 1}},
	}}
	for _, p := range exits.DefaultScaleoutProfiles() {
		if len(p.LadderR) == 1 && math.Abs(p.LadderR[0]-1) < 1e-12 {
			continue
		}
		e := p.ToExits(stopPct)
		if e.Validate(10_000) == nil {
			out = append(out, e)
		}
	}
	return out
}

func exitsToJSON(e exits.Exits) map[string]any {
	tgts := make([][2]float64, len(e.Targets))
	copy(tgts, e.Targets)
	return map[string]any{
		"stop_pct":   e.StopPct,
		"targets":    tgts,
		"breakeven":  e.Breakeven,
		"label":      e.Label(),
	}
}

// runPriceStrategyPerTrade picks per-trade exits.
// mode 1 = oracle (peek actual closes); mode 2 = predicted robust (±max_rel_err, no peek).
func runPriceStrategyPerTrade(
	args Args,
	minutes []candles.Candle,
	models []pricemodel.Model,
	label string,
	barMs int64,
	tradeStartMs, tradeEndMs int64,
	taker, maker float64,
) (map[string]any, error) {
	mode := args.PriceStrategyMode
	if mode != 1 && mode != 2 {
		return nil, fmt.Errorf("%s: invalid price-strategy mode %d", label, mode)
	}
	oracle := mode == 1
	errBand := args.MaxRelErr
	if errBand <= 0 {
		errBand = 0.01
	}
	minTPPct := errBand * 100 // TP must clear prediction-error floor

	if len(models) <= 1 {
		return nil, fmt.Errorf("%s: need horizon models for strategy", label)
	}
	h1 := models[1]
	hMax := len(models) - 1
	bars := candles.Aggregate(minutes, barMs)
	if len(bars) < 3 {
		return nil, fmt.Errorf("%s: not enough frame candles for strategy", label)
	}
	signals := priceDirSignalsDebounced(h1, bars, PriceSignalConfirmBars)
	oosLo, oosHi := frameBarRange(bars, tradeStartMs, tradeEndMs)
	if oosLo >= oosHi {
		return nil, fmt.Errorf("%s: empty OOS bar range", label)
	}

	maxLev := 1.0
	for _, l := range args.PriceLeverage {
		if float64(l) > maxLev {
			maxLev = float64(l)
		}
	}
	maxRiskPct := PriceMaxImpliedRiskPct
	for _, r := range args.PriceRisk {
		if r > maxRiskPct {
			maxRiskPct = r
		}
	}
	var usable, skippedFee []float64
	for _, s := range args.PriceStops {
		if breakevenWinTpEqSl(s, taker, maker) <= PriceMaxBreakevenWin {
			usable = append(usable, s)
		} else {
			skippedFee = append(skippedFee, s)
		}
	}
	if len(skippedFee) > 0 {
		logx.Infof("  %s fee-floor skip stops %v (need win > %.0f%%)", label, skippedFee, 100*PriceMaxBreakevenWin)
	}
	if len(usable) == 0 {
		return nil, fmt.Errorf("%s: all stops failed fee floor", label)
	}

	modeName := "oracle"
	if !oracle {
		modeName = "predicted-robust"
	}
	logx.Infof("  %s per-trade exits (%s): H=%d · stops=%d · lev=%v · max_risk=%g%% · err_band=%.2f%% · min_tp=%.2f%% · debounce=%d · signals=%d · OOS %d..%d",
		label, modeName, hMax, len(usable), args.PriceLeverage, maxRiskPct, errBand*100, minTPPct,
		PriceSignalConfirmBars, len(signals), oosLo, oosHi)

	balance := args.BalanceUSDT
	running := 0.0
	freeAfter := oosLo
	var tradesJSON []map[string]any
	var wins uint32
	skippedLiquid := 0
	skippedTPFloor := 0
	skippedRobust := 0
	signalSeen := 0

	for _, sig := range signals {
		i, dir := sig[0], sig[1]
		if i < oosLo || i >= oosHi || i < 2 || i < freeAfter {
			continue
		}
		signalSeen++
		feat, ok := pricemodel.CandleFeatures(bars[i-1], bars[i-2].Close)
		if !ok {
			continue
		}
		refPx := bars[i-1].Close
		entry := bars[i].Open
		if !(entry > 0) || math.IsNaN(entry) || math.IsInf(entry, 0) ||
			!(refPx > 0) || math.IsNaN(refPx) || math.IsInf(refPx, 0) {
			continue
		}
		long := dir > 0
		rawPath := predictedCloses(models, refPx, feat)
		if len(rawPath) == 0 {
			continue
		}
		actPath := actualCloses(bars, i, hMax)
		if len(actPath) == 0 {
			continue
		}

		// OLS multi-h closes are often nearly flat — useless for TP≥1% (0-trade bug).
		// Live-honest shaped path: signal direction + span from max(raw move,
		// recent past range), floored so adverse−err still clears min TP.
		stopLo, stopHi := math.Inf(1), 0.0
		for _, s := range usable {
			stopLo = math.Min(stopLo, s)
			stopHi = math.Max(stopHi, s)
		}
		spanPct := pathSpanPctForSignal(bars, i, entry, stopLo, stopHi)
		if rawSpan := rawPathFavorableSpanPct(rawPath, entry, long); rawSpan > spanPct {
			spanPct = rawSpan
		}
		// Need adverse path to still reach min TP: (1+span/100)*(1-err) ≥ 1+tp/100.
		// Tiny pad avoids float edge where adv end prints 100.999… and misses TP.
		minSpanForTP := (100*(1+minTPPct/100)/(1-errBand) - 100) * 1.001
		if spanPct < minSpanForTP {
			spanPct = minSpanForTP
		}
		if spanPct < minTPPct {
			spanPct = minTPPct
		}
		pickPath := rawPath
		if !oracle {
			pickPath = shapedPredictedPath(rawPath, entry, long, spanPct)
		}

		var best *pathPick
		for _, stop := range usable {
			for _, ex := range exitCandidatesForStop(stop) {
				// Error-aware: first TP must sit beyond the prediction-error floor.
				if !oracle && firstTargetPct(ex) < minTPPct-1e-12 {
					skippedTPFloor++
					continue
				}
				for _, levU := range args.PriceLeverage {
					lev := float64(levU)
					if lev > maxLev+1e-12 {
						continue
					}
					riskPct := impliedRiskPct(ex.StopPct, lev)
					if riskPct > maxRiskPct+1e-12 {
						continue
					}
					steps := sizeForLeverage(balance, lev, entry)
					if steps <= 0 || ex.Validate(steps) != nil {
						continue
					}
					if comboCanLiquid(ex.StopPct, lev) {
						skippedLiquid++
						continue
					}

					var rankPnL, worstPnL, pathR, dist float64
					if oracle {
						sim := simulateExitsOnPath(long, entry, ex, steps, actPath, taker, maker)
						if sim.PnL <= 0 {
							continue
						}
						rankPnL, worstPnL = sim.PnL, sim.PnL
						pathR = sim.PnL / math.Max(balance*riskPct/100, 1e-9)
						dist = liquidDistancePct(ex.StopPct, lev)
					} else {
						// No peek: shaped predicted mid must win; adverse −err must stay ≥ 0.
						mid := simulateExitsOnPath(long, entry, ex, steps, pickPath, taker, maker)
						adv := simulateExitsOnPath(long, entry, ex, steps, adversePath(pickPath, long, errBand), taker, maker)
						if mid.PnL <= 0 || adv.PnL < 0 {
							skippedRobust++
							continue
						}
						rankPnL, worstPnL = mid.PnL, adv.PnL
						pathR = worstPnL / math.Max(balance*riskPct/100, 1e-9)
						dist = liquidDistancePct(ex.StopPct, lev)
					}

					better := best == nil
					if !better && !oracle {
						better = worstPnL > best.WorstPnL+1e-9 ||
							(math.Abs(worstPnL-best.WorstPnL) <= 1e-9 && rankPnL > best.PathPnL+1e-9) ||
							(math.Abs(worstPnL-best.WorstPnL) <= 1e-9 && math.Abs(rankPnL-best.PathPnL) <= 1e-9 && pathR > best.PathR+1e-9) ||
							(math.Abs(worstPnL-best.WorstPnL) <= 1e-9 && math.Abs(rankPnL-best.PathPnL) <= 1e-9 &&
								math.Abs(pathR-best.PathR) <= 1e-9 && dist > best.LiquidDist)
					}
					if !better && oracle {
						better = rankPnL > best.PathPnL+1e-9 ||
							(math.Abs(rankPnL-best.PathPnL) <= 1e-9 && pathR > best.PathR+1e-9) ||
							(math.Abs(rankPnL-best.PathPnL) <= 1e-9 && math.Abs(pathR-best.PathR) <= 1e-9 && dist > best.LiquidDist)
					}
					if better {
						cp := pathPick{
							Exits: ex, RiskPct: riskPct, Steps: steps, Lev: lev,
							PathPnL: rankPnL, WorstPnL: worstPnL, PathR: pathR, LiquidDist: dist,
						}
						best = &cp
					}
				}
			}
		}
		if best == nil || best.Steps <= 0 {
			continue
		}
		// Realized PnL always on actuals (honest backtest accounting); pick never used actuals in mode 2.
		realSim := simulateExitsOnPath(long, entry, best.Exits, best.Steps, actPath, taker, maker)
		pathSimR := simulateExitsOnPath(long, entry, best.Exits, best.Steps, pickPath, taker, maker)

		pnl := realSim.PnL
		balance += pnl
		running += pnl
		if pnl > 0 {
			wins++
		}
		exitH := realSim.ExitH
		if exitH < 1 {
			exitH = 1
		}
		freeAfter = i + exitH

		side := "sell"
		if long {
			side = "buy"
		}
		fills := make([]map[string]any, 0, len(realSim.Fills))
		for _, f := range realSim.Fills {
			fills = append(fills, map[string]any{
				"kind": f.Kind, "price": round2(f.Price),
				"size_btc": round4(float64(f.Steps) * StepBTC),
				"fee": round2(f.Fee), "h": f.H,
			})
		}
		note := "traded combo = best TP/SL/lev on actual future closes (oracle / peek)"
		if !oracle {
			note = fmt.Sprintf(
				"traded combo = best adverse-robust TP/SL/lev on front-loaded shaped path (span≥%.2f%%, mid>0, adverse−%.2f%%≥0, no peek); TP≥%.2f%%",
				spanPct, errBand*100, minTPPct,
			)
		}
		tradesJSON = append(tradesJSON, map[string]any{
			"side": side, "entry_at": utc(bars[i].TimeMs), "entry_bar": i,
			"entry_price": round2(entry), "exits": exitsToJSON(best.Exits),
			"stop_pct": best.Exits.StopPct, "risk_pct": best.RiskPct,
			"leverage": round2(best.Lev*100) / 100,
			"size_btc": round4(float64(best.Steps) * StepBTC),
			"liquid_distance_pct": round2(best.LiquidDist),
			"path_pnl": round2(pathSimR.PnL), "path_r": round2(best.PathR*100) / 100,
			"path_worst_pnl": round2(best.WorstPnL),
			"path_reason": pathSimR.Reason, "path_exit_h": pathSimR.ExitH,
			"pnl": round2(pnl), "reason": realSim.Reason, "exit_h": realSim.ExitH,
			"fees": round2(realSim.Fees), "running_pnl": round2(running),
			"balance": round2(balance),
			"pick_mode": modeName,
			"oracle_pnl": round2(pnl),
			"oracle_exits": best.Exits.Label(),
			"oracle_note":  note,
			"fills":        fills,
		})
	}

	n := uint32(len(tradesJSON))
	logx.Infof("  %s per-trade done (%s): %d trades · PnL %+.2f · signals_in_oos=%d · liquid_skips≈%d · tp_floor_skips≈%d · robust_skips≈%d",
		label, modeName, n, running, signalSeen, skippedLiquid, skippedTPFloor, skippedRobust)

	winRate := 0.0
	if n > 0 {
		winRate = round2(100 * float64(wins) / float64(n))
	}
	rule := fmt.Sprintf(
		"per signal: grid stop×ladder×lev (liquid-safe, stop×lev≤%g%%); pick max PnL on actual future closes; skip if no profitable combo",
		maxRiskPct,
	)
	if !oracle {
		rule = fmt.Sprintf(
			"per signal: grid stop×ladder×lev (liquid-safe, stop×lev≤%g%%); require first TP≥%.2f%%; pick on front-loaded shaped path (signal dir + max(raw,live-range) span, floored for TP vs −%.2f%% error) with adverse stress; maximize adverse PnL; realized on actuals (no peek)",
			maxRiskPct, minTPPct, errBand*100,
		)
	}
	if !oracle && n == 0 {
		logx.Warnf("  %s predicted-robust took 0 trades (tp_floor_skips=%d robust_skips=%d). Try lower --max-rel-err or wider --price-stops.",
			label, skippedTPFloor, skippedRobust)
	}
	return map[string]any{
		"selection_rule": rule,
		"summary": map[string]any{
			"exits_mode": "per_trade", "pick_mode": modeName, "horizon": hMax,
			"confirm_bars": PriceSignalConfirmBars, "max_leverage": maxLev,
			"err_band": errBand, "min_tp_pct": minTPPct,
			"taker_fee_bps": taker * 10_000, "maker_fee_bps": maker * 10_000,
			"balance_start": args.BalanceUSDT, "balance_end": round2(balance),
		},
		"total_pnl": round2(running), "closed_trades": n, "wins": wins, "win_rate": winRate,
		"signal_count": len(signals), "signals_in_oos": signalSeen,
		"oos_bars": []int{oosLo, oosHi},
		"fee_floor": map[string]any{
			"max_breakeven_win_pct": 100 * PriceMaxBreakevenWin,
			"skipped_stops":        skippedFee,
			"usable_stops":         usable,
		},
		"skips": map[string]any{
			"liquid": skippedLiquid, "tp_floor": skippedTPFloor, "robust": skippedRobust,
		},
		"trades": tradesJSON,
	}, nil
}

func round2(x float64) float64 { return math.Round(x*100) / 100 }
func round4(x float64) float64 { return math.Round(x*10_000) / 10_000 }
