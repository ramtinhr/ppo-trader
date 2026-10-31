// Package exits ports TP/SL ladder types shared by Go research paths.
package exits

import (
	"fmt"
	"strings"
)

// Exits is a stop plus take-profit ladder placed on entry.
type Exits struct {
	StopPct   float64    `json:"stop_pct"`
	Targets   [][2]float64 `json:"targets"` // (pct, share)
	Breakeven bool       `json:"breakeven"`
}

// Profile is a scale-out template (R multiples × share %).
type Profile struct {
	Label     string
	LadderR   []float64
	SharesPct []float64
	Breakeven bool
}

func DefaultScaleoutProfiles() []Profile {
	return []Profile{
		{Label: "1R", LadderR: []float64{1}, SharesPct: []float64{100}},
		{Label: "1.5R", LadderR: []float64{1.5}, SharesPct: []float64{100}},
		{Label: "2R", LadderR: []float64{2}, SharesPct: []float64{100}},
		{Label: "1R+2R", LadderR: []float64{1, 2}, SharesPct: []float64{50, 50}, Breakeven: true},
		{Label: "1R+3R", LadderR: []float64{1, 3}, SharesPct: []float64{50, 50}, Breakeven: true},
		{Label: "1R+2R+3R", LadderR: []float64{1, 2, 3}, SharesPct: []float64{34, 33, 33}, Breakeven: true},
	}
}

func (p Profile) ToExits(stopPct float64) Exits {
	tgts := make([][2]float64, len(p.LadderR))
	for i := range p.LadderR {
		tgts[i] = [2]float64{p.LadderR[i] * stopPct, p.SharesPct[i] / 100}
	}
	return Exits{
		StopPct:   stopPct,
		Targets:   tgts,
		Breakeven: p.Breakeven && len(p.LadderR) > 1,
	}
}

func (e Exits) Validate(sizeSteps int64) error {
	if e.StopPct <= 0 || e.StopPct >= 50 {
		return fmt.Errorf("stop-loss must be between 0%% and 50%%")
	}
	if len(e.Targets) == 0 {
		return fmt.Errorf("at least one take-profit is needed")
	}
	if e.Targets[0][0] <= 0 {
		return fmt.Errorf("take-profits must be positive")
	}
	for i := 1; i < len(e.Targets); i++ {
		if e.Targets[i][0] <= e.Targets[i-1][0] {
			return fmt.Errorf("take-profits must increase")
		}
	}
	var total float64
	for _, t := range e.Targets {
		total += t[1]
	}
	if abs(total-1) > 1e-9 {
		return fmt.Errorf("take-profit shares must add up to 100%%")
	}
	for _, s := range e.TargetSteps(sizeSteps) {
		if s <= 0 {
			return fmt.Errorf("position too small to split into %d take-profits", len(e.Targets))
		}
	}
	return nil
}

func (e Exits) TargetSteps(steps int64) []int64 {
	sizes := make([]int64, len(e.Targets))
	var sum int64
	for i, t := range e.Targets {
		if i == len(e.Targets)-1 {
			break
		}
		sizes[i] = int64(float64(steps)*t[1] + 1e-9)
		sum += sizes[i]
	}
	sizes[len(sizes)-1] = steps - sum
	return sizes
}

func (e Exits) Label() string {
	join := func(f func([2]float64) float64) string {
		parts := make([]string, len(e.Targets))
		for i, t := range e.Targets {
			parts[i] = fmt.Sprintf("%.2g", float64(int(f(t)*100+0.5))/100)
		}
		return strings.Join(parts, "/")
	}
	shares := ""
	if len(e.Targets) > 1 {
		shares = " · " + join(func(t [2]float64) float64 { return t[1] * 100 }) + "%"
	}
	be := ""
	if e.Breakeven {
		be = " · BE"
	}
	return fmt.Sprintf("SL %g%% · TP %s%%%s%s", e.StopPct, join(func(t [2]float64) float64 { return t[0] }), shares, be)
}

func abs(x float64) float64 {
	if x < 0 {
		return -x
	}
	return x
}
