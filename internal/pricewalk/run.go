package pricewalk

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/config"
	"github.com/ramtin/dynamic-trader/internal/logx"
	"github.com/ramtin/dynamic-trader/internal/pricemodel"
	"github.com/ramtin/dynamic-trader/internal/report"
	"github.com/ramtin/dynamic-trader/internal/score"
)

// Meta is the window_scan meta.json subset needed by the price path.
type Meta struct {
	Start        string   `json:"start"`
	End          string   `json:"end"`
	Days         uint64   `json:"days"`
	Hours        *uint64  `json:"hours"`
	Windows      []Window `json:"windows"`
	SizeBTC      float64  `json:"size_btc"`
	TakerFeeBps  float64  `json:"taker_fee_bps"`
	MakerFeeBps  float64  `json:"maker_fee_bps"`
	EveryPair    int      `json:"every_pair"`
	Timeframes   []int    `json:"timeframes"`
	Lengths      []int    `json:"lengths"`
}

type Window struct {
	Start string `json:"start"`
	End   string `json:"end"`
	File  string `json:"file"`
}

type priceBarRun struct {
	Label           string
	BarMs           int64
	Locked          bool
	FitPairs        int
	ReturnMAE       float64
	PriceRelErr     float64
	BaselineRelErr  float64
	TotalPnL        float64
	NSignals        int
	Report          map[string]any
}

// Run is the entry point for dt price-walk.
func Run(args Args) error {
	if args.HTMLOnly {
		return regenerateHTML(args.Scan)
	}
	cfg, err := config.Load(args.Config)
	if err != nil {
		return err
	}
	meta, err := loadMeta(args.Scan)
	if err != nil {
		return err
	}
	if meta.EveryPair != 1 {
		return fmt.Errorf("window_scan ran on every %dth RSI pair only; run it on all of them", meta.EveryPair)
	}
	if meta.TakerFeeBps != cfg.Venues.OKX.TakerFeeBps || meta.MakerFeeBps != cfg.Venues.OKX.MakerFeeBps {
		return fmt.Errorf("window_scan used other fees than %s", args.Config)
	}
	return runPriceOnly(args, cfg, meta)
}

func loadMeta(scan string) (Meta, error) {
	raw, err := os.ReadFile(filepath.Join(scan, "meta.json"))
	if err != nil {
		return Meta{}, fmt.Errorf("no finished scan in %s: window_scan writes meta.json when it is done: %w", scan, err)
	}
	var m Meta
	if err := json.Unmarshal(raw, &m); err != nil {
		return Meta{}, err
	}
	return m, nil
}

func regenerateHTML(scan string) error {
	entries, err := os.ReadDir(scan)
	if err != nil {
		return err
	}
	n := 0
	for _, e := range entries {
		name := e.Name()
		if !strings.HasSuffix(name, ".json") {
			continue
		}
		if !strings.HasPrefix(name, "nm_price_walk_forward") && !strings.HasPrefix(name, "nm_walk_forward") {
			continue
		}
		if err := report.FromJSON(filepath.Join(scan, name)); err != nil {
			return err
		}
		n++
		logx.Infof("regenerated HTML for %s", name)
	}
	if n == 0 {
		return fmt.Errorf("no nm_*_walk_forward*.json in %s", scan)
	}
	return nil
}

func dayMs(day string) (int64, error) {
	// Accept YYYY-MM-DD or full timestamps; day floor for NaiveDate fields.
	if t, err := time.ParseInLocation("2006-01-02", day, time.UTC); err == nil {
		return t.UnixMilli(), nil
	}
	return candles.ParseWindowMs(day)
}

func runPriceOnly(args Args, cfg *config.App, meta Meta) error {
	lookbackMs := int64(args.LookbackDays) * 86_400_000
	startMs, err := dayMs(meta.Start)
	if err != nil {
		return fmt.Errorf("meta.start: %w", err)
	}
	endDayMs, err := dayMs(meta.End)
	if err != nil {
		return fmt.Errorf("meta.end: %w", err)
	}
	wantedFrom := startMs - lookbackMs
	if wantedFrom < 0 {
		wantedFrom = 0
	}
	tapeFrom := wantedFrom
	if tapeFrom < startMs {
		tapeFrom = startMs
	}
	scanEndMs := endDayMs
	trainCutoff := scanEndMs - int64(args.ReserveTradeDays)*86_400_000
	if len(meta.Windows) < 2 {
		return fmt.Errorf("need at least 2 windows in meta.json")
	}
	firstPickMs, err := candles.ParseWindowMs(meta.Windows[1].Start)
	if err != nil {
		return err
	}
	if trainCutoff <= firstPickMs {
		return fmt.Errorf("--reserve-trade-days leaves no TRAIN window before TRADE")
	}
	var trainWs []int
	for w := 1; w < len(meta.Windows); w++ {
		end, err := candles.ParseWindowMs(meta.Windows[w].End)
		if err != nil {
			return err
		}
		if end > trainCutoff {
			break
		}
		trainWs = append(trainWs, w)
	}
	if len(trainWs) == 0 {
		return fmt.Errorf("no TRAIN windows before TRADE cutoff")
	}
	priceTradeStart := trainCutoff
	if args.FirstChoice != "" {
		floor, err := dayMs(args.FirstChoice)
		if err != nil {
			return err
		}
		if floor > priceTradeStart {
			priceTradeStart = floor
		}
	}
	priceTradeEnd := scanEndMs
	if args.TradeDays > 0 {
		priceTradeEnd = priceTradeStart + int64(args.TradeDays)*86_400_000
		if priceTradeEnd > scanEndMs {
			priceTradeEnd = scanEndMs
		}
	}

	logx.Infof("PRICE-ONLY fast path (no tick tape / no finalists). Cache under %s/.cache/", args.Scan)
	// Rust: load_minutes_cached(data, scan, tape_from_ms, ms(meta.end)).
	minutes, err := candles.LoadMinutes(args.Data, args.Scan, tapeFrom, endDayMs)
	if err != nil {
		return err
	}

	taker := cfg.Taker()
	maker := cfg.Maker()
	return runPricePath(args, meta, minutes, trainWs, trainCutoff, priceTradeStart, priceTradeEnd, args.LookbackDays, taker, maker)
}

func runPricePath(
	args Args,
	meta Meta,
	minutes []candles.Candle,
	trainWs []int,
	trainCutoff, tradeStart, tradeEnd int64,
	lookbackDays uint64,
	taker, maker float64,
) error {
	var labels []string
	for _, b := range args.PriceBars {
		labels = append(labels, b.Label)
	}
	barList := strings.Join(labels, ",")
	logx.Infof("\n=== PRICE PATH (candle grid) ===\n"+
		"Equation: (close_t/close_{t-1})-1 = a·prev_ret + b·range + c·body + d·typical_dev\n"+
		"pred_close = close_{t-1}·(1+Σαf)   |  OHLC from market tape\n"+
		"Bars: [%s]  ·  gate return-MAE < %.0f%% + beat pred=ref · ≥%d points\n"+
		"TRADE %s → %s",
		barList, args.MaxRelErr*100, args.MinFitPairs, utc(tradeStart), utc(tradeEnd))

	var runs []priceBarRun
	for _, b := range args.PriceBars {
		run, err := runOnePriceBar(args, meta, minutes, trainWs, trainCutoff, tradeStart, tradeEnd, lookbackDays, b.Label, b.BarMs, taker, maker)
		if err != nil {
			logx.Warnf("  [%s] skipped: %v", b.Label, err)
			continue
		}
		logx.Infof("  [%s] done: locked=%v return_mae=%.3f%% pnl=%+.2f signals=%d",
			b.Label, run.Locked, run.ReturnMAE*100, run.TotalPnL, run.NSignals)
		runs = append(runs, run)
	}
	if len(runs) == 0 {
		return fmt.Errorf("price bar grid produced no usable runs")
	}

	gridSummary := make([]map[string]any, 0, len(runs))
	for _, r := range runs {
		gridSummary = append(gridSummary, map[string]any{
			"bar": r.Label, "bar_ms": r.BarMs, "locked": r.Locked,
			"fit_pairs": r.FitPairs, "return_mae": r.ReturnMAE,
			"price_rel_err": r.PriceRelErr, "baseline_rel_err": r.BaselineRelErr,
			"total_pnl": r.TotalPnL, "signals": r.NSignals,
		})
	}

	var lockedIdx []int
	for i, r := range runs {
		if r.Locked {
			lockedIdx = append(lockedIdx, i)
		}
	}
	pickBest := func(idxs []int) int {
		best := idxs[0]
		for _, i := range idxs[1:] {
			if runs[i].TotalPnL > runs[best].TotalPnL ||
				(runs[i].TotalPnL == runs[best].TotalPnL && runs[i].ReturnMAE < runs[best].ReturnMAE) {
				best = i
			}
		}
		return best
	}
	var bestIdx int
	if len(lockedIdx) > 0 {
		bestIdx = pickBest(lockedIdx)
	} else if args.RequireFit {
		return fmt.Errorf("price TRAIN never LOCKed on any bar in [%s] (need return_mae < %.0f%% and ≥%d points)",
			barList, args.MaxRelErr*100, args.MinFitPairs)
	} else {
		logx.Warnf("  warning: no bar LOCKed; --require-fit 0 → pick best OOS strategy PnL anyway")
		all := make([]int, len(runs))
		for i := range runs {
			all[i] = i
		}
		bestIdx = pickBest(all)
	}
	selected := runs[bestIdx].Label
	for i := range runs {
		runs[i].Report["meta"].(map[string]any)["bar_grid"] = gridSummary
		runs[i].Report["meta"].(map[string]any)["selected_bar"] = selected
		runs[i].Report["meta"].(map[string]any)["is_selected"] = i == bestIdx
		stemBase := "nm_price_walk_forward"
		if args.PriceStrategyMode == 2 {
			stemBase = "nm_price_walk_forward_predicted"
		}
		stem := stemBase
		if i != bestIdx {
			stem = stemBase + "_" + runs[i].Label
		}
		payload, err := json.MarshalIndent(runs[i].Report, "", "  ")
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(args.Scan, stem+".json"), payload, 0o644); err != nil {
			return err
		}
		if err := report.WritePriceHTML(args.Scan, stem, runs[i].Report); err != nil {
			return err
		}
		tag := "grid"
		if i == bestIdx {
			tag = "SELECTED"
		}
		logx.Infof("  wrote %s (%s)", filepath.Join(args.Scan, stem+".html"), tag)
	}
	best := runs[bestIdx]
	logx.Infof("PRICE PATH done: selected %s → %+.2f USDT · return_mae=%.3f%% · %d signals",
		best.Label, best.TotalPnL, best.ReturnMAE*100, best.NSignals)
	return nil
}

func runOnePriceBar(
	args Args,
	meta Meta,
	minutes []candles.Candle,
	trainWs []int,
	trainCutoff, tradeStart, tradeEnd int64,
	lookbackDays uint64,
	label string,
	barMs int64,
	taker, maker float64,
) (priceBarRun, error) {
	typical := priceAt(minutes, trainCutoff)
	priceModel := pricemodel.ColdStart()
	var fitPairs []pricemodel.Sample
	var fitTrace []map[string]any
	locked := false
	lockMs := trainCutoff

	bars := candles.Aggregate(minutes, barMs)
	allSamples := pricemodel.SamplesFromCandles(bars)
	if len(allSamples) < score.MinFitSamples {
		return priceBarRun{}, fmt.Errorf("not enough %s candle samples on tape (%d)", label, len(allSamples))
	}

	logx.Infof("\n--- price bar %s (%d bars · %d samples) ---", label, len(bars), len(allSamples))
	lastTrain := meta.Windows[trainWs[len(trainWs)-1]].Start
	logx.Infof("  fit near→far (%d train days, newest first = %s)…", len(trainWs), lastTrain)

	// Near→far: reverse trainWs (newest first in Rust is iter().rev()).
	for k := len(trainWs) - 1; k >= 0; k-- {
		w := trainWs[k]
		t0 := time.Now()
		window := meta.Windows[w]
		start, err := candles.ParseWindowMs(window.Start)
		if err != nil {
			return priceBarRun{}, err
		}
		end, err := candles.ParseWindowMs(window.End)
		if err != nil {
			return priceBarRun{}, err
		}
		if end > trainCutoff {
			continue
		}
		var batch []pricemodel.Sample
		for _, s := range allSamples {
			if s.AtMs >= start && s.AtMs < end {
				batch = append(batch, s)
			}
		}
		added := len(batch)
		if added == 0 {
			continue
		}
		fitPairs = append(fitPairs, batch...)
		if args.MaxFitPairs > 0 && len(fitPairs) > args.MaxFitPairs {
			fitPairs = fitPairs[:args.MaxFitPairs]
		}
		pairsOK := len(fitPairs) >= args.MinFitPairs
		if len(fitPairs) < score.MinFitSamples {
			logx.Infof("  %s: %s +%d → points=%d (need ≥%d) in %.0fs",
				window.Start, label, added, len(fitPairs), score.MinFitSamples, time.Since(t0).Seconds())
			continue
		}
		m, err := pricemodel.Fit(fitPairs, &priceModel)
		if err != nil {
			logx.Infof("  %s: %s fit failed (%v); keep going back", window.Start, label, err)
			continue
		}
		prev := priceModel.RelErr
		priceModel = m
		logx.Infof("  %s: %s +%d → points=%d/%d | return_mae=%.3f%% (was %.3f%%) price_err=%.3f%% baseline=%.3f%% in %.0fs",
			window.Start, label, added, len(fitPairs), args.MinFitPairs,
			priceModel.RelErr*100, prev*100, priceModel.PriceRelErr*100, priceModel.BaselineRelErr*100,
			time.Since(t0).Seconds())
		entry := map[string]any{
			"phase": "price_candle_near_to_far", "bar": label, "label": window.Start,
			"alphas": priceModel.Alphas, "return_mae": priceModel.RelErr,
			"price_rel_err": priceModel.PriceRelErr, "baseline_rel_err": priceModel.BaselineRelErr,
			"fit_pairs": len(fitPairs), "locked": false,
		}
		fitTrace = append(fitTrace, entry)
		if priceModel.Ok(args.MaxRelErr) {
			if pairsOK {
				locked = true
				lockMs = trainCutoff
				logx.Infof("  %s LOCKED (back to %s) — return_mae=%.3f%% < %.0f%% beats baseline %.3f%% (n=%d)",
					label, window.Start, priceModel.RelErr*100, args.MaxRelErr*100,
					priceModel.BaselineRelErr*100, len(fitPairs))
				fitTrace[len(fitTrace)-1]["locked"] = true
				break
			}
			logx.Infof("  … gate ok but need more points (%d/%d) — go further back", len(fitPairs), args.MinFitPairs)
		} else if priceModel.RelErr < args.MaxRelErr {
			logx.Infof("  … return_mae under gate but does not beat baseline (%.3f%% ≥ %.3f%%) — go further back",
				priceModel.PriceRelErr*100, priceModel.BaselineRelErr*100)
		} else {
			logx.Infof("  … return_mae still above %.0f%% — go further back", args.MaxRelErr*100)
		}
	}

	if !locked {
		logx.Infof("  %s: not LOCKed (cold=%v return_mae=%.3f%% n=%d)",
			label, priceModel.ColdStart, priceModel.RelErr*100, len(fitPairs))
	}
	_ = typical

	horizons, err := evalPriceHorizons(
		priceModel, bars, max(args.PriceHorizon, 1), barMs,
		trainCutoff, tradeStart, tradeEnd, args.MaxRelErr, args.PriceHorizonAuto,
	)
	if err != nil {
		return priceBarRun{}, err
	}
	horizonN := max(horizons.Horizon, 1)
	logx.Infof("  %s OOS trading horizon 1..%d: h=1 err=%.3f%% (base %.3f%%) · h=%d err=%.3f%% (base %.3f%%) · forward-until-1%% path = %d candles",
		label, horizonN,
		horizons.OOSH1PriceErr*100, horizons.OOSH1BaseErr*100,
		horizonN, horizons.OOSHMaxPriceErr*100, horizons.OOSHMaxBaseErr*100,
		horizons.PathUnder1Pct)
	if horizons.PathExample != nil {
		if steps, ok := horizons.PathExample["steps"].([]map[string]any); ok && len(steps) > 0 {
			fwdN := len(steps)
			logx.Infof("  %s FORWARD from last candle %v (close %v) → %d steps (under-1%% path %d)",
				label, horizons.PathExample["origin"], horizons.PathExample["origin_close"],
				fwdN, horizons.PathUnder1Pct)
			show := func(s map[string]any) {
				spot := "n/a (future)"
				if s["spot"] != nil {
					spot = fmt.Sprintf("%v", s["spot"])
				}
				logx.Infof("    +%-2v %v  pred=%v  spot=%s", s["h"], s["at"], s["predicted"], spot)
			}
			for i, s := range steps {
				if i < 3 {
					show(s)
				}
			}
			if len(steps) > 3 {
				logx.Infof("    …")
				show(steps[len(steps)-1])
			}
		}
	}

	var chartSignals []map[string]any
	for _, s := range allSamples {
		if s.AtMs < tradeStart || s.AtMs >= tradeEnd {
			continue
		}
		pred := priceModel.PredictClose(s.RefPx, s.Features)
		chartSignals = append(chartSignals, map[string]any{
			"at": utc(s.AtMs), "spot": round2(s.Spot), "ref": round2(s.RefPx),
			"predicted": round2(pred), "side": "h1", "pnl": 0.0, "label": label,
			"rel_err": math.Abs(pred-s.Spot) / math.Max(s.Spot, 1e-12),
		})
	}

	var strategy map[string]any
	balancePnL := 0.0
	if args.PriceStrategyMode == 1 || args.PriceStrategyMode == 2 {
		strategy, err = runPriceStrategyPerTrade(args, minutes, horizons.Models, label, barMs, tradeStart, tradeEnd, taker, maker)
		if err != nil {
			return priceBarRun{}, err
		}
		if v, ok := strategy["total_pnl"].(float64); ok {
			balancePnL = v
		}
	}

	horizonRule := "fixed --price-horizon"
	if args.PriceHorizonAuto {
		horizonRule = "auto: last h whose OOS mean |pred-spot|/spot < max_rel_err; path_under_1pct = forward from last candle vs tape/OKX until miss ≥ max_rel_err"
	}
	alphas := make([]float64, pricemodel.Features)
	for i := 0; i < pricemodel.Features; i++ {
		alphas[i] = round2(priceModel.Alphas[i]*1e6) / 1e6
	}
	var stratSummary any
	if strategy != nil {
		stratSummary = strategy["summary"]
	}
	var stratBody any = strategy
	if strategy == nil {
		stratBody = nil
	}
	signalRule := fmt.Sprintf(
		"debounced dir from h=1 return (%d bars); no per-trade exits",
		PriceSignalConfirmBars,
	)
	switch args.PriceStrategyMode {
	case 1:
		signalRule = fmt.Sprintf(
			"debounced dir from h=1 return (%d bars); per-trade TP/SL/lev = max PnL on actual closes t+1..t+%d (oracle / peek)",
			PriceSignalConfirmBars, horizonN,
		)
	case 2:
		signalRule = fmt.Sprintf(
			"debounced dir from h=1 return (%d bars); per-trade TP/SL/lev = max worst-case on predicted closes ±%.2f%% with TP≥%.2f%% (no peek); realized on actuals",
			PriceSignalConfirmBars, args.MaxRelErr*100, args.MaxRelErr*100,
		)
	}

	rep := map[string]any{
		"meta": map[string]any{
			"path": "price", "instrument": candles.Instrument,
			"feature_names": pricemodel.FeatureNames,
			"equation":      "(close_{t+h}/close_t)-1 = Σαf(t); one OLS per horizon h",
			"bar": label, "bar_ms": barMs, "price_horizon": horizonN,
			"price_horizon_auto": args.PriceHorizonAuto,
			"path_under_1pct":    horizons.PathUnder1Pct,
			"horizon_rule":       horizonRule,
			"price_strategy_mode": args.PriceStrategyMode,
			"signal_rule":        signalRule,
			"lookback_days": lookbackDays, "max_rel_err": args.MaxRelErr,
			"min_fit_pairs": args.MinFitPairs, "require_fit": args.RequireFit,
			"fit_pairs": len(fitPairs), "locked": locked, "locked_at": utc(lockMs),
			"trade_start": utc(tradeStart), "trade_end": utc(tradeEnd),
			"size_btc": meta.SizeBTC, "balance_usdt": args.BalanceUSDT,
			"taker_fee_bps": taker * 10_000, "maker_fee_bps": maker * 10_000,
			"price_model": map[string]any{
				"alphas": alphas, "rel_err": priceModel.RelErr, "return_mae": priceModel.RelErr,
				"price_rel_err": priceModel.PriceRelErr, "baseline_rel_err": priceModel.BaselineRelErr,
				"mae": priceModel.MAE, "samples": priceModel.Samples, "cold_start": priceModel.ColdStart,
				"form": "candle_ohlc_multi_horizon", "horizon": 1,
			},
			"horizons": horizons.Table, "path_example": horizons.PathExample,
			"strategy": stratSummary, "fit_trace": fitTrace,
			"generated_utc": time.Now().UTC().Format("2006-01-02 15:04"),
		},
		"signals": chartSignals, "strategy": stratBody, "total_pnl": round2(balancePnL),
	}

	return priceBarRun{
		Label: label, BarMs: barMs, Locked: locked, FitPairs: len(fitPairs),
		ReturnMAE: priceModel.RelErr, PriceRelErr: priceModel.PriceRelErr,
		BaselineRelErr: priceModel.BaselineRelErr, TotalPnL: balancePnL,
		NSignals: len(chartSignals), Report: rep,
	}, nil
}

func priceAt(minutes []candles.Candle, at int64) float64 {
	if len(minutes) == 0 {
		return 0
	}
	// last candle with TimeMs <= at
	lo, hi := 0, len(minutes)
	for lo < hi {
		mid := (lo + hi) / 2
		if minutes[mid].TimeMs <= at {
			lo = mid + 1
		} else {
			hi = mid
		}
	}
	i := lo - 1
	if i < 0 {
		i = 0
	}
	return minutes[i].Close
}

func utc(ms int64) string {
	return time.UnixMilli(ms).UTC().Format("2006-01-02 15:04:05")
}
