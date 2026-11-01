package walkforward

import (
	"math"
	"testing"

	"github.com/ramtin/dynamic-trader/internal/score"
)

func finalist(id uint32, nets []float64) Finalist {
	cum := 0.0
	trades := make([]Kept, len(nets))
	for k, n := range nets {
		cum += n
		trades[k] = Kept{
			EntryMs: int64(k) * 10,
			ExitMs:  int64(k)*10 + 5,
			Cum:     cum,
		}
	}
	return Finalist{ID: id, Trades: trades}
}

func TestTestSize(t *testing.T) {
	if TestSize(9) != 3 || TestSize(10) != 3 || TestSize(3) != 1 {
		t.Fatalf("TestSize unexpected")
	}
}

func TestTrainingLookback(t *testing.T) {
	exits := make([]int64, 10)
	for i := range exits {
		exits[i] = int64(i)*10 + 5
	}
	lo, hi := Training(exits, 95, 100, 50)
	if lo != 5 || hi != 10 {
		t.Fatalf("got %d..%d want 5..10", lo, hi)
	}
	lo, hi = Training(exits, 95, 3, 50)
	if lo != 7 || hi != 10 {
		t.Fatalf("got %d..%d want 7..10", lo, hi)
	}
	f := finalist(1, []float64{5, 5, 5, 5, 5, 5, 5, 5, 5, 5})
	model := score.ColdStart()
	if PickBest([]Finalist{f}, 95, 100, 40, &model) != nil {
		t.Fatal("expected no pick with < MIN_TRADES in lookback")
	}
}

func TestPickMatchesBruteForce(t *testing.T) {
	var x uint64 = 5
	rand := func() float64 {
		x = x*6364136223846793005 + 1442695040888963407
		return float64(x>>33) / float64(uint64(1)<<31)
	}
	brute := func(finalists []Finalist, at int64, n int, lookback int64, model *score.ScoreModel) *int {
		type cand struct {
			k      int
			sc     float64
			profit float32
			id     uint32
		}
		var best *cand
		for k, f := range finalists {
			exits := make([]int64, len(f.Trades))
			for i, tr := range f.Trades {
				exits[i] = tr.ExitMs
			}
			lo, hi := Training(exits, at, n, lookback)
			if hi-lo < score.MinTrades {
				continue
			}
			before := 0.0
			if lo > 0 {
				before = f.Trades[lo-1].Cum
			}
			if f.Trades[hi-1].Cum-before <= MinISProfit {
				continue
			}
			var tally score.Tally
			for i := lo; i < hi; i++ {
				tally.Add(f.Net(i), f.Trades[i].ExitMs-f.Trades[i].EntryMs)
				tally.NoteExit(f.Trades[i].ExitMs)
			}
			st := tally.Stats().WithScoreClock(at, lookback)
			sc, ok := model.Predict(&st)
			if !ok {
				continue
			}
			c := cand{k: k, sc: sc, profit: st.Profit, id: f.ID}
			if best == nil || c.sc > best.sc || (c.sc == best.sc && c.profit > best.profit) ||
				(c.sc == best.sc && c.profit == best.profit && c.id < best.id) {
				cp := c
				best = &cp
			}
		}
		if best == nil {
			return nil
		}
		k := best.k
		return &k
	}
	picked := 0
	model := score.ColdStart()
	for round := 0; round < 400; round++ {
		finalists := make([]Finalist, 40)
		for id := 0; id < 40; id++ {
			length := 3 + int(rand()*30)
			drift := rand() - 0.5
			nets := make([]float64, length)
			for i := range nets {
				nets[i] = math.Round((rand() - 0.5 + drift) * 6)
			}
			finalists[id] = finalist(uint32(id*7%40), nets)
		}
		at := int64(rand() * 350)
		n := 3 + int(rand()*12)
		lookback := int64(40 + rand()*200)
		got := PickBest(finalists, at, n, lookback, &model)
		want := brute(finalists, at, n, lookback, &model)
		var gi *int
		if got != nil {
			gi = &got.Finalist
		}
		if (gi == nil) != (want == nil) || (gi != nil && want != nil && *gi != *want) {
			t.Fatalf("round %d: got %v want %v", round, gi, want)
		}
		if got != nil {
			picked++
		}
	}
	if picked <= 100 {
		t.Fatalf("too few picks tested: %d", picked)
	}
}

func TestAdvanceChainTakesNextM(t *testing.T) {
	f := finalist(0, []float64{1, 1, 1, -1, 2, 2, 2, 1, 1})
	chain := Chain{N: 5, M: 2, At: 46, StopReason: ""}
	model := score.ColdStart()
	pairs := Advance(&chain, []Finalist{f}, 60, 1000, &model)
	// M=2 < MIN_TRADES, so no IS→OOS fit sample; the chain still takes the next M trades.
	if len(pairs) != 0 {
		t.Fatalf("pairs=%d want 0 (OOS shorter than MIN_TRADES)", len(pairs))
	}
	if len(chain.Windows) != 1 || chain.Windows[0] != 4 {
		t.Fatalf("windows=%v", chain.Windows)
	}
	if chain.At != 65 {
		t.Fatalf("at=%d want 65", chain.At)
	}
	if chain.Trades[0].EntryMs != 50 || chain.Trades[1].ExitMs != 65 {
		t.Fatalf("trades=%+v", chain.Trades)
	}
}

func TestAdvanceEmitsFitPairWhenOOSLongEnough(t *testing.T) {
	// N=5, M=5 so OOS clears MIN_TRADES and yields a fit pair.
	nets := make([]float64, 20)
	for i := range nets {
		nets[i] = 1
	}
	f := finalist(0, nets)
	chain := Chain{N: 5, M: 5, At: 46, StopReason: ""}
	model := score.ColdStart()
	pairs := Advance(&chain, []Finalist{f}, 200, 1000, &model)
	if len(pairs) < 1 {
		t.Fatalf("expected at least one fit pair, got %d (windows=%v)", len(pairs), chain.Windows)
	}
	if len(chain.Windows) < 1 {
		t.Fatal("expected chain windows")
	}
}

func TestParseArgsScorePath(t *testing.T) {
	args, err := ParseArgs([]string{
		"--scan", "data/x",
		"--lookback-days", "14",
		"--train-trades", "30,20,10",
		"--require-fit", "0",
		"--max-rel-err", "0.05",
		"--path", "score",
	})
	if err != nil {
		t.Fatal(err)
	}
	if args.Scan != "data/x" || args.LookbackDays != 14 || args.RequireFit || args.MaxRelErr != 0.05 {
		t.Fatalf("%+v", args)
	}
	if len(args.TrainTrades) != 3 || args.TrainTrades[0] != 30 {
		t.Fatalf("train=%v", args.TrainTrades)
	}
}
