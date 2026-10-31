package score

import (
	"math"
	"testing"
)

func stats(trades, wins uint16) WindowStats {
	return WindowStats{
		Profit: 50, Drop: 10, Worst: -5, HoldHours: 0,
		Trades: trades, Wins: wins,
	}
}

func TestFewTradesAreNotTrusted(t *testing.T) {
	round := func(n, w uint16) float64 {
		return math.Round(stats(n, w).TrustedWinRate() * 100)
	}
	if got := round(1, 1); got != 21 {
		t.Fatalf("trusted(1,1)=%v want 21", got)
	}
	if got := round(10, 6); got != 31 {
		t.Fatalf("trusted(10,6)=%v want 31", got)
	}
	if got := round(100, 60); got != 50 {
		t.Fatalf("trusted(100,60)=%v want 50", got)
	}
	if _, ok := stats(4, 4).Score(); ok {
		t.Fatal("4 trades should not score")
	}
	if _, ok := stats(5, 5).Score(); !ok {
		t.Fatal("5 trades should score")
	}
}

func TestEveryPartCounts(t *testing.T) {
	s := WindowStats{
		Profit: 50, Drop: 10, Worst: -5, HoldHours: 24,
		Trades: 100, Wins: 60,
	}
	sc, ok := s.Score()
	if !ok {
		t.Fatal("expected score")
	}
	expected := 5.0 * s.TrustedWinRate() * (50.0 / 55.0) * 0.5
	if math.Abs(sc-expected) >= 1e-9 {
		t.Fatalf("score=%v want %v", sc, expected)
	}
	small := s
	small.Drop = 1
	scSmall, _ := small.Score()
	scBase, _ := s.Score()
	if math.Abs(scSmall-scBase*10.0/5.0) >= 1e-9 {
		t.Fatalf("small drop score mismatch")
	}
	longer := s
	longer.HoldHours = 48
	sl, _ := longer.Score()
	if !(sl < scBase) {
		t.Fatal("longer hold should lower score")
	}
	worse := s
	worse.Worst = -40
	sw, _ := worse.Score()
	if !(sw < scBase) {
		t.Fatal("worse loss should lower score")
	}
	fewerWins := s
	fewerWins.Wins = 50
	sf, _ := fewerWins.Score()
	if !(sf < scBase) {
		t.Fatal("fewer wins should lower score")
	}
	biggerDrop := s
	biggerDrop.Drop = 20
	sd, _ := biggerDrop.Score()
	if !(sd < scBase) {
		t.Fatal("bigger drop should lower score")
	}
}

func TestLosersRankBelowEveryWinner(t *testing.T) {
	tinyWin := WindowStats{
		Profit: 0.01, Drop: 50, Worst: -40, HoldHours: 200,
		Trades: 5, Wins: 1,
	}
	smallLoss := WindowStats{
		Profit: -0.01, Drop: 5, Worst: -1, HoldHours: 1,
		Trades: 50, Wins: 40,
	}
	tw, _ := tinyWin.Score()
	sl, _ := smallLoss.Score()
	if !(tw > 0) || !(sl < 0) {
		t.Fatalf("tinyWin=%v smallLoss=%v", tw, sl)
	}
	bigLoss := smallLoss
	bigLoss.Profit = -20
	bl, _ := bigLoss.Score()
	if !(bl < sl) {
		t.Fatal("bigger loss should rank lower")
	}
}

func TestTallyCountsTradeByTrade(t *testing.T) {
	var tally Tally
	for _, pair := range []struct {
		net   float64
		hours int64
	}{{3, 1}, {-4, 2}, {-2, 3}, {10, 6}} {
		tally.Add(pair.net, pair.hours*3_600_000)
	}
	s := tally.Stats()
	if s.Profit != 7 || s.Drop != 6 || s.Worst != -4 || s.HoldHours != 3 || s.Trades != 4 || s.Wins != 2 {
		t.Fatalf("got %+v", s)
	}
	b := s.Bytes()
	round := FromBytes(b[:])
	if round.Profit != s.Profit || round.Drop != s.Drop || round.Worst != s.Worst ||
		round.HoldHours != s.HoldHours || round.Trades != s.Trades || round.Wins != s.Wins {
		t.Fatalf("from_bytes mismatch: %+v vs %+v", round, s)
	}
}

func TestLearnedScoreFitsUnderOnePercentError(t *testing.T) {
	trueW := ProjectBudget([ScoreFeatures]float64{2, 1.5, 2, 1, 2, 1.5})
	sum := 0.0
	for _, w := range trueW {
		sum += w
	}
	if math.Abs(sum-ScoreBudget) >= 1e-9 {
		t.Fatalf("true_w sum=%v", sum)
	}
	samples := make([]Sample, 0, 40)
	for k := 0; k < 40; k++ {
		s := WindowStats{
			Profit: 10 + float32(k), Drop: 8, Worst: -3,
			HoldHours: float32(k % 5),
			Trades:    uint16(20 + k%10),
			Wins:      uint16(12 + k%8),
			LastExitMs: 1_000_000 + int64(k)*86_400_000,
			ScoreAtMs:  1_000_000 + 30*86_400_000,
			LookbackMs: 30 * 86_400_000,
		}
		x := s.ScoreFeatures()
		y := 0.0
		for i := 0; i < ScoreFeatures; i++ {
			y += trueW[i] * x[i]
		}
		samples = append(samples, Sample{Stats: s, Target: y})
	}
	model, err := Fit(samples, nil)
	if err != nil {
		t.Fatal(err)
	}
	if model.ColdStart {
		t.Fatal("expected learned model")
	}
	wsum := 0.0
	for _, w := range model.Weights {
		wsum += w
	}
	if math.Abs(wsum-ScoreBudget) >= 1e-6 {
		t.Fatalf("weights sum %v", wsum)
	}
	if model.ScoreRelErr >= MaxScoreRelErr {
		t.Fatalf("rel_err=%v", model.ScoreRelErr)
	}
	if !model.ScoreOK() {
		t.Fatal("score_ok")
	}
}

func TestColdStartWeightsSumToBudget(t *testing.T) {
	m := ColdStart()
	sum := 0.0
	for _, w := range m.Weights {
		sum += w
	}
	if math.Abs(sum-ScoreBudget) >= 1e-9 {
		t.Fatalf("sum=%v", sum)
	}
	if m.Intercept != 0 {
		t.Fatal("intercept")
	}
	perfect := [ScoreFeatures]float64{1, 1, 1, 1, 1, 1}
	if math.Abs(m.PredictFeatures(&perfect)-ScoreBudget) >= 1e-9 {
		t.Fatal("perfect features should score 10")
	}
}

func TestUntilFactorIsLastTradeFreshness(t *testing.T) {
	lookback := int64(10 * 86_400_000)
	at := int64(100 * 86_400_000)
	fresh := WindowStats{LastExitMs: at, ScoreAtMs: at, LookbackMs: lookback}
	if math.Abs(fresh.UntilFactor()-1) >= 1e-9 {
		t.Fatalf("fresh=%v", fresh.UntilFactor())
	}
	old := WindowStats{LastExitMs: at - lookback, ScoreAtMs: at, LookbackMs: lookback}
	if math.Abs(old.UntilFactor()-0) >= 1e-9 {
		t.Fatalf("old=%v", old.UntilFactor())
	}
	if math.Abs((WindowStats{}).UntilFactor()-0.5) >= 1e-9 {
		t.Fatal("default until")
	}
}

func TestLearnedScoreRejectsUnfittableNoise(t *testing.T) {
	samples := make([]Sample, 0, 20)
	for k := 0; k < 20; k++ {
		s := WindowStats{Profit: 5, Drop: 5, Worst: -1, HoldHours: 1, Trades: 10, Wins: 5}
		target := 0.0
		if k%2 != 0 {
			target = 10
		}
		samples = append(samples, Sample{Stats: s, Target: target})
	}
	model, err := Fit(samples, nil)
	if err != nil {
		t.Fatal(err)
	}
	if model.ColdStart {
		t.Fatal("should keep best weights")
	}
	if model.ScoreOK() {
		t.Fatalf("noise must stay above 1%% gate, rel_err=%v", model.ScoreRelErr)
	}
}

func TestSameWindowTargetIsBudgetFormula(t *testing.T) {
	s := WindowStats{
		Profit: 50, Drop: 10, Worst: -5, HoldHours: 2,
		Trades: 40, Wins: 24,
		LastExitMs: 10_000_000, ScoreAtMs: 10_000_000, LookbackMs: 30 * 86_400_000,
	}
	f := s.ScoreFeatures()
	budget := ColdStart().PredictFeatures(&f)
	if math.Abs(s.TargetQuality()-budget) >= 1e-9 {
		t.Fatal("target_quality")
	}
	raw, _ := s.Score()
	var heuristic float64
	if raw >= 0 {
		heuristic = clamp(5+5*(raw/(raw+2)), 0, ScoreBudget)
	} else {
		heuristic = clamp(5+5*(raw/(math.Abs(raw)+2)), 0, ScoreBudget)
	}
	if math.Abs(s.TargetQuality()-heuristic) <= 0.05 {
		t.Fatal("S* must follow budget-10, not heuristic map")
	}
	samples := make([]Sample, 12)
	for i := range samples {
		samples[i] = Sample{Stats: s, Target: s.TargetQuality()}
	}
	model, err := Fit(samples, nil)
	if err != nil {
		t.Fatal(err)
	}
	if model.ScoreRelErr >= MaxScoreRelErr {
		t.Fatalf("rel_err=%v", model.ScoreRelErr)
	}
}

func TestWarmStartKeepsPreviousLineWhenBetter(t *testing.T) {
	trueW := ProjectBudget([ScoreFeatures]float64{2, 1.5, 2, 1, 2, 1.5})
	samples := make([]Sample, 0, 30)
	for k := 0; k < 30; k++ {
		s := WindowStats{
			Profit: 20 + float32(k), Drop: 8, Worst: -3, HoldHours: 1,
			Trades: 20, Wins: 12,
			LastExitMs: 1_000_000 + int64(k)*86_400_000,
			ScoreAtMs:  1_000_000 + 30*86_400_000,
			LookbackMs: 30 * 86_400_000,
		}
		x := s.ScoreFeatures()
		y := 0.0
		for i := 0; i < ScoreFeatures; i++ {
			y += trueW[i] * x[i]
		}
		samples = append(samples, Sample{Stats: s, Target: y})
	}
	day1, err := Fit(samples[:15], nil)
	if err != nil {
		t.Fatal(err)
	}
	day2, err := Fit(samples, &day1)
	if err != nil {
		t.Fatal(err)
	}
	if day2.ColdStart {
		t.Fatal("day2 cold")
	}
	if day2.ScoreRelErr > day1.RelativeError(samples)+1e-9 {
		t.Fatalf("day2 err %v > day1 on full %v", day2.ScoreRelErr, day1.RelativeError(samples))
	}
	if !day2.ScoreOK() {
		t.Fatal("day2 not ok")
	}
}

func TestColdStartWhenFewSamples(t *testing.T) {
	m, err := Fit(nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !m.ColdStart || !m.ScoreOK() {
		t.Fatalf("%+v", m)
	}
}
