// Package score ports src/window_score.rs: WindowStats and the learned ScoreModel.
package score

import (
	"encoding/binary"
	"fmt"
	"math"
)

const (
	MinTrades       = 5
	MinDrop         = 5.0
	Record          = 20
	MaxScoreRelErr  = 0.01
	ScoreFeatures   = 6
	ScoreBudget     = 10.0
	MinFitSamples   = 8
)

// WindowStats is one combination over one window.
type WindowStats struct {
	Profit      float32
	Drop        float32
	Worst       float32
	HoldHours   float32
	Trades      uint16
	Wins        uint16
	LastExitMs  int64
	ScoreAtMs   int64
	LookbackMs  int64
}

func (s WindowStats) WinRate() float64 {
	if s.Trades == 0 {
		return 0
	}
	return float64(s.Wins) / float64(s.Trades)
}

// TrustedWinRate is the lower end of the 95% Wilson confidence interval.
func (s WindowStats) TrustedWinRate() float64 {
	n := float64(s.Trades)
	if n == 0 {
		return 0
	}
	z := 1.96
	p := float64(s.Wins) / n
	z2 := z * z
	return (p + z2/(2.0*n) - z*math.Sqrt(p*(1.0-p)/n+z2/(4.0*n*n))) / (1.0 + z2/n)
}

func (s WindowStats) RiskFactor() float64 {
	profit := float64(s.Profit)
	if profit <= 0 {
		return 0
	}
	worst := float64(s.Worst)
	if worst > 0 {
		worst = 0
	}
	return profit / (profit - worst)
}

func (s WindowStats) HoldFactor() float64 {
	return 1.0 / (1.0 + float64(s.HoldHours)/24.0)
}

func (s WindowStats) Score() (float64, bool) {
	if uint32(s.Trades) < MinTrades {
		return 0, false
	}
	base := float64(s.Profit) / math.Max(float64(s.Drop), MinDrop)
	if s.Profit <= 0 {
		return base, true
	}
	return base * s.TrustedWinRate() * s.RiskFactor() * s.HoldFactor(), true
}

func (s WindowStats) Bytes() [Record]byte {
	var b [Record]byte
	binary.LittleEndian.PutUint32(b[0:4], math.Float32bits(s.Profit))
	binary.LittleEndian.PutUint32(b[4:8], math.Float32bits(s.Drop))
	binary.LittleEndian.PutUint32(b[8:12], math.Float32bits(s.Worst))
	binary.LittleEndian.PutUint32(b[12:16], math.Float32bits(s.HoldHours))
	binary.LittleEndian.PutUint16(b[16:18], s.Trades)
	binary.LittleEndian.PutUint16(b[18:20], s.Wins)
	return b
}

func FromBytes(b []byte) WindowStats {
	f := func(k int) float32 {
		return math.Float32frombits(binary.LittleEndian.Uint32(b[k : k+4]))
	}
	u := func(k int) uint16 {
		return binary.LittleEndian.Uint16(b[k : k+2])
	}
	return WindowStats{
		Profit:    f(0),
		Drop:      f(4),
		Worst:     f(8),
		HoldHours: f(12),
		Trades:    u(16),
		Wins:      u(18),
	}
}

func (s WindowStats) ScaleLeverage(leverage uint32) WindowStats {
	l := float32(leverage)
	s.Profit *= l
	s.Drop *= l
	s.Worst *= l
	return s
}

func (s WindowStats) WithScoreClock(at, lookbackMs int64) WindowStats {
	s.ScoreAtMs = at
	s.LookbackMs = lookbackMs
	return s
}

func (s WindowStats) UntilFactor() float64 {
	if s.LastExitMs <= 0 || s.ScoreAtMs <= 0 || s.LookbackMs <= 0 {
		return 0.5
	}
	start := s.ScoreAtMs - s.LookbackMs
	v := float64(s.LastExitMs-start) / float64(s.LookbackMs)
	return clamp01(v)
}

func (s WindowStats) ScoreFeatures() [ScoreFeatures]float64 {
	profit := float64(s.Profit)
	drop := math.Max(float64(s.Drop), MinDrop)
	worst := float64(s.Worst)
	if worst > 0 {
		worst = 0
	}
	wr := clamp01(s.WinRate())
	gain := clamp01(math.Tanh(profit / 100.0))
	var rr float64
	if worst < 0 {
		rr = clamp(profit/(-worst), 0, 5) / 5.0
	} else if profit > 0 {
		rr = 1.0
	}
	until := s.UntilFactor()
	drawdown := clamp01(MinDrop / drop)
	n := clamp01(math.Log(1.0+float64(s.Trades)) / math.Log(1.0+200.0))
	return [ScoreFeatures]float64{wr, gain, rr, until, drawdown, n}
}

func (s WindowStats) TargetQuality() float64 {
	if uint32(s.Trades) < MinTrades {
		return 0
	}
	f := s.ScoreFeatures()
	m := ColdStart()
	return m.PredictFeatures(&f)
}

// ScoreModel is learned ranking: S = Σ wᵢ fᵢ, Σ w = ScoreBudget.
type ScoreModel struct {
	Weights     [ScoreFeatures]float64
	Intercept   float64
	ScoreMAE    float64
	ScoreRelErr float64
	Samples     int
	ColdStart   bool
}

func ColdStart() ScoreModel {
	return ScoreModel{
		Weights:   [ScoreFeatures]float64{2.0, 1.5, 2.0, 1.0, 2.0, 1.5},
		Intercept: 0,
		ColdStart: true,
	}
}

func (m *ScoreModel) ScoreOK() bool {
	return m.ColdStart || m.ScoreRelErr < MaxScoreRelErr
}

func (m *ScoreModel) Predict(stats *WindowStats) (float64, bool) {
	if uint32(stats.Trades) < MinTrades {
		return 0, false
	}
	f := stats.ScoreFeatures()
	return m.PredictFeatures(&f), true
}

func (m ScoreModel) PredictFeatures(x *[ScoreFeatures]float64) float64 {
	s := 0.0
	for i := 0; i < ScoreFeatures; i++ {
		s += m.Weights[i] * clamp01(x[i])
	}
	return clamp(s, 0, ScoreBudget)
}

type Sample struct {
	Stats  WindowStats
	Target float64
}

func (m *ScoreModel) RelativeError(samples []Sample) float64 {
	if len(samples) == 0 {
		return math.Inf(1)
	}
	sum := 0.0
	n := 0
	for i := range samples {
		pred, ok := m.Predict(&samples[i].Stats)
		if !ok {
			continue
		}
		sum += math.Abs(pred - samples[i].Target)
		n++
	}
	if n == 0 {
		return math.Inf(1)
	}
	return (sum / float64(n)) / ScoreBudget
}

func Fit(samples []Sample, warm *ScoreModel) (ScoreModel, error) {
	if len(samples) < MinFitSamples {
		var m ScoreModel
		if warm != nil {
			m = *warm
			m.ColdStart = warm.ColdStart
		} else {
			m = ColdStart()
		}
		if len(samples) > 0 {
			m.ScoreRelErr = m.RelativeError(samples)
			m.ScoreMAE = m.ScoreRelErr * ScoreBudget
			m.Samples = len(samples)
		}
		return m, nil
	}
	var warmW *[ScoreFeatures]float64
	if warm != nil {
		w := warm.Weights
		warmW = &w
	}
	var best *ScoreModel
	for _, seed := range budgetFitSeeds(warmW) {
		model, err := budgetFit(samples, seed)
		if err != nil {
			continue
		}
		if best == nil || model.ScoreRelErr < best.ScoreRelErr {
			cp := model
			best = &cp
		}
	}
	if best == nil {
		return ScoreModel{}, fmt.Errorf("score fit failed for all budget-10 seeds")
	}
	model := *best
	if warm != nil && !warm.ColdStart {
		prevErr := warm.RelativeError(samples)
		if prevErr < model.ScoreRelErr {
			model.Weights = warm.Weights
			model.ScoreRelErr = prevErr
			model.ScoreMAE = prevErr * ScoreBudget
			model.Samples = len(samples)
			model.ColdStart = false
		}
	}
	return model, nil
}

func FitPairsFromWindows(windows []WindowStats) []Sample {
	out := make([]Sample, 0)
	for i := 0; i+1 < len(windows); i++ {
		if uint32(windows[i].Trades) < MinTrades || uint32(windows[i+1].Trades) < MinTrades {
			continue
		}
		out = append(out, Sample{Stats: windows[i], Target: windows[i+1].TargetQuality()})
	}
	return out
}

func budgetFitSeeds(warm *[ScoreFeatures]float64) [][ScoreFeatures]float64 {
	equal := [ScoreFeatures]float64{}
	for i := range equal {
		equal[i] = ScoreBudget / float64(ScoreFeatures)
	}
	cold := ColdStart().Weights
	mkHeavy := func(idx int) [ScoreFeatures]float64 {
		w := equal
		w[idx] = 4.0
		return ProjectBudget(w)
	}
	seeds := [][ScoreFeatures]float64{
		equal,
		cold,
		mkHeavy(0),
		mkHeavy(1),
		mkHeavy(2),
		mkHeavy(3),
		mkHeavy(4),
	}
	if warm != nil {
		seeds = append([][ScoreFeatures]float64{ProjectBudget(*warm)}, seeds...)
	}
	return seeds
}

// ProjectBudget enforces w ≥ 0 and Σ w = ScoreBudget.
func ProjectBudget(w [ScoreFeatures]float64) [ScoreFeatures]float64 {
	sum := 0.0
	for i := range w {
		if w[i] < 0 {
			w[i] = 0
		}
		sum += w[i]
	}
	if sum <= 1e-12 {
		eq := ScoreBudget / float64(ScoreFeatures)
		for i := range w {
			w[i] = eq
		}
		return w
	}
	scale := ScoreBudget / sum
	for i := range w {
		w[i] *= scale
	}
	return w
}

func budgetFit(samples []Sample, seed [ScoreFeatures]float64) (ScoreModel, error) {
	rows := make([]struct {
		x [ScoreFeatures]float64
		y float64
	}, 0, len(samples))
	for i := range samples {
		if uint32(samples[i].Stats.Trades) < MinTrades {
			continue
		}
		rows = append(rows, struct {
			x [ScoreFeatures]float64
			y float64
		}{samples[i].Stats.ScoreFeatures(), samples[i].Target})
	}
	if len(rows) < MinFitSamples {
		return ScoreModel{}, fmt.Errorf("not enough scored samples")
	}
	n := float64(len(rows))
	w := ProjectBudget(seed)
	bestW := w
	bestMSE := math.Inf(1)
	lr0 := 0.35
	for step := 0; step < 400; step++ {
		var grad [ScoreFeatures]float64
		mse := 0.0
		for _, row := range rows {
			pred := 0.0
			for i := 0; i < ScoreFeatures; i++ {
				pred += w[i] * row.x[i]
			}
			err := pred - row.y
			mse += err * err
			for i := 0; i < ScoreFeatures; i++ {
				grad[i] += 2.0 * err * row.x[i] / n
			}
		}
		mse /= n
		if mse < bestMSE {
			bestMSE = mse
			bestW = w
		}
		lr := lr0 / (1.0 + float64(step)*0.01)
		for i := 0; i < ScoreFeatures; i++ {
			w[i] -= lr * grad[i]
		}
		w = ProjectBudget(w)
	}
	model := ScoreModel{
		Weights:   bestW,
		Intercept: 0,
		Samples:   len(rows),
		ColdStart: false,
	}
	model.ScoreRelErr = model.RelativeError(samples)
	model.ScoreMAE = model.ScoreRelErr * ScoreBudget
	return model, nil
}

// Tally builds WindowStats trade by trade.
type Tally struct {
	sum        float64
	peak       float64
	drop       float64
	worst      float64
	heldMs     int64
	trades     uint32
	wins       uint32
	lastExitMs int64
}

func (t *Tally) Add(net float64, heldMs int64) {
	t.sum += net
	if t.sum > t.peak {
		t.peak = t.sum
	}
	d := t.peak - t.sum
	if d > t.drop {
		t.drop = d
	}
	if net < t.worst {
		t.worst = net
	}
	t.heldMs += heldMs
	t.trades++
	if net > 0 {
		t.wins++
	}
}

func (t *Tally) NoteExit(exitMs int64) {
	t.lastExitMs = exitMs
}

func (t *Tally) Stats() WindowStats {
	hold := float32(0)
	if t.trades > 0 {
		hold = float32(float64(t.heldMs) / float64(t.trades) / 3_600_000.0)
	}
	trades := t.trades
	wins := t.wins
	if trades > math.MaxUint16 {
		trades = math.MaxUint16
	}
	if wins > math.MaxUint16 {
		wins = math.MaxUint16
	}
	return WindowStats{
		Profit:     float32(t.sum),
		Drop:       float32(t.drop),
		Worst:      float32(t.worst),
		HoldHours:  hold,
		Trades:     uint16(trades),
		Wins:       uint16(wins),
		LastExitMs: t.lastExitMs,
	}
}

func clamp01(x float64) float64 { return clamp(x, 0, 1) }

func clamp(x, lo, hi float64) float64 {
	if x < lo {
		return lo
	}
	if x > hi {
		return hi
	}
	return x
}
