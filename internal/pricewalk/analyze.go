package pricewalk

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"sort"
	"time"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/config"
	"github.com/ramtin/dynamic-trader/internal/exits"
	"github.com/ramtin/dynamic-trader/internal/logx"
)

// AnalyzeArgs for dt price-analyze — replay report trades' actual paths across TP×SL×lev.
type AnalyzeArgs struct {
	Scan     string
	Report   string // default: scan/nm_price_walk_forward_predicted.json
	Data     string
	Config   string
	Stops    []float64
	Targets  []float64
	Leverage []uint32
	MaxRisk  float64
	Balance  float64
}

func DefaultAnalyzeArgs() AnalyzeArgs {
	stops, _ := ParseRange("0.25-2/0.25")
	tgts, _ := ParseRange("0.25-2/0.25")
	return AnalyzeArgs{
		Report:   "",
		Data:     "data/okx/BTC-USDT-SWAP/archives",
		Config:   "config/testnet.toml",
		Stops:    stops,
		Targets:  tgts,
		Leverage: []uint32{1, 2, 3, 4, 5},
		MaxRisk:  5,
		Balance:  500,
	}
}

type tradeRec struct {
	Side       string         `json:"side"`
	EntryBar   int            `json:"entry_bar"`
	EntryPrice float64        `json:"entry_price"`
	EntryAt    string         `json:"entry_at"`
	StopPct    float64        `json:"stop_pct"`
	Leverage   float64        `json:"leverage"`
	ExitH      int            `json:"exit_h"`
	PathWindow int            `json:"path_window"`
	PnL        float64        `json:"pnl"`
	Reason     string         `json:"reason"`
	Exits      map[string]any `json:"exits"`
}

type reportFile struct {
	Meta     map[string]any `json:"meta"`
	Strategy map[string]any `json:"strategy"`
	TotalPnL float64        `json:"total_pnl"`
}

// RunAnalyze loads a predicted report + tape and answers:
// entry/direction vs TP/SL/lev — by replaying actual paths on a TP×SL×lev grid.
func RunAnalyze(a AnalyzeArgs) error {
	if a.Scan == "" {
		return fmt.Errorf("--scan is required")
	}
	repPath := a.Report
	if repPath == "" {
		repPath = filepath.Join(a.Scan, "nm_price_walk_forward_predicted.json")
	}
	raw, err := os.ReadFile(repPath)
	if err != nil {
		return fmt.Errorf("read report %s: %w", repPath, err)
	}
	var rep reportFile
	if err := json.Unmarshal(raw, &rep); err != nil {
		return err
	}
	cfg, err := config.Load(a.Config)
	if err != nil {
		return err
	}
	taker, maker := cfg.Taker(), cfg.Maker()

	barMs := int64(15 * 60_000)
	if v, ok := rep.Meta["bar_ms"].(float64); ok && v > 0 {
		barMs = int64(v)
	}
	barLabel, _ := rep.Meta["bar"].(string)
	tradeStartS, _ := rep.Meta["trade_start"].(string)
	tradeEndS, _ := rep.Meta["trade_end"].(string)
	tradeStart, err := parseReportUTC(tradeStartS)
	if err != nil {
		return fmt.Errorf("meta.trade_start: %w", err)
	}
	tradeEnd, err := parseReportUTC(tradeEndS)
	if err != nil {
		return fmt.Errorf("meta.trade_end: %w", err)
	}
	bal0 := a.Balance
	if v, ok := rep.Meta["balance_usdt"].(float64); ok && v > 0 {
		bal0 = v
	}

	tradesRaw, _ := rep.Strategy["trades"].([]any)
	trades := make([]tradeRec, 0, len(tradesRaw))
	for _, tr := range tradesRaw {
		m, ok := tr.(map[string]any)
		if !ok {
			continue
		}
		b, _ := json.Marshal(m)
		var t tradeRec
		_ = json.Unmarshal(b, &t)
		if t.EntryPrice > 0 || t.EntryAt != "" {
			trades = append(trades, t)
		}
	}
	if len(trades) == 0 {
		return fmt.Errorf("no trades in %s", repPath)
	}

	logx.Infof("ANALYZE exits on report %s · %d trades · bar=%s (%dms) · grid stops=%d tps=%d lev=%v",
		repPath, len(trades), barLabel, barMs, len(a.Stops), len(a.Targets), a.Leverage)

	// Load tape covering TRADE (+ a bit of lookback for features unused here).
	from := tradeStart - 7*86_400_000
	if from < 0 {
		from = 0
	}
	minutes, err := candles.LoadMinutes(a.Data, a.Scan, from, tradeEnd)
	if err != nil {
		return err
	}
	bars := candles.Aggregate(minutes, barMs)
	if len(bars) < 10 {
		return fmt.Errorf("not enough aggregated bars")
	}

	type comboKey struct {
		Stop, TP float64
		Lev      uint32
	}
	type comboAgg struct {
		Key      comboKey
		Wins     int
		N        int
		PnL      float64
		StopHits int
		TPHits   int
	}
	combos := map[comboKey]*comboAgg{}
	ensure := func(k comboKey) *comboAgg {
		if c, ok := combos[k]; ok {
			return c
		}
		c := &comboAgg{Key: k}
		combos[k] = c
		return c
	}

	var (
		liveWins, liveN int
		livePnL         float64
		mfeSum, maeSum  float64
		dirWrong        int // actual path never reached even a tiny favorable move
		slTooTight      int // MFE ≥ some TP but MAE hit first under tight SL
		oracleWins      int
		oraclePnL       float64
	)

	horizon := 40
	for _, tr := range trades {
		if tr.PathWindow > horizon {
			horizon = tr.PathWindow
		}
		if tr.ExitH > horizon {
			horizon = tr.ExitH
		}
	}
	if horizon < 20 {
		horizon = 20
	}
	if horizon > 200 {
		horizon = 200
	}

	for _, tr := range trades {
		liveN++
		livePnL += tr.PnL
		if tr.PnL > 0 {
			liveWins++
		}
		long := tr.Side == "buy" || tr.Side == "long"
		if tr.Side == "sell" || tr.Side == "short" {
			long = false
		}
		entry := tr.EntryPrice
		if !(entry > 0) && tr.EntryBar >= 0 && tr.EntryBar < len(bars) {
			entry = bars[tr.EntryBar].Open
		}
		i := tr.EntryBar
		if i < 0 || i >= len(bars) {
			continue
		}
		hMax := tr.PathWindow
		if hMax < tr.ExitH {
			hMax = tr.ExitH
		}
		if hMax < 8 {
			hMax = horizon
		}
		if hMax > horizon {
			hMax = horizon
		}
		act := actualCloses(bars, i, hMax)
		if len(act) == 0 {
			continue
		}
		mfe := rawPathFavorableSpanPct(act, entry, long)
		mae := pathAdversePct(act, entry, long)
		mfeSum += mfe
		maeSum += mae
		if mfe < 0.10 {
			dirWrong++
		}

		// Per-trade oracle: best PnL combo on actuals (research ceiling on THIS entry).
		var bestPnL float64
		bestOK := false
		bestHitTP := false
		for _, stop := range a.Stops {
			for _, tp := range a.Targets {
				if tp > mfe+1e-9 {
					continue // unreachable TP on this actual path
				}
				for _, levU := range a.Leverage {
					lev := float64(levU)
					if impliedRiskPct(stop, lev) > a.MaxRisk+1e-12 {
						continue
					}
					if comboCanLiquid(stop, lev) {
						continue
					}
					ex := exits.Exits{StopPct: stop, Targets: [][2]float64{{tp, 1}}}
					steps := sizeForLeverage(bal0, lev, entry)
					if steps <= 0 || ex.Validate(steps) != nil {
						continue
					}
					sim := simulateExitsOnPath(long, entry, ex, steps, act, taker, maker)
					k := comboKey{Stop: stop, TP: tp, Lev: levU}
					agg := ensure(k)
					agg.N++
					agg.PnL += sim.PnL
					if sim.PnL > 0 {
						agg.Wins++
					}
					if sim.Reason == "tp" || sim.Reason == "ladder" {
						agg.TPHits++
					}
					if sim.Reason == "stop" {
						agg.StopHits++
					}
					if !bestOK || sim.PnL > bestPnL {
						bestPnL = sim.PnL
						bestOK = true
						bestHitTP = sim.Reason == "tp" || sim.Reason == "ladder"
					}
				}
			}
		}
		if bestOK {
			oraclePnL += bestPnL
			if bestPnL > 0 {
				oracleWins++
			}
			if bestHitTP && tr.Reason == "stop" && mfe >= 0.25 {
				slTooTight++
			}
		}
	}

	liveWR := 100 * float64(liveWins) / math.Max(float64(liveN), 1)
	oracleWR := 100 * float64(oracleWins) / math.Max(float64(liveN), 1)
	avgMFE := mfeSum / math.Max(float64(liveN), 1)
	avgMAE := maeSum / math.Max(float64(liveN), 1)

	type ranked struct {
		Agg *comboAgg
		WR  float64
	}
	var list []ranked
	for _, c := range combos {
		if c.N < liveN/2 { // must cover most trades
			continue
		}
		wr := 100 * float64(c.Wins) / float64(c.N)
		list = append(list, ranked{c, wr})
	}
	sort.Slice(list, func(i, j int) bool {
		if math.Abs(list[i].Agg.PnL-list[j].Agg.PnL) > 1e-6 {
			return list[i].Agg.PnL > list[j].Agg.PnL
		}
		return list[i].WR > list[j].WR
	})

	var best90 *ranked
	var bestPnLCombo *ranked
	if len(list) > 0 {
		bestPnLCombo = &list[0]
	}
	// Among ≥90% WR, prefer max PnL.
	for i := range list {
		if list[i].WR+1e-9 >= 90 {
			if best90 == nil || list[i].Agg.PnL > best90.Agg.PnL {
				cp := list[i]
				best90 = &cp
			}
		}
	}

	verdict := "mixed"
	detail := ""
	switch {
	case oracleWR < 90:
		verdict = "ENTRY / DIRECTION"
		detail = fmt.Sprintf(
			"Even peeking actuals (oracle TP×SL×lev per trade) only reaches WR=%.1f%% on these entries. "+
				"No exit grid can force ~90%% WR — the predicted side/timing is wrong too often (dir_wrong_mfe<0.10%%: %d/%d).",
			oracleWR, dirWrong, liveN)
	case best90 == nil:
		verdict = "TP / SL SHAPE (no fixed combo ≥90% WR)"
		detail = fmt.Sprintf(
			"Oracle-per-trade WR=%.1f%% (exits CAN win if picked with peek), but no SINGLE fixed TP×SL×lev across all trades hits ≥90%% WR. "+
				"Live pick/formula is the gap (sl_too_tight_suspects≈%d).",
			oracleWR, slTooTight)
	default:
		verdict = "TP / SL / LEV PICK"
		detail = fmt.Sprintf(
			"A fixed combo SL=%.2f%% TP=%.2f%% lev=%dx hits WR=%.1f%% PnL=%+.2f on the same entries. "+
				"Live report used a worse pick (WR=%.1f%% PnL=%+.2f). Problem is exit selection, not entries.",
			best90.Agg.Key.Stop, best90.Agg.Key.TP, best90.Agg.Key.Lev, best90.WR, best90.Agg.PnL, liveWR, livePnL)
	}

	top := make([]map[string]any, 0, 15)
	for i := 0; i < len(list) && i < 15; i++ {
		c := list[i].Agg
		top = append(top, map[string]any{
			"stop_pct": c.Key.Stop, "tp_pct": c.Key.TP, "leverage": c.Key.Lev,
			"n": c.N, "wins": c.Wins, "win_rate": round2(list[i].WR),
			"pnl": round2(c.PnL), "tp_hits": c.TPHits, "stop_hits": c.StopHits,
		})
	}

	out := map[string]any{
		"report":   repPath,
		"bar":      barLabel,
		"trades_n": liveN,
		"live": map[string]any{
			"win_rate": round2(liveWR), "pnl": round2(livePnL), "wins": liveWins,
		},
		"path_stats": map[string]any{
			"avg_mfe_pct": round2(avgMFE*100) / 100, "avg_mae_pct": round2(avgMAE*100) / 100,
			"dir_wrong_mfe_lt_0_10pct": dirWrong, "sl_too_tight_suspects": slTooTight,
		},
		"oracle_per_trade": map[string]any{
			"win_rate": round2(oracleWR), "pnl": round2(oraclePnL), "wins": oracleWins,
			"note": "best TP×SL×lev on actual closes per entry (peek ceiling on THESE entries)",
		},
		"best_fixed_combo_by_pnl": nil,
		"best_fixed_combo_wr90":   nil,
		"top_fixed_combos":        top,
		"verdict":                 verdict,
		"detail":                  detail,
		"grid": map[string]any{
			"stops": a.Stops, "targets": a.Targets, "leverage": a.Leverage, "max_risk_pct": a.MaxRisk,
		},
	}
	if bestPnLCombo != nil {
		c := bestPnLCombo.Agg
		out["best_fixed_combo_by_pnl"] = map[string]any{
			"stop_pct": c.Key.Stop, "tp_pct": c.Key.TP, "leverage": c.Key.Lev,
			"win_rate": round2(bestPnLCombo.WR), "pnl": round2(c.PnL),
		}
	}
	if best90 != nil {
		c := best90.Agg
		out["best_fixed_combo_wr90"] = map[string]any{
			"stop_pct": c.Key.Stop, "tp_pct": c.Key.TP, "leverage": c.Key.Lev,
			"win_rate": round2(best90.WR), "pnl": round2(c.PnL),
		}
	}

	payload, _ := json.MarshalIndent(out, "", "  ")
	outPath := filepath.Join(a.Scan, "nm_price_exit_analyze.json")
	if err := os.WriteFile(outPath, payload, 0o644); err != nil {
		return err
	}
	logx.Infof("VERDICT: %s", verdict)
	logx.Infof("%s", detail)
	logx.Infof("live WR=%.1f%% PnL=%+.2f · oracle-per-trade WR=%.1f%% PnL=%+.2f · avg MFE=%.3f%% MAE=%.3f%%",
		liveWR, livePnL, oracleWR, oraclePnL, avgMFE, avgMAE)
	if bestPnLCombo != nil {
		c := bestPnLCombo.Agg
		logx.Infof("best fixed-by-PnL: SL=%.2f TP=%.2f lev=%dx → WR=%.1f%% PnL=%+.2f",
			c.Key.Stop, c.Key.TP, c.Key.Lev, bestPnLCombo.WR, c.PnL)
	}
	if best90 != nil {
		c := best90.Agg
		logx.Infof("best fixed ≥90%% WR: SL=%.2f TP=%.2f lev=%dx → WR=%.1f%% PnL=%+.2f",
			c.Key.Stop, c.Key.TP, c.Key.Lev, best90.WR, c.PnL)
	} else {
		logx.Warnf("no fixed TP×SL×lev on this grid reaches ≥90%% WR")
	}
	logx.Infof("wrote %s", outPath)
	fmt.Println(string(payload))
	return nil
}

func parseReportUTC(s string) (int64, error) {
	if s == "" {
		return 0, fmt.Errorf("empty time")
	}
	layouts := []string{
		"2006-01-02 15:04:05",
		time.RFC3339,
		"2006-01-02",
	}
	for _, layout := range layouts {
		if t, err := time.ParseInLocation(layout, s, time.UTC); err == nil {
			return t.UnixMilli(), nil
		}
	}
	return 0, fmt.Errorf("bad time %q", s)
}
