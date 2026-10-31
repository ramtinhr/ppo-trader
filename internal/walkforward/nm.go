// Package walkforward ports the N/M score-path helpers from window_nm_walk_forward.rs.
package walkforward

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/ramtin/dynamic-trader/internal/engine"
	"github.com/ramtin/dynamic-trader/internal/report"
	"github.com/ramtin/dynamic-trader/internal/score"
)

const (
	MinISProfit = 0.1
	Shown       = 20
	Record      = score.Record
)

// Args mirrors window_nm_walk_forward score-path CLI flags.
type Args struct {
	Scan             string
	LookbackDays     uint64
	TrainTrades      []int
	TradeDays        uint64
	MinFitPairs      int
	ReserveTradeDays uint64
	FirstChoice      string // YYYY-MM-DD or empty
	RequireFit       bool
	MaxRelErr        float64
	MaxFinalists     int
	MemoryGB         float64
	MaxFitPairs      int
	Threads          int
	Verify           int
	BalanceUSDT      float64
	Config           string
	Data             string
	HTMLOnly         bool
	Rerank           bool
	SizeBTC          float64
	Start            string // override tape start (optional)
	End              string // override tape end (optional)
}

func DefaultArgs() Args {
	tt := make([]int, 0, 10)
	for k := 10; k >= 1; k-- {
		tt = append(tt, k*10)
	}
	return Args{
		Scan:             "data/windowscan/2026-09-01_2026-10-01_1d",
		LookbackDays:     30,
		TrainTrades:      tt,
		MinFitPairs:      2000,
		ReserveTradeDays: 30,
		RequireFit:       true,
		MaxRelErr:        score.MaxScoreRelErr,
		MaxFinalists:     200_000,
		MemoryGB:         4,
		Threads:          2,
		Verify:           1,
		BalanceUSDT:      500,
		Config:           "config/testnet.toml",
		Data:             "data/okx/BTC-USDT-SWAP/archives",
		SizeBTC:          0.005,
	}
}

// ParseArgs parses window_nm score-path flags from argv (without the program name).
func ParseArgs(argv []string) (Args, error) {
	args := DefaultArgs()
	for i := 0; i < len(argv); i++ {
		flag := argv[i]
		if flag == "-h" || flag == "--help" {
			return args, errHelp
		}
		if flag == "--html-only" {
			args.HTMLOnly = true
			continue
		}
		if flag == "--rerank" {
			args.Rerank = true
			continue
		}
		need := func() (string, error) {
			if i+1 >= len(argv) {
				return "", fmt.Errorf("%s needs a value", flag)
			}
			i++
			return argv[i], nil
		}
		switch flag {
		case "--scan":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.Scan = v
		case "--lookback-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil || n == 0 {
				return args, fmt.Errorf("bad --lookback-days")
			}
			args.LookbackDays = n
		case "--train-trades":
			v, err := need()
			if err != nil {
				return args, err
			}
			parts := strings.Split(v, ",")
			args.TrainTrades = args.TrainTrades[:0]
			for _, p := range parts {
				p = strings.TrimSpace(p)
				if p == "" {
					continue
				}
				n, err := strconv.Atoi(p)
				if err != nil {
					return args, err
				}
				args.TrainTrades = append(args.TrainTrades, n)
			}
		case "--trade-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil {
				return args, err
			}
			args.TradeDays = n
		case "--min-fit-pairs":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil || n <= 0 {
				return args, fmt.Errorf("bad --min-fit-pairs")
			}
			args.MinFitPairs = n
		case "--reserve-trade-days":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.ParseUint(v, 10, 64)
			if err != nil || n == 0 {
				return args, fmt.Errorf("bad --reserve-trade-days")
			}
			args.ReserveTradeDays = n
		case "--first-choice":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.FirstChoice = v
		case "--require-fit":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil || n > 1 {
				return args, fmt.Errorf("--require-fit must be 0 or 1")
			}
			args.RequireFit = n == 1
		case "--max-rel-err":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil || x <= 0 || x > 1 {
				return args, fmt.Errorf("bad --max-rel-err")
			}
			args.MaxRelErr = x
		case "--max-finalists":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return args, err
			}
			args.MaxFinalists = n
		case "--memory-gb":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil {
				return args, err
			}
			args.MemoryGB = x
		case "--max-fit-pairs":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return args, err
			}
			args.MaxFitPairs = n
		case "--threads":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil || n <= 0 {
				return args, fmt.Errorf("bad --threads")
			}
			args.Threads = n
		case "--verify":
			v, err := need()
			if err != nil {
				return args, err
			}
			n, err := strconv.Atoi(v)
			if err != nil {
				return args, err
			}
			args.Verify = n
		case "--balance-usdt":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil {
				return args, err
			}
			args.BalanceUSDT = x
		case "--config":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.Config = v
		case "--data":
			v, err := need()
			if err != nil {
				return args, err
			}
			args.Data = v
		case "--size-btc":
			v, err := need()
			if err != nil {
				return args, err
			}
			x, err := strconv.ParseFloat(v, 64)
			if err != nil {
				return args, err
			}
			args.SizeBTC = x
		case "--path":
			v, err := need()
			if err != nil {
				return args, err
			}
			if v != "score" {
				return args, fmt.Errorf("dt walk-forward only supports --path score (use dt price-walk for price)")
			}
		default:
			return args, fmt.Errorf("unknown option %s", flag)
		}
	}
	if len(args.TrainTrades) == 0 {
		return args, fmt.Errorf("--train-trades must be non-empty")
	}
	for _, n := range args.TrainTrades {
		if n < 3 {
			return args, fmt.Errorf("--train-trades values must be >= 3")
		}
	}
	return args, nil
}

var errHelp = fmt.Errorf("help")

func IsHelp(err error) bool { return err == errHelp }

// TestSize is M = N/3 for a training window of N trades.
func TestSize(n int) int { return n / 3 }

// Meta is the window scan meta.json subset we need.
type Meta struct {
	Timeframes     []int     `json:"timeframes"`
	Lengths        []int     `json:"lengths"`
	TP             []float64 `json:"tp"`
	SL             []float64 `json:"sl"`
	Leverage       []uint32  `json:"leverage"`
	Combinations   int       `json:"combinations"`
	EveryPair      int       `json:"every_pair"`
	Days           uint64    `json:"days"`
	Hours          *uint64   `json:"hours"`
	Start          string    `json:"start"`
	End            string    `json:"end"`
	Windows        []Window  `json:"windows"`
	SurvivorGroups []int     `json:"survivor_groups"`
	ExitProfiles   []any     `json:"exit_profiles"`
	SizeBTC        float64   `json:"size_btc"`
}

type Window struct {
	Start string `json:"start"`
	End   string `json:"end"`
	File  string `json:"file"`
}

// Space decodes combination ids like the Rust Space.
type Space struct {
	Timeframes     []int
	Lengths        []int
	Pairs          [][2]int
	TP, SL         []float64
	Lev            []uint32
	SurvivorGroups []int
}

func (s Space) Settings() int {
	return len(s.TP) * len(s.SL) * max(1, len(s.Lev))
}

func (s Space) Decode(id uint32) (tf, pair, tp, sl, lev int) {
	settings := s.Settings()
	fileGroup := int(id) / settings
	setting := int(id) % settings
	group := fileGroup
	if len(s.SurvivorGroups) > 0 {
		group = s.SurvivorGroups[fileGroup]
	}
	nSL := len(s.SL)
	nLev := max(1, len(s.Lev))
	lev = setting % nLev
	tpsl := setting / nLev
	return group / len(s.Pairs), group % len(s.Pairs), tpsl / nSL, tpsl % nSL, lev
}

func (s Space) Label(id uint32) string {
	f, p, tp, sl, li := s.Decode(id)
	i, j := s.Pairs[p][0], s.Pairs[p][1]
	return fmt.Sprintf("%dm RSI %d/%d · TP %.2g%% / SL %.2g%% · %dx",
		s.Timeframes[f], s.Lengths[i], s.Lengths[j], s.TP[tp], s.SL[sl], s.Lev[li])
}

func (s Space) ComboParams(id uint32) (minutes, fast, slow int, stop, tp float64, lev uint32) {
	f, p, tpi, sli, li := s.Decode(id)
	i, j := s.Pairs[p][0], s.Pairs[p][1]
	return s.Timeframes[f], s.Lengths[i], s.Lengths[j], s.SL[sli], s.TP[tpi], s.Lev[li]
}

// Kept is one closed trade with running cum PnL.
type Kept struct {
	EntryMs int64
	ExitMs  int64
	Cum     float64
}

// Finalist is one shortlisted combo with its closed trades.
type Finalist struct {
	ID     uint32
	Trades []Kept
}

func (f Finalist) Net(k int) float64 {
	before := 0.0
	if k > 0 {
		before = f.Trades[k-1].Cum
	}
	return f.Trades[k].Cum - before
}

// Training returns the index range of the last n closes at or before at, none before at-lookback.
func Training(exits []int64, at int64, n int, lookbackMs int64) (start, end int) {
	countUntil := func(t int64) int {
		lo, hi := 0, len(exits)
		for lo < hi {
			mid := (lo + hi) / 2
			if exits[mid] <= t {
				lo = mid + 1
			} else {
				hi = mid
			}
		}
		return lo
	}
	known := countUntil(at)
	first := countUntil(at - lookbackMs)
	if known-n > first {
		first = known - n
	}
	return first, known
}

// Pick is the best finalist at time at under the score model.
type Pick struct {
	Finalist int
	Stats    score.WindowStats
	Score    float64
}

// PickBest chooses the finalist with best learned score on last n IS trades.
func PickBest(finalists []Finalist, at int64, n int, lookbackMs int64, model *score.ScoreModel) *Pick {
	var best *Pick
	var bestID uint32
	for k, f := range finalists {
		exits := make([]int64, len(f.Trades))
		for i, t := range f.Trades {
			exits[i] = t.ExitMs
		}
		lo, hi := Training(exits, at, n, lookbackMs)
		if hi-lo < score.MinTrades {
			continue
		}
		before := 0.0
		if lo > 0 {
			before = f.Trades[lo-1].Cum
		}
		profit := f.Trades[hi-1].Cum - before
		if profit <= MinISProfit {
			continue
		}
		var tally score.Tally
		for i := lo; i < hi; i++ {
			tally.Add(f.Net(i), f.Trades[i].ExitMs-f.Trades[i].EntryMs)
			tally.NoteExit(f.Trades[i].ExitMs)
		}
		st := tally.Stats().WithScoreClock(at, lookbackMs)
		sc, ok := model.Predict(&st)
		if !ok {
			continue
		}
		wins := best == nil
		if best != nil {
			if sc > best.Score || (sc == best.Score && st.Profit > best.Stats.Profit) ||
				(sc == best.Score && st.Profit == best.Stats.Profit && f.ID < bestID) {
				wins = true
			}
		}
		if wins {
			p := Pick{Finalist: k, Stats: st, Score: sc}
			best = &p
			bestID = f.ID
		}
	}
	return best
}

// ChainTrade is one OOS trade in an N/M chain.
type ChainTrade struct {
	EntryMs int64
	ExitMs  int64
	Net     float64
	Combo   uint32
}

// Chain is one (N,M) walk-forward path.
type Chain struct {
	N, M       int
	At         int64
	Done       bool
	Windows    []float64
	Trades     []ChainTrade
	StopReason string
}

// Advance runs picks until windowEnd; returns IS→OOS fit pairs.
func Advance(chain *Chain, finalists []Finalist, windowEnd, lookbackMs int64, model *score.ScoreModel) []score.Sample {
	var fitPairs []score.Sample
	for !chain.Done && chain.At < windowEnd {
		p := PickBest(finalists, chain.At, chain.N, lookbackMs, model)
		if p == nil {
			chain.At = windowEnd
			break
		}
		f := &finalists[p.Finalist]
		first := 0
		for first < len(f.Trades) && f.Trades[first].EntryMs < chain.At {
			first++
		}
		if first+chain.M > len(f.Trades) {
			chain.StopReason = fmt.Sprintf("pick %d has fewer than %d trades left", f.ID, chain.M)
			chain.Done = true
			break
		}
		net := 0.0
		var oos score.Tally
		for k := first; k < first+chain.M; k++ {
			t := &f.Trades[k]
			tradeNet := f.Net(k)
			chain.Trades = append(chain.Trades, ChainTrade{
				EntryMs: t.EntryMs, ExitMs: t.ExitMs, Net: tradeNet, Combo: f.ID,
			})
			net += tradeNet
			oos.Add(tradeNet, t.ExitMs-t.EntryMs)
			oos.NoteExit(t.ExitMs)
		}
		if uint32(p.Stats.Trades) >= score.MinTrades {
			oosAt := f.Trades[first+chain.M-1].ExitMs
			oosStats := oos.Stats().WithScoreClock(oosAt, lookbackMs)
			if uint32(oosStats.Trades) >= score.MinTrades {
				fitPairs = append(fitPairs, score.Sample{Stats: p.Stats, Target: oosStats.TargetQuality()})
			}
		}
		chain.Windows = append(chain.Windows, net)
		chain.At = f.Trades[first+chain.M-1].ExitMs
	}
	return fitPairs
}

type ranked struct {
	id    uint32
	stats score.WindowStats
	sc    float64
}

// RankTop reads a window_*.bin and returns the top keep combos by cold-start score.
func RankTop(path string, nCombos, keep int, model *score.ScoreModel) ([]ranked, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	buf := make([]byte, Record)
	out := make([]ranked, 0, keep)
	for id := 0; id < nCombos; id++ {
		if _, err := f.ReadAt(buf, int64(id)*Record); err != nil {
			return nil, err
		}
		st := score.FromBytes(buf)
		sc, ok := model.Predict(&st)
		if !ok {
			sc, ok = st.Score()
			if !ok {
				continue
			}
		}
		if st.Trades == 0 {
			continue
		}
		out = append(out, ranked{uint32(id), st, sc})
		if len(out) >= 2*keep {
			sort.Slice(out, func(i, j int) bool {
				if out[i].sc != out[j].sc {
					return out[i].sc > out[j].sc
				}
				if out[i].stats.Profit != out[j].stats.Profit {
					return out[i].stats.Profit > out[j].stats.Profit
				}
				return out[i].id < out[j].id
			})
			out = out[:keep]
		}
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].sc != out[j].sc {
			return out[i].sc > out[j].sc
		}
		if out[i].stats.Profit != out[j].stats.Profit {
			return out[i].stats.Profit > out[j].stats.Profit
		}
		return out[i].id < out[j].id
	})
	if len(out) > keep {
		out = out[:keep]
	}
	return out, nil
}

func parseDayMs(s string) (int64, error) {
	// Accept "YYYY-MM-DD" or "YYYY-MM-DD HH:MM:SS"
	s = strings.TrimSpace(s)
	if len(s) >= 10 {
		day := s[:10]
		t, err := time.Parse("2006-01-02", day)
		if err != nil {
			return 0, err
		}
		return t.UTC().UnixMilli(), nil
	}
	return 0, fmt.Errorf("bad date %q", s)
}

func loadMeta(dir string) (Meta, Space, error) {
	raw, err := os.ReadFile(filepath.Join(dir, "meta.json"))
	if err != nil {
		return Meta{}, Space{}, err
	}
	var m Meta
	if err := json.Unmarshal(raw, &m); err != nil {
		return Meta{}, Space{}, err
	}
	if len(m.Leverage) == 0 {
		m.Leverage = []uint32{1}
	}
	if m.EveryPair != 0 && m.EveryPair != 1 {
		return Meta{}, Space{}, fmt.Errorf("scan every_pair=%d", m.EveryPair)
	}
	pairs := make([][2]int, 0)
	for i := 0; i < len(m.Lengths); i++ {
		for j := i + 1; j < len(m.Lengths); j++ {
			pairs = append(pairs, [2]int{i, j})
		}
	}
	sp := Space{
		Timeframes: m.Timeframes, Lengths: m.Lengths, Pairs: pairs,
		TP: m.TP, SL: m.SL, Lev: m.Leverage, SurvivorGroups: m.SurvivorGroups,
	}
	return m, sp, nil
}

func exitsJSON(stop, tp float64) json.RawMessage {
	b, _ := json.Marshal(map[string]any{
		"stop_pct": stop,
		"targets":  [][]float64{{tp, 1.0}},
		"breakeven": false,
	})
	return b
}

// SimulateFinalists calls Rust simulate in batch for the given combo ids.
func SimulateFinalists(space Space, ids []uint32, simArgs engine.SimulateArgs, fromMs, toMs *int64) ([]Finalist, error) {
	reqs := make([]engine.SimulateRequest, 0, len(ids))
	for _, id := range ids {
		minutes, fast, slow, stop, tp, lev := space.ComboParams(id)
		req := engine.SimulateRequest{
			ID: strconv.FormatUint(uint64(id), 10),
			Minutes: minutes, Fast: fast, Slow: slow,
			Exits: exitsJSON(stop, tp), Leverage: lev, Logged: true,
			FromMs: fromMs, ToMs: toMs,
		}
		reqs = append(reqs, req)
	}
	resps, err := engine.BatchSimulate(simArgs, reqs)
	if err != nil {
		return nil, err
	}
	byID := map[string]engine.SimulateResponse{}
	for _, r := range resps {
		byID[r.ID] = r
	}
	out := make([]Finalist, 0, len(ids))
	for _, id := range ids {
		r, ok := byID[strconv.FormatUint(uint64(id), 10)]
		if !ok {
			out = append(out, Finalist{ID: id})
			continue
		}
		cum := 0.0
		trades := make([]Kept, 0, len(r.Closed))
		for _, c := range r.Closed {
			cum += float64(c.Net)
			trades = append(trades, Kept{EntryMs: c.EntryMs, ExitMs: c.ExitMs, Cum: cum})
		}
		out = append(out, Finalist{ID: id, Trades: trades})
	}
	return out, nil
}

func scoreModelJSON(m *score.ScoreModel, maxRelErr float64) map[string]any {
	ok := !m.ColdStart && m.ScoreRelErr < maxRelErr
	weights := make([]float64, score.ScoreFeatures)
	sum := 0.0
	for i, w := range m.Weights {
		weights[i] = math.Round(w*1000) / 1000
		sum += w
	}
	return map[string]any{
		"weights": weights, "weight_sum": math.Round(sum*1000) / 1000,
		"budget": score.ScoreBudget, "intercept": 0.0,
		"score_mae": math.Round(m.ScoreMAE*1000) / 1000,
		"score_rel_err": math.Round(m.ScoreRelErr*10000) / 10000,
		"samples": m.Samples, "cold_start": m.ColdStart, "score_ok": ok, "max_rel_err": maxRelErr,
	}
}

// RunScorePath executes the Go-owned N/M score walk-forward (skeleton + real helpers).
func RunScorePath(args Args) error {
	if args.HTMLOnly {
		return regenerateHTML(args.Scan)
	}
	meta, space, err := loadMeta(args.Scan)
	if err != nil {
		return err
	}
	if len(meta.Windows) < 2 {
		return fmt.Errorf("need at least 2 windows")
	}
	nCombos := meta.Combinations
	nWindows := len(meta.Windows)
	lookbackMs := int64(args.LookbackDays) * 86_400_000

	maxM := 0
	for _, n := range args.TrainTrades {
		if m := TestSize(n); m > maxM {
			maxM = m
		}
	}
	configs := make([][2]int, 0)
	for _, n := range args.TrainTrades {
		for m := TestSize(n); m >= 1; m-- {
			configs = append(configs, [2]int{n, m})
		}
	}

	model := score.ColdStart()
	finalistIDs := make([][]uint32, nWindows)
	windowsJSON := make([]map[string]any, 0)

	fmt.Printf("Ranking combinations window by window (max %d finalists)…\n", args.MaxFinalists)
	for w := 1; w < nWindows; w++ {
		rankedWin := meta.Windows[w-1]
		tradedIn := meta.Windows[w]
		top, err := RankTop(filepath.Join(args.Scan, rankedWin.File), nCombos, args.MaxFinalists, &model)
		if err != nil {
			return fmt.Errorf("rank %s: %w", rankedWin.File, err)
		}
		ids := make([]uint32, 0, len(top))
		for _, r := range top {
			ids = append(ids, r.id)
		}
		finalistIDs[w] = ids
		shown := make([]map[string]any, 0, Shown)
		for i, r := range top {
			if i >= Shown {
				break
			}
			shown = append(shown, map[string]any{
				"rank": i + 1, "id": r.id, "label": space.Label(r.id),
				"score": math.Round(r.sc*1000) / 1000,
				"profit": math.Round(float64(r.stats.Profit)*100) / 100,
			})
		}
		windowsJSON = append(windowsJSON, map[string]any{
			"ranked": rankedWin.Start, "traded": tradedIn.Start,
			"finalists": len(ids), "shown": shown,
		})
		fmt.Printf("  %s → %d finalists for %s\n", rankedWin.Start, len(ids), tradedIn.Start)
	}

	// Determine tape range from meta.
	startDay := meta.Start
	endDay := meta.End
	if startDay == "" && len(meta.Windows) > 0 {
		startDay = meta.Windows[0].Start[:10]
	}
	if endDay == "" && len(meta.Windows) > 0 {
		endDay = meta.Windows[len(meta.Windows)-1].End[:10]
	}
	if args.Start != "" {
		startDay = args.Start
	}
	if args.End != "" {
		endDay = args.End
	}

	simArgs := engine.SimulateArgs{
		Start: startDay, End: endDay,
		SizeBTC: args.SizeBTC, BalanceUSDT: args.BalanceUSDT,
		Config: args.Config, Data: args.Data, Jobs: args.Threads,
	}

	// Near→far score TRAIN on windows (newest first), using simulate for finalist trades.
	trainCutoffMs, err := parseDayMs(meta.Windows[nWindows-1].End)
	if err != nil {
		return err
	}
	reserveMs := int64(args.ReserveTradeDays) * 86_400_000
	trainCutoffMs -= reserveMs
	firstPickMs, err := parseDayMs(meta.Windows[1].Start)
	if err != nil {
		return err
	}

	trainWs := make([]int, 0)
	for w := 1; w < nWindows; w++ {
		endMs, _ := parseDayMs(meta.Windows[w].End)
		if endMs <= trainCutoffMs {
			trainWs = append(trainWs, w)
		}
	}
	if len(trainWs) == 0 {
		// Fall back: use all windows except last reserve slice conceptually.
		for w := 1; w < nWindows; w++ {
			trainWs = append(trainWs, w)
		}
	}

	fmt.Printf("Score TRAIN near→far over %d windows (%d N/M configs)…\n", len(trainWs), len(configs))
	fitPairs := make([]score.Sample, 0)
	scoreLocked := false
	harvest := score.ColdStart()
	scoreModelsJSON := make([]map[string]any, 0)

	for i := len(trainWs) - 1; i >= 0; i-- {
		w := trainWs[i]
		window := meta.Windows[w]
		startMs, _ := parseDayMs(window.Start)
		endMs, _ := parseDayMs(window.End)
		dayStart := startMs
		if dayStart < firstPickMs {
			dayStart = firstPickMs
		}
		ids := finalistIDs[w]
		if len(ids) == 0 {
			continue
		}
		from := startMs - lookbackMs
		to := endMs
		fmt.Printf("  simulate %d finalists for %s…\n", len(ids), window.Start)
		finalists, err := SimulateFinalists(space, ids, simArgs, &from, &to)
		if err != nil {
			// Soft-fail: if simulate binary missing / no data, continue with synthetic empty.
			fmt.Fprintf(os.Stderr, "  warning: simulate failed (%v); skipping window\n", err)
			continue
		}
		added := 0
		for _, cfg := range configs {
			chain := Chain{N: cfg[0], M: cfg[1], At: dayStart, StopReason: "the data ended"}
			pairs := Advance(&chain, finalists, endMs, lookbackMs, &harvest)
			fitPairs = append(fitPairs, pairs...)
			added += len(pairs)
		}
		if args.MaxFitPairs > 0 && len(fitPairs) > args.MaxFitPairs {
			fitPairs = fitPairs[:args.MaxFitPairs]
		}
		if len(fitPairs) < score.MinFitSamples {
			fmt.Printf("  %s: +%d → fit_pairs=%d (need ≥%d)\n", window.Start, added, len(fitPairs), score.MinFitSamples)
			continue
		}
		m, err := score.Fit(fitPairs, &model)
		if err != nil {
			fmt.Printf("  %s: fit failed (%v)\n", window.Start, err)
			continue
		}
		model = m
		entry := map[string]any{
			"phase": "train_near_to_far", "label": window.Start,
			"model": scoreModelJSON(&model, args.MaxRelErr),
			"fit_pairs": len(fitPairs), "locked": false,
		}
		pairsOK := len(fitPairs) >= args.MinFitPairs
		if !model.ColdStart && model.ScoreRelErr < args.MaxRelErr && pairsOK {
			scoreLocked = true
			entry["locked"] = true
			scoreModelsJSON = append(scoreModelsJSON, entry)
			fmt.Printf("  score LOCKED at %s — rel_err=%.3f%% pairs=%d\n",
				window.Start, model.ScoreRelErr*100, len(fitPairs))
			break
		}
		scoreModelsJSON = append(scoreModelsJSON, entry)
		fmt.Printf("  %s: +%d → fit_pairs=%d cold=%v rel_err=%.3f%%\n",
			window.Start, added, len(fitPairs), model.ColdStart, model.ScoreRelErr*100)
	}

	if !scoreLocked && args.RequireFit {
		return fmt.Errorf("score TRAIN never LOCKed (cold=%v rel_err=%.3f%% pairs=%d; need <%g and ≥%d)",
			model.ColdStart, model.ScoreRelErr*100, len(fitPairs), args.MaxRelErr, args.MinFitPairs)
	}

	// TRADE phase: continue N/M chains with locked model on remaining windows.
	tradeChains := make([]*Chain, len(configs))
	for i, cfg := range configs {
		tradeChains[i] = &Chain{N: cfg[0], M: cfg[1], At: trainCutoffMs, StopReason: "the data ended"}
		if tradeChains[i].At < firstPickMs {
			tradeChains[i].At = firstPickMs
		}
	}
	fmt.Println("TRADE: advancing N/M chains with locked score model…")
	for w := 1; w < nWindows; w++ {
		window := meta.Windows[w]
		startMs, _ := parseDayMs(window.Start)
		endMs, _ := parseDayMs(window.End)
		if endMs <= trainCutoffMs {
			continue
		}
		ids := finalistIDs[w]
		if len(ids) == 0 {
			continue
		}
		from := startMs - lookbackMs
		to := endMs
		finalists, err := SimulateFinalists(space, ids, simArgs, &from, &to)
		if err != nil {
			fmt.Fprintf(os.Stderr, "  warning: simulate failed (%v)\n", err)
			continue
		}
		for _, chain := range tradeChains {
			_ = Advance(chain, finalists, endMs, lookbackMs, &model)
		}
	}

	// Choose best chain by total OOS PnL.
	bestIdx := 0
	bestTotal := math.Inf(-1)
	for i, c := range tradeChains {
		tot := 0.0
		for _, w := range c.Windows {
			tot += w
		}
		if tot > bestTotal {
			bestTotal = tot
			bestIdx = i
		}
	}
	best := tradeChains[bestIdx]
	chosen := map[string]any{
		"train_trades": best.N, "test_size": best.M,
		"total": math.Round(bestTotal*100) / 100,
		"windows": len(best.Windows), "trades": len(best.Trades),
		"stop_reason": best.StopReason,
		"short_oos_policy": "stop",
	}

	out := map[string]any{
		"meta": map[string]any{
			"scan": args.Scan, "lookback_days": args.LookbackDays,
			"train_trades": args.TrainTrades, "max_rel_err": args.MaxRelErr,
			"min_fit_pairs": args.MinFitPairs, "reserve_trade_days": args.ReserveTradeDays,
			"require_fit": args.RequireFit, "max_finalists": args.MaxFinalists,
			"score_locked": scoreLocked, "path": "score",
			"engine": "go+rust-simulate",
		},
		"score_model": scoreModelJSON(&model, args.MaxRelErr),
		"score_models": scoreModelsJSON,
		"windows": windowsJSON,
		"chosen": chosen,
		"fit_pairs": len(fitPairs),
	}

	jsonPath := filepath.Join(args.Scan, "nm_walk_forward.json")
	b, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(jsonPath, b, 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", jsonPath)
	if err := report.WriteHTML(args.Scan, "nm_walk_forward", out); err != nil {
		return err
	}
	fmt.Printf("wrote %s / nm_walk_forward_fa.html\n", filepath.Join(args.Scan, "nm_walk_forward.html"))
	return nil
}

func regenerateHTML(scan string) error {
	wrote := 0
	for _, stem := range []string{"nm_walk_forward", "nm_walk_forward_allow_short"} {
		p := filepath.Join(scan, stem+".json")
		if _, err := os.Stat(p); err != nil {
			fmt.Printf("skip (no JSON): %s\n", p)
			continue
		}
		if err := report.FromJSON(p); err != nil {
			return err
		}
		fmt.Printf("wrote %s / %s_fa.html\n", filepath.Join(scan, stem+".html"), stem)
		wrote++
	}
	if wrote == 0 {
		return fmt.Errorf("no nm_walk_forward*.json found in %s", scan)
	}
	return nil
}
