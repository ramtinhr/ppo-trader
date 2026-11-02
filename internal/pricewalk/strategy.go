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
	ExitH      int // predicted bars to TP (prefer earlier)
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
		reason := "timeout"
		kind := "timeout"
		// Never realize worse than the stop on timeout (the −20 bleed bug).
		sPx := stopPx(stopPct)
		mtm := sign * (fillPx - entry) * qty
		atStop := sign * (sPx - entry) * qty
		if mtm < atStop || hitStop(fillPx, stopPct) {
			fillPx = sPx
			reason = "stop"
			kind = "stop"
			mtm = atStop
		} else if mtm < 0 {
			// Still losing at horizon end but inside stop band → flatten at stop (risk cap).
			fillPx = sPx
			reason = "stop"
			kind = "stop"
			mtm = atStop
		}
		fee := taker * qty * fillPx
		realized += mtm
		fees += fee
		fills = append(fills, pathExitFill{Kind: kind, Price: fillPx, Steps: remaining, Fee: fee, H: len(path)})
		return pathSim{PnL: realized - fees, Fees: fees, Reason: reason, ExitH: len(path), Fills: fills}
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

// maxPredActualRelDev is max |act-pred|/pred over the overlapping prefix (0 if empty).
func maxPredActualRelDev(pred, act []float64) float64 {
	n := len(pred)
	if len(act) < n {
		n = len(act)
	}
	worst := 0.0
	for i := 0; i < n; i++ {
		p := pred[i]
		a := act[i]
		if !(p > 0) || math.IsNaN(p) || math.IsInf(p, 0) ||
			math.IsNaN(a) || math.IsInf(a, 0) || a <= 0 {
			continue
		}
		d := math.Abs(a-p) / p
		if d > worst {
			worst = d
		}
	}
	return worst
}

// h1ExtrapolatedPath projects the h=1 predicted return out to every horizon in the
// signal direction. Multi-h OLS closes are often nearly flat even when h=1 (the
// signal model) has real edge — without this, error-band rules take 0 trades.
func h1ExtrapolatedPath(n int, entry float64, long bool, r1 float64) []float64 {
	if n < 1 || !(entry > 0) {
		return nil
	}
	step := math.Abs(r1)
	if !(step > 0) || math.IsNaN(step) || math.IsInf(step, 0) {
		return nil
	}
	out := make([]float64, n)
	for i := 0; i < n; i++ {
		move := step * float64(i+1)
		if long {
			out[i] = entry * (1 + move)
		} else {
			out[i] = entry * (1 - move)
		}
	}
	return out
}

// mergeFavorablePaths keeps the more favorable price at each step (signal direction).
func mergeFavorablePaths(a, b []float64, long bool) []float64 {
	if len(a) == 0 {
		return append([]float64(nil), b...)
	}
	if len(b) == 0 {
		return append([]float64(nil), a...)
	}
	n := len(a)
	if len(b) < n {
		n = len(b)
	}
	out := make([]float64, n)
	for i := 0; i < n; i++ {
		if long {
			out[i] = math.Max(a[i], b[i])
		} else {
			out[i] = math.Min(a[i], b[i])
		}
	}
	return out
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

// recentMeanBarRangePct = average per-bar (high−low)/entry over the lookback.
// Used as SL noise floor — NOT window high−low (that confuses trend with noise).
func recentMeanBarRangePct(bars []candles.Candle, i int, entry float64) float64 {
	if !(entry > 0) || i < 2 {
		return PathMinStopPct
	}
	lo := i - PriceRangeLookback
	if lo < 0 {
		lo = 0
	}
	var sum float64
	n := 0
	for _, c := range bars[lo:i] {
		if math.IsNaN(c.High) || math.IsNaN(c.Low) || !(c.High > c.Low) {
			continue
		}
		sum += (c.High - c.Low) / entry * 100
		n++
	}
	if n == 0 {
		return PathMinStopPct
	}
	return sum / float64(n)
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

// truncatePath keeps the first n predicted closes (fixed look-ahead).
func truncatePath(path []float64, n int) []float64 {
	if n <= 0 || n >= len(path) {
		return path
	}
	return path[:n]
}

// truncatePathUntilRelErr keeps predicted closes while |pred−actual|/actual < maxRelErr.
// Length is dynamic (not a fixed 160): stop at the first breach. Used per trade origin.
func truncatePathUntilRelErr(pred, actual []float64, maxRelErr float64) []float64 {
	if maxRelErr <= 0 {
		maxRelErr = 0.01
	}
	n := len(pred)
	if len(actual) < n {
		n = len(actual)
	}
	out := make([]float64, 0, n)
	for i := 0; i < n; i++ {
		p, a := pred[i], actual[i]
		if math.IsNaN(p) || math.IsInf(p, 0) || p <= 0 ||
			math.IsNaN(a) || math.IsInf(a, 0) || a <= 0 {
			break
		}
		if math.Abs(p-a)/a >= maxRelErr {
			break
		}
		out = append(out, p)
	}
	return out
}

// pathDirection picks side from the predicted window: whichever way the path
// moves further from entry. Ties → long.
func pathDirection(path []float64, entry float64) (long bool, peakPct float64) {
	up := rawPathFavorableSpanPct(path, entry, true)
	dn := rawPathFavorableSpanPct(path, entry, false)
	if up >= dn {
		return true, up
	}
	return false, dn
}

// pathInternalSpanPct is (max-min)/entry on the predicted window.
// Flat OLS clouds sit near ref; open/close gaps must not look like a real peak.
func pathInternalSpanPct(path []float64, entry float64) float64 {
	if len(path) == 0 || !(entry > 0) {
		return 0
	}
	lo, hi := math.Inf(1), math.Inf(-1)
	for _, px := range path {
		if math.IsNaN(px) || math.IsInf(px, 0) || px <= 0 {
			continue
		}
		if px < lo {
			lo = px
		}
		if px > hi {
			hi = px
		}
	}
	if !(hi > lo) {
		return 0
	}
	return (hi - lo) / entry * 100
}

// pathAdversePct is the worst predicted move against the position in the window.
func pathAdversePct(path []float64, entry float64, long bool) float64 {
	if len(path) == 0 || !(entry > 0) {
		return 0
	}
	worst := 0.0
	for _, px := range path {
		if math.IsNaN(px) || math.IsInf(px, 0) || px <= 0 {
			continue
		}
		var against float64
		if long {
			against = (1 - px/entry) * 100
		} else {
			against = (px/entry - 1) * 100
		}
		if against > worst {
			worst = against
		}
	}
	return worst
}

// minTradablePeakPct — fee floor for having a directional side.
func minTradablePeakPct(taker, maker float64) float64 {
	feePct := (taker + maker) * 100
	need := (feePct + 0.05) / PathTakeFrac
	if need < PathMinPeakPct {
		need = PathMinPeakPct
	}
	return need
}

// exitsFromPredictedPath — legacy helper for geometry unit tests.
func exitsFromPredictedPath(peakPct, adversePct, errBand, taker, maker float64) exits.Exits {
	_ = errBand
	_ = taker
	_ = maker
	tp := peakPct * PathTakeFrac
	if tp > peakPct {
		tp = peakPct * 0.95
	}
	sl := math.Max(adversePct*1.25+0.05, PathMinStopPct)
	return exits.Exits{StopPct: round2(sl), Targets: [][2]float64{{round2(tp), 1}}}
}

type trainPick struct {
	pathPick
	TrainPnL float64
	TrainN   int
}

// trainSample is one expanding-lookback origin: side from the model, PnL on actuals.
type trainSample struct {
	Long  bool
	Entry float64
	Peak  float64
	Act   []float64
}

// collectTrainSamples builds the TRAIN set once per live origin (past only).
// Side comes from the model at each train origin; paths are actual closes.
func collectTrainSamples(
	bars []candles.Candle,
	models []pricemodel.Model,
	lo, hi, hWin int,
	minPeak float64,
) []trainSample {
	if hi-lo < 16 || hWin < 1 {
		return nil
	}
	span := hi - lo - hWin - 2
	if span < 1 {
		return nil
	}
	step := span / PathTrainSamples
	if step < 1 {
		step = 1
	}
	out := make([]trainSample, 0, PathTrainSamples+2)
	for t := lo + 2; t+hWin < hi; t += step {
		feat, ok := pricemodel.CandleFeatures(bars[t-1], bars[t-2].Close)
		if !ok {
			continue
		}
		ref := bars[t-1].Close
		entry := bars[t].Open
		if !(entry > 0) || !(ref > 0) {
			continue
		}
		raw := predictedCloses(models, ref, feat)
		if len(raw) == 0 {
			continue
		}
		pred := truncatePath(raw, hWin)
		long, peak := pathDirection(pred, entry)
		if peak+1e-12 < minPeak {
			continue
		}
		act := actualCloses(bars, t, hWin)
		if len(act) == 0 {
			continue
		}
		out = append(out, trainSample{Long: long, Entry: entry, Peak: peak, Act: act})
	}
	return out
}

// scoreComboOnSamples sums PnL of one TP×SL×lev on TRAIN actual paths (most gain).
// Does NOT require TP ≤ predicted peak — OLS peaks are tiny; actuals decide.
func scoreComboOnSamples(
	samples []trainSample,
	ex exits.Exits, lev float64,
	balance, maxRiskPct, taker, maker float64,
) (pnl float64, n int) {
	if len(samples) == 0 {
		return 0, 0
	}
	riskPct := impliedRiskPct(ex.StopPct, lev)
	if riskPct > maxRiskPct+1e-12 || comboCanLiquid(ex.StopPct, lev) {
		return 0, 0
	}
	tp := firstTargetPct(ex)
	if tp/ex.StopPct+1e-12 < PathMinTpSlRatio {
		return 0, 0
	}
	for _, s := range samples {
		steps := sizeForLeverage(balance, lev, s.Entry)
		if steps <= 0 || ex.Validate(steps) != nil {
			continue
		}
		sim := simulateExitsOnPath(s.Long, s.Entry, ex, steps, s.Act, taker, maker)
		pnl += sim.PnL
		n++
	}
	return pnl, n
}

// pickExitsByTrainGain scores TP×SL×lev on the expanding TRAIN lookback and
// returns the combo with the most train PnL (most gain). Re-run every origin.
func pickExitsByTrainGain(
	bars []candles.Candle,
	models []pricemodel.Model,
	lookbackStart, origin, hWin int,
	long bool, entry float64, pickPath []float64,
	stops, targets []float64, levs []uint32,
	balance, maxRiskPct, minPeak, taker, maker float64,
) *trainPick {
	samples := collectTrainSamples(bars, models, lookbackStart, origin, hWin, minPeak)
	if len(samples) == 0 {
		return nil
	}
	minN := 5
	if len(samples) < 5 {
		minN = len(samples)
		if minN < 1 {
			minN = 1
		}
	}
	targetRisk := maxRiskPct
	if PathTargetRiskPct > 0 && PathTargetRiskPct < targetRisk {
		targetRisk = PathTargetRiskPct
	}
	// Stride large CLI grids so scoring stays fast.
	stopStride, tpStride := 1, 1
	if len(stops) > 24 {
		stopStride = (len(stops) + 23) / 24
	}
	if len(targets) > 24 {
		tpStride = (len(targets) + 23) / 24
	}
	advShock := adversePath(pickPath, long, 0.01)
	minTP := minPeak * PathTakeFrac
	var best *trainPick
	for si := 0; si < len(stops); si += stopStride {
		stop := stops[si]
		if stop+1e-12 < PathMinStopPct {
			continue
		}
		for ti := 0; ti < len(targets); ti += tpStride {
			tp := targets[ti]
			if tp+1e-12 < minTP {
				continue
			}
			if tp/stop+1e-12 < PathMinTpSlRatio {
				continue
			}
			ex := exits.Exits{StopPct: stop, Targets: [][2]float64{{tp, 1}}}
			for _, levU := range levs {
				lev := float64(levU)
				riskPct := impliedRiskPct(stop, lev)
				if riskPct > targetRisk+1e-12 {
					continue
				}
				if comboCanLiquid(stop, lev) {
					continue
				}
				steps := sizeForLeverage(balance, lev, entry)
				if steps <= 0 || ex.Validate(steps) != nil {
					continue
				}
				if pathAdversePct(advShock, entry, long) >= liquidPctAtLev(lev)*0.85 {
					continue
				}
				tpnl, tn := scoreComboOnSamples(samples, ex, lev, balance, targetRisk, taker, maker)
				if tn < minN {
					continue
				}
				mid := simulateExitsOnPath(long, entry, ex, steps, pickPath, taker, maker)
				pathR := mid.PnL / math.Max(balance*riskPct/100, 1e-9)
				cand := trainPick{
					pathPick: pathPick{
						Exits: ex, RiskPct: riskPct, Steps: steps, Lev: lev,
						PathPnL: mid.PnL, WorstPnL: tpnl, PathR: pathR,
						LiquidDist: liquidDistancePct(stop, lev), ExitH: mid.ExitH,
					},
					TrainPnL: tpnl, TrainN: tn,
				}
				if cand.ExitH < 1 {
					cand.ExitH = 1
				}
				better := best == nil
				if !better {
					if tpnl > best.TrainPnL+1e-9 {
						better = true
					} else if math.Abs(tpnl-best.TrainPnL) <= 1e-9 && mid.PnL > best.PathPnL+1e-9 {
						better = true
					} else if math.Abs(tpnl-best.TrainPnL) <= 1e-9 && math.Abs(mid.PnL-best.PathPnL) <= 1e-9 &&
						lev < best.Lev-1e-12 {
						better = true
					}
				}
				if better {
					cp := cand
					best = &cp
				}
			}
		}
	}
	return best
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
	return exitCandidatesGrid([]float64{stopPct}, nil)
}

// exitCandidatesGrid builds the full SL×TP asymmetric grid (e.g. 0.1 TP with 0.9 SL).
// When targets is empty, falls back to TP=SL plus default R-multiple ladders.
func exitCandidatesGrid(stops, targets []float64) []exits.Exits {
	var out []exits.Exits
	seen := map[string]bool{}
	add := func(e exits.Exits) {
		if e.Validate(10_000) != nil {
			return
		}
		k := e.Label()
		if seen[k] {
			return
		}
		seen[k] = true
		out = append(out, e)
	}
	for _, stop := range stops {
		if len(targets) == 0 {
			add(exits.Exits{StopPct: stop, Targets: [][2]float64{{stop, 1}}})
			for _, p := range exits.DefaultScaleoutProfiles() {
				if len(p.LadderR) == 1 && math.Abs(p.LadderR[0]-1) < 1e-12 {
					continue
				}
				add(p.ToExits(stop))
			}
			continue
		}
		for _, tp := range targets {
			add(exits.Exits{StopPct: stop, Targets: [][2]float64{{tp, 1}}})
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

// forwardOriginIndex = last bar strictly before tradeEnd (same origin as FORWARD path).
func forwardOriginIndex(bars []candles.Candle, tradeEndMs int64) int {
	origin := -1
	for t := 1; t < len(bars); t++ {
		if bars[t].TimeMs < tradeEndMs {
			origin = t
		}
	}
	return origin
}

// extendBarsWithOfficial appends OKX candles after the tape so FORWARD trades have actuals.
func extendBarsWithOfficial(bars []candles.Candle, barMs int64, nMore int) []candles.Candle {
	if len(bars) == 0 || nMore <= 0 {
		return bars
	}
	last := bars[len(bars)-1].TimeMs
	from := last + barMs
	to := last + int64(nMore)*barMs
	okx := softOfficialCandles(barMs, from, to)
	if len(okx) == 0 {
		return bars
	}
	out := append([]candles.Candle{}, bars...)
	for _, c := range okx {
		if c.TimeMs > last {
			out = append(out, c)
			last = c.TimeMs
		}
	}
	return out
}

// runPriceStrategyPerTrade picks per-trade exits.
// mode 1 = oracle (peek actual closes) over the OOS TRADE window;
// mode 2 = live predicted: TRAIN before tradeStart; discover N/H under 1%;
//   best TP×SL×lev on predicted path → trade → append actuals → re-validate H
//   + re-pick until trade_end (stable expanding N).
func runPriceStrategyPerTrade(
	args Args,
	minutes []candles.Candle,
	models []pricemodel.Model,
	label string,
	barMs int64,
	tradeStartMs, tradeEndMs int64,
	taker, maker float64,
	pathWindow int,
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

	if len(models) <= 1 {
		return nil, fmt.Errorf("%s: need horizon models for strategy", label)
	}
	h1 := models[1]
	hMax := len(models) - 1
	if pathWindow <= 0 || pathWindow > hMax {
		pathWindow = hMax
	}
	bars := candles.Aggregate(minutes, barMs)
	if len(bars) < 3 {
		return nil, fmt.Errorf("%s: not enough frame candles for strategy", label)
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
	targets := args.PriceTargets
	var usable, skippedFee []float64
	for _, s := range args.PriceStops {
		// Oracle: keep TP=SL fee-floor.
		if oracle && breakevenWinTpEqSl(s, taker, maker) > PriceMaxBreakevenWin {
			skippedFee = append(skippedFee, s)
			continue
		}
		usable = append(usable, s)
	}
	if len(skippedFee) > 0 {
		logx.Infof("  %s fee-floor skip stops %v (need win > %.0f%% for TP=SL)", label, skippedFee, 100*PriceMaxBreakevenWin)
	}

	if len(usable) == 0 {
		return nil, fmt.Errorf("%s: no usable stops", label)
	}
	exitGrid := exitCandidatesGrid(usable, targets)
	if len(exitGrid) == 0 {
		return nil, fmt.Errorf("%s: empty TP/SL grid", label)
	}

	oosLo, oosHi := frameBarRange(bars, tradeStartMs, tradeEndMs)
	if oosLo >= oosHi {
		return nil, fmt.Errorf("%s: empty OOS / TRADE bar range", label)
	}

	if oracle {
		signals := priceDirSignalsDebounced(h1, bars, PriceSignalConfirmBars)
		return runOracleGrid(args, bars, models, label, oosLo, oosHi, signals, usable, targets, exitGrid,
			maxLev, maxRiskPct, errBand, taker, maker, skippedFee)
	}

	// OOS TRADE month only (trainCutoff == tradeStart). FORWARD chart = demo.
	logx.Infof("  %s OOS TRADE predicted: %s → %s · bars %d..%d · max_models=%d (per-origin until err≥%.2f%%, trade after origin, no liquid, lev≤%.0f)",
		label, utc(tradeStartMs), utc(tradeEndMs), oosLo, oosHi, pathWindow, errBand*100, math.Min(maxLev, PathLiveMaxLev))

	return runRollingPredicted(args, bars, models, label, oosLo, oosHi, pathWindow,
		usable, targets, exitGrid, maxLev, maxRiskPct, errBand, taker, maker)
}

// runOracleGrid: research ceiling — peek actuals, pick best TP/SL/lev per debounce signal.
func runOracleGrid(
	args Args,
	bars []candles.Candle,
	models []pricemodel.Model,
	label string,
	oosLo, oosHi int,
	signals [][2]int,
	usable, targets []float64,
	exitGrid []exits.Exits,
	maxLev, maxRiskPct, errBand, taker, maker float64,
	skippedFee []float64,
) (map[string]any, error) {
	hMax := len(models) - 1
	modeName := "oracle"
	logx.Infof("  %s per-trade exits (%s): H=%d · stops=%d · tps=%d · combos=%d · lev=%v · max_risk=%g%% · debounce=%d · signals=%d · OOS %d..%d",
		label, modeName, hMax, len(usable), len(targets), len(exitGrid), args.PriceLeverage, maxRiskPct,
		PriceSignalConfirmBars, len(signals), oosLo, oosHi)

	balance := args.BalanceUSDT
	running := 0.0
	freeAfter := oosLo
	var tradesJSON []map[string]any
	var wins uint32
	skippedLiquid := 0
	signalSeen := 0

	for _, sig := range signals {
		i, dir := sig[0], sig[1]
		if i < oosLo || i >= oosHi || i < 2 || i < freeAfter {
			continue
		}
		signalSeen++
		entry := bars[i].Open
		if !(entry > 0) || math.IsNaN(entry) || math.IsInf(entry, 0) {
			continue
		}
		long := dir > 0
		actPath := actualCloses(bars, i, hMax)
		if len(actPath) == 0 {
			continue
		}

		var best *pathPick
		for _, ex := range exitGrid {
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
				sim := simulateExitsOnPath(long, entry, ex, steps, actPath, taker, maker)
				if sim.PnL <= 0 {
					continue
				}
				pathR := sim.PnL / math.Max(balance*riskPct/100, 1e-9)
				dist := liquidDistancePct(ex.StopPct, lev)
				better := best == nil ||
					sim.PnL > best.PathPnL+1e-9 ||
					(math.Abs(sim.PnL-best.PathPnL) <= 1e-9 && pathR > best.PathR+1e-9) ||
					(math.Abs(sim.PnL-best.PathPnL) <= 1e-9 && math.Abs(pathR-best.PathR) <= 1e-9 && dist > best.LiquidDist)
				if better {
					cp := pathPick{
						Exits: ex, RiskPct: riskPct, Steps: steps, Lev: lev,
						PathPnL: sim.PnL, WorstPnL: sim.PnL, PathR: pathR, LiquidDist: dist,
					}
					best = &cp
				}
			}
		}
		if best == nil || best.Steps <= 0 {
			continue
		}
		realSim := simulateExitsOnPath(long, entry, best.Exits, best.Steps, actPath, taker, maker)
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
		tradesJSON = append(tradesJSON, map[string]any{
			"side": side, "entry_at": utc(bars[i].TimeMs), "entry_bar": i,
			"entry_price": round2(entry), "exits": exitsToJSON(best.Exits),
			"stop_pct": best.Exits.StopPct, "risk_pct": best.RiskPct,
			"leverage": round2(best.Lev*100) / 100,
			"size_btc": round4(float64(best.Steps) * StepBTC),
			"liquid_distance_pct": round2(best.LiquidDist),
			"path_pnl": round2(best.PathPnL), "path_r": round2(best.PathR*100) / 100,
			"path_worst_pnl": round2(best.WorstPnL),
			"path_reason": realSim.Reason, "path_exit_h": realSim.ExitH,
			"path_source": "actual_oracle", "path_span_pct": 0,
			"pnl": round2(pnl), "reason": realSim.Reason, "exit_h": realSim.ExitH,
			"fees": round2(realSim.Fees), "running_pnl": round2(running),
			"balance": round2(balance),
			"pick_mode": modeName,
			"in_band": true, "band_dev": 0,
			"oracle_pnl": round2(pnl),
			"oracle_exits": best.Exits.Label(),
			"oracle_note":  "traded combo = best TP/SL/lev on actual future closes (oracle / peek)",
			"fills":        fills,
		})
	}

	n := uint32(len(tradesJSON))
	logx.Infof("  %s per-trade done (%s): %d trades · PnL %+.2f · signals_in_oos=%d · liquid_skips≈%d · win_rate=%.1f%%",
		label, modeName, n, running, signalSeen, skippedLiquid, 100*float64(wins)/math.Max(float64(n), 1))

	winRate := 0.0
	if n > 0 {
		winRate = round2(100 * float64(wins) / float64(n))
	}
	return map[string]any{
		"selection_rule": fmt.Sprintf(
			"per signal: grid stop×ladder×lev (liquid-safe, stop×lev≤%g%%); pick max PnL on actual future closes; skip if no profitable combo",
			maxRiskPct,
		),
		"summary": map[string]any{
			"exits_mode": "per_trade", "pick_mode": modeName, "horizon": hMax,
			"confirm_bars": PriceSignalConfirmBars, "max_leverage": maxLev,
			"err_band": errBand, "grid_stops": len(usable), "grid_tps": len(targets),
			"grid_combos": len(exitGrid),
			"in_band_trades": n, "in_band_losses": 0,
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
			"liquid": skippedLiquid, "tp_floor": 0, "robust": 0, "flat": 0,
		},
		"trades": tradesJSON,
	}, nil
}

// runRollingPredicted — confirmed IS/OOS loop:
//   Origin at latest IS → discover lookback N + under-1% H from data →
//   predict H → SCORE TP×SL×lev on expanding TRAIN lookback (most train PnL) →
//   trade AFTER origin → APPEND actual entry→exit candles into lookback →
//   re-validate H + re-score combo (stable expanding/rolling N; widen N only if
//   H collapses) → repeat until trade_end. Flat forecast = sit out.
func runRollingPredicted(
	args Args,
	bars []candles.Candle,
	models []pricemodel.Model,
	label string,
	oosLo, oosHi, pathWindow int,
	usable, targets []float64,
	exitGrid []exits.Exits,
	maxLev, maxRiskPct, errBand, taker, maker float64,
) (map[string]any, error) {
	hMax := len(models) - 1
	modeName := "rolling-predicted"
	liveMaxLev := math.Min(maxLev, PathLiveMaxLev)
	levs := make([]uint32, 0, len(args.PriceLeverage))
	for _, l := range args.PriceLeverage {
		if l >= 1 && float64(l) <= liveMaxLev+1e-12 {
			levs = append(levs, l)
		}
	}
	if len(levs) == 0 {
		levs = []uint32{1}
	}
	sort.Slice(levs, func(i, j int) bool { return levs[i] < levs[j] })

	minPeak := minTradablePeakPct(taker, maker)

	// Origin = latest IS bar (just before TRADE). Discover N/H from past data only.
	isOrigin := oosLo
	if isOrigin < 2 {
		isOrigin = 2
	}
	lookbackN, liveH := discoverLookbackNH(models, bars, isOrigin, errBand, pathWindow)
	if liveH < 1 {
		liveH = max(pathWindow, 1)
	}
	if liveH > hMax {
		liveH = hMax
	}
	// Expanding window start: keep size-class N behind the first IS origin.
	lookbackStart := isOrigin - lookbackN
	if lookbackStart < 1 {
		lookbackStart = 1
	}
	logx.Infof("  %s rolling-predicted (live): N=%d H=%d (models≤%d) · stops=%d · tps=%d · combos=%d · lev=%v (cap %.0f) · max_risk=%g%% · err_band=%.2f%% · min_peak=%.2f%% · exits=train_score · OOS %d..%d",
		label, lookbackN, liveH, hMax, len(usable), len(targets), len(exitGrid), levs, liveMaxLev, maxRiskPct, errBand*100, minPeak, oosLo, oosHi)

	balance := args.BalanceUSDT
	running := 0.0
	var tradesJSON []map[string]any
	var wins uint32
	skippedLiquid := 0
	skippedNoCombo := 0
	skippedFlat := 0
	skippedDisagree := 0
	nWiden := 0
	origins := 0
	inBandN, inBandLoss := 0, 0

	for i := oosLo; i < oosHi; {
		if i < 2 {
			i++
			continue
		}

		// Re-validate under-1% H on expanding lookback (past only; includes appended actuals).
		// Rolling size = bars from lookbackStart..i; N class stays unless H collapses.
		expandN := i - lookbackStart
		if expandN < lookbackN {
			expandN = lookbackN
		}
		hNow := under1PctHorizonLookback(models, bars, i, expandN, errBand)
		if hNow < 1 {
			hNow = liveH // keep last known good until data says otherwise
		}
		if hNow > hMax {
			hNow = hMax
		}
		// H collapsed → widen N (option B: do not freeze first H; do not re-brute N every trade).
		if hNow < PathMinHorizonH && i > isOrigin {
			wider := widenLookbackN(lookbackN, i, i-1)
			if wider > lookbackN {
				lookbackN = wider
				nWiden++
				newStart := isOrigin - lookbackN
				if newStart < 1 {
					newStart = 1
				}
				lookbackStart = newStart
				expandN = i - lookbackStart
				if expandN < lookbackN {
					expandN = lookbackN
				}
				h2 := under1PctHorizonLookback(models, bars, i, expandN, errBand)
				if h2 > hNow {
					hNow = h2
				}
				logx.Infof("  %s H collapsed → widen N=%d → H=%d @ bar %d", label, lookbackN, hNow, i)
			}
		}
		if hNow < 1 {
			hNow = 1
		}
		liveH = hNow

		feat, ok := pricemodel.CandleFeatures(bars[i-1], bars[i-2].Close)
		if !ok {
			i++
			continue
		}
		refPx := bars[i-1].Close
		entry := bars[i].Open
		if !(entry > 0) || math.IsNaN(entry) || math.IsInf(entry, 0) ||
			!(refPx > 0) || math.IsNaN(refPx) || math.IsInf(refPx, 0) {
			i++
			continue
		}

		// Predict from older bar (ref = i-1); trade enters on bar i (after origin).
		rawFull := predictedCloses(models, refPx, feat)
		if len(rawFull) == 0 {
			i++
			continue
		}
		// Look-ahead = re-validated under-1% H (no future-actual truncation).
		pickPath := truncatePath(rawFull, liveH)
		origins++
		if len(pickPath) == 0 {
			i++
			continue
		}
		actPath := actualCloses(bars, i, len(pickPath))
		if len(actPath) == 0 {
			i++
			continue
		}

		long, peak := pathDirection(pickPath, entry)
		pathSrc := "train_score_tp_sl_lev"
		if peak+1e-12 < minPeak {
			skippedFlat++
			i++
			continue
		}
		predOriginMs := bars[i-1].TimeMs
		predOriginPx := refPx

		// Score TP×SL×lev on expanding TRAIN lookback (actual paths) — most gain.
		// Entry = predicted SIDE with fee-floor peak; exits are NOT static.
		best := pickExitsByTrainGain(
			bars, models, lookbackStart, i, liveH,
			long, entry, pickPath,
			usable, targets, levs,
			balance, maxRiskPct, minPeak, taker, maker,
		)
		if best == nil || best.Steps <= 0 {
			skippedNoCombo++
			i++
			continue
		}

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

		nCmp := exitH
		if nCmp > len(pickPath) {
			nCmp = len(pickPath)
		}
		if nCmp > len(actPath) {
			nCmp = len(actPath)
		}
		bandDev := maxPredActualRelDev(pickPath[:nCmp], actPath[:nCmp])
		inBand := bandDev <= errBand+1e-12
		if inBand {
			inBandN++
			if pnl < 0 {
				inBandLoss++
			}
		}

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
		note := fmt.Sprintf(
			"N=%d H=%d predict@%s → peak=%.3f%% → train_score %s train_pnl=%+.2f n=%d mid=%+.2f → trade → append %d actuals; in_band=%v max_dev=%.3f%%",
			lookbackN, liveH, utc(predOriginMs), peak, best.Exits.Label(), best.TrainPnL, best.TrainN, best.PathPnL, exitH, inBand, bandDev*100,
		)
		tradesJSON = append(tradesJSON, map[string]any{
			"side": side,
			"predict_at": utc(predOriginMs), "predict_close": round2(predOriginPx),
			"entry_at": utc(bars[i].TimeMs), "entry_bar": i,
			"entry_price": round2(entry), "exits": exitsToJSON(best.Exits),
			"stop_pct": best.Exits.StopPct, "risk_pct": best.RiskPct,
			"leverage": round2(best.Lev*100) / 100,
			"size_btc": round4(float64(best.Steps) * StepBTC),
			"liquid_distance_pct": round2(best.LiquidDist),
			"path_pnl": round2(pathSimR.PnL), "path_r": round2(best.PathR*100) / 100,
			"path_worst_pnl": round2(best.WorstPnL),
			"train_pnl": round2(best.TrainPnL), "train_n": best.TrainN,
			"path_reason": pathSimR.Reason, "path_exit_h": pathSimR.ExitH,
			"path_source": pathSrc, "path_span_pct": round2(peak*100) / 100,
			"path_window": len(pickPath),
			"lookback_n": lookbackN, "horizon_h": liveH,
			"pnl": round2(pnl), "reason": realSim.Reason, "exit_h": realSim.ExitH,
			"fees": round2(realSim.Fees), "running_pnl": round2(running),
			"balance": round2(balance),
			"pick_mode": modeName,
			"in_band": inBand, "band_dev": round2(bandDev*10000) / 10000,
			"oracle_pnl": round2(pnl),
			"oracle_exits": best.Exits.Label(),
			"oracle_note":  note,
			"fills":        fills,
		})

		// APPEND actual entry→exit candles into the expanding lookback, then continue.
		// A loss does not halt the bot; next origin re-validates H + re-picks combo.
		i = i + exitH
	}

	n := uint32(len(tradesJSON))
	winRate := 0.0
	if n > 0 {
		winRate = round2(100 * float64(wins) / float64(n))
	}
	logx.Infof("  %s rolling-predicted done: %d trades · PnL %+.2f · origins=%d · win_rate=%.1f%% · N=%d H=%d · flat≈%d · no_combo≈%d · liquid≈%d · N_widen=%d · in_band=%d in_band_loss=%d",
		label, n, running, origins, winRate, lookbackN, liveH, skippedFlat, skippedNoCombo, skippedLiquid, nWiden, inBandN, inBandLoss)
	if n == 0 {
		logx.Warnf("  %s live predicted: 0 trades (flat_peak<%.2f%% → %d · no_combo=%d). Sitting out flat forecasts is correct for live.",
			label, minPeak, skippedFlat, skippedNoCombo)
	}

	rule := fmt.Sprintf(
		"IS origin → N=%d under-%.2f%% H → side from pred (peak≥%.2f%% fees); SCORE TP×SL×lev on TRAIN lookback (most train PnL) → append → re-score until trade_end",
		lookbackN, errBand*100, minPeak,
	)
	return map[string]any{
		"selection_rule": rule,
		"summary": map[string]any{
			"exits_mode": "train_score", "pick_mode": modeName, "horizon": hMax,
			"lookback_n": lookbackN, "path_window": liveH, "confirm_bars": 0, "max_leverage": liveMaxLev,
			"err_band": errBand, "min_peak_pct": minPeak, "grid_stops": len(usable), "grid_tps": len(targets),
			"grid_combos": len(exitGrid), "n_widen": nWiden,
			"in_band_trades": inBandN, "in_band_losses": inBandLoss,
			"taker_fee_bps": taker * 10_000, "maker_fee_bps": maker * 10_000,
			"balance_start": args.BalanceUSDT, "balance_end": round2(balance),
		},
		"total_pnl": round2(running), "closed_trades": n, "wins": wins, "win_rate": winRate,
		"signal_count": origins, "signals_in_oos": origins,
		"oos_bars": []int{oosLo, oosHi},
		"fee_floor": map[string]any{
			"max_breakeven_win_pct": 100 * PriceMaxBreakevenWin,
			"skipped_stops":        []float64{},
			"usable_stops":         usable,
		},
		"skips": map[string]any{
			"liquid": skippedLiquid, "tp_floor": 0, "robust": skippedNoCombo, "flat": skippedFlat,
			"h1_disagree": skippedDisagree, "n_widen": nWiden,
		},
		"trades": tradesJSON,
	}, nil
}

func round2(x float64) float64 { return math.Round(x*100) / 100 }
func round4(x float64) float64 { return math.Round(x*10_000) / 10_000 }
