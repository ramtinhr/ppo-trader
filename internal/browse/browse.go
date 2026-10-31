// Package browse serves the merged window/month scan browser HTTP API.
package browse

import (
	"bytes"
	"embed"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/ramtin/dynamic-trader/internal/score"
)

//go:embed static/*
var staticFS embed.FS

const (
	windowRecord = 20
	monthRecord  = 12
	maxRows      = 1_000_000
)

var wfReportStems = []string{
	"nm_walk_forward",
	"nm_walk_forward_allow_short",
	"nm_price_walk_forward",
}

// Layout is the on-disk scan record layout.
type Layout string

const (
	LayoutWindow Layout = "window"
	LayoutMonth  Layout = "month"
	LayoutAuto   Layout = "auto"
)

// Options for Serve.
type Options struct {
	ScanDir string
	Port    int
	Layout  Layout
	MinDrop float64
}

type windowMeta struct {
	Timeframes      []int       `json:"timeframes"`
	Lengths         []int       `json:"lengths"`
	TP              []float64   `json:"tp"`
	SL              []float64   `json:"sl"`
	Leverage        []uint32    `json:"leverage"`
	Combinations    int         `json:"combinations"`
	EveryPair       int         `json:"every_pair"`
	Days            uint64      `json:"days"`
	Hours           *uint64     `json:"hours"`
	Windows         []winEntry  `json:"windows"`
	Months          []string    `json:"months"`
	SurvivorGroups  []int       `json:"survivor_groups"`
	ScanVersion     int         `json:"scan_version"`
	Record          string      `json:"record"`
}

type winEntry struct {
	Start string `json:"start"`
	End   string `json:"end"`
	File  string `json:"file"`
}

type scan struct {
	meta     windowMeta
	pairs    [][2]int
	settings int
	files    []*os.File
	layout   Layout
	minDrop  float64
	dir      string
}

type windowRow struct {
	id    uint32
	stats score.WindowStats
	sc    float64
}

type monthRow struct {
	id     uint32
	profit float32
	drop   float32
	trades uint32
	sc     float32
}

type filter struct {
	window     int // or period for month
	tf         *int
	fast       *int
	slow       *int
	tp         *int
	sl         *int
	lev        *int
	minTrades  uint32
	scored     bool
	profitable bool
	sort       []sortKey
}

type sortKey struct {
	name string
	asc  bool
}

// DetectLayout reads meta.json and optionally probes a bin file size.
func DetectLayout(dir string, want Layout) (Layout, error) {
	raw, err := os.ReadFile(filepath.Join(dir, "meta.json"))
	if err != nil {
		return "", fmt.Errorf("no finished scan in %s: %w", dir, err)
	}
	var m windowMeta
	if err := json.Unmarshal(raw, &m); err != nil {
		return "", err
	}
	if want == LayoutWindow || want == LayoutMonth {
		return want, nil
	}
	if len(m.Windows) > 0 || m.ScanVersion > 0 || strings.Contains(m.Record, "20-byte") || strings.Contains(m.Record, "window_") {
		return LayoutWindow, nil
	}
	if len(m.Months) > 0 {
		return LayoutMonth, nil
	}
	// Probe first .bin
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".bin") {
			continue
		}
		st, err := os.Stat(filepath.Join(dir, e.Name()))
		if err != nil || st.Size() == 0 {
			continue
		}
		if m.Combinations > 0 {
			if st.Size()%int64(windowRecord) == 0 && st.Size()/int64(windowRecord) == int64(m.Combinations) {
				return LayoutWindow, nil
			}
			if st.Size()%int64(monthRecord) == 0 && st.Size()/int64(monthRecord) == int64(m.Combinations) {
				return LayoutMonth, nil
			}
		}
		if st.Size()%int64(windowRecord) == 0 {
			return LayoutWindow, nil
		}
		if st.Size()%int64(monthRecord) == 0 {
			return LayoutMonth, nil
		}
	}
	return "", fmt.Errorf("cannot detect scan layout in %s (pass --layout window|month)", dir)
}

func openScan(dir string, layout Layout, minDrop float64) (*scan, error) {
	raw, err := os.ReadFile(filepath.Join(dir, "meta.json"))
	if err != nil {
		return nil, err
	}
	var m windowMeta
	if err := json.Unmarshal(raw, &m); err != nil {
		return nil, err
	}
	if len(m.Leverage) == 0 {
		m.Leverage = []uint32{1}
	}
	if m.EveryPair != 0 && m.EveryPair != 1 {
		return nil, fmt.Errorf("scan ran on every %dth RSI pair only", m.EveryPair)
	}
	pairs := make([][2]int, 0)
	for i := 0; i < len(m.Lengths); i++ {
		for j := i + 1; j < len(m.Lengths); j++ {
			pairs = append(pairs, [2]int{i, j})
		}
	}
	settings := len(m.TP) * len(m.SL) * max(1, len(m.Leverage))
	s := &scan{meta: m, pairs: pairs, settings: settings, layout: layout, minDrop: minDrop, dir: dir}
	if layout == LayoutWindow {
		nGroups := len(m.Timeframes) * len(pairs)
		if len(m.SurvivorGroups) > 0 {
			nGroups = len(m.SurvivorGroups)
		}
		if nGroups*settings != m.Combinations {
			return nil, fmt.Errorf("meta.json does not add up (groups=%d settings=%d combos=%d)", nGroups, settings, m.Combinations)
		}
		for _, w := range m.Windows {
			f, err := os.Open(filepath.Join(dir, w.File))
			if err != nil {
				return nil, err
			}
			s.files = append(s.files, f)
		}
	} else {
		if len(m.Timeframes)*len(pairs)*settings != m.Combinations {
			return nil, fmt.Errorf("meta.json does not add up")
		}
		for _, mo := range m.Months {
			f, err := os.Open(filepath.Join(dir, fmt.Sprintf("month_%s.bin", mo)))
			if err != nil {
				return nil, err
			}
			s.files = append(s.files, f)
		}
		f, err := os.Open(filepath.Join(dir, "all.bin"))
		if err != nil {
			return nil, err
		}
		s.files = append(s.files, f)
	}
	return s, nil
}

func (s *scan) Close() {
	for _, f := range s.files {
		_ = f.Close()
	}
}

func (s *scan) decode(id uint32) (tf, pair, tp, sl, lev int) {
	settings := s.settings
	fileGroup := int(id) / settings
	setting := int(id) % settings
	group := fileGroup
	if s.layout == LayoutWindow && len(s.meta.SurvivorGroups) > 0 {
		group = s.meta.SurvivorGroups[fileGroup]
	}
	nSL := len(s.meta.SL)
	nLev := max(1, len(s.meta.Leverage))
	lev = setting % nLev
	tpsl := setting / nLev
	return group / len(s.pairs), group % len(s.pairs), tpsl / nSL, tpsl % nSL, lev
}

func round2(x float64) float64 {
	return math.Round(x*100) / 100
}

func (s *scan) windowStatsJSON(st score.WindowStats) map[string]any {
	sc, ok := st.Score()
	var scv any
	if ok {
		scv = math.Round(sc*1000) / 1000
	}
	return map[string]any{
		"profit":           round2(float64(st.Profit)),
		"drop":             round2(float64(st.Drop)),
		"worst":            round2(float64(st.Worst)),
		"hold_hours":       round2(float64(st.HoldHours)),
		"trades":           st.Trades,
		"wins":             st.Wins,
		"win_rate":         round2(100 * st.WinRate()),
		"trusted_win_rate": round2(100 * st.TrustedWinRate()),
		"score":            scv,
	}
}

func (s *scan) windowRowJSON(rank int, r windowRow) map[string]any {
	f, p, tp, sl, li := s.decode(r.id)
	i, j := s.pairs[p][0], s.pairs[p][1]
	v := s.windowStatsJSON(r.stats)
	v["rank"] = rank
	v["id"] = r.id
	v["tf"] = s.meta.Timeframes[f]
	v["fast"] = s.meta.Lengths[i]
	v["slow"] = s.meta.Lengths[j]
	v["tp"] = s.meta.TP[tp]
	v["sl"] = s.meta.SL[sl]
	v["leverage"] = s.meta.Leverage[li]
	return v
}

func (s *scan) monthRowJSON(rank int, r monthRow) map[string]any {
	f, p, tp, sl, li := s.decode(r.id)
	i, j := s.pairs[p][0], s.pairs[p][1]
	return map[string]any{
		"rank": rank, "id": r.id,
		"tf": s.meta.Timeframes[f], "fast": s.meta.Lengths[i], "slow": s.meta.Lengths[j],
		"tp": s.meta.TP[tp], "sl": s.meta.SL[sl], "leverage": s.meta.Leverage[li],
		"profit": round2(float64(r.profit)), "drop": round2(float64(r.drop)),
		"score": round2(float64(r.sc)), "trades": r.trades,
	}
}

func parseSort(sortStr string, ascending bool) []sortKey {
	parts := strings.Split(sortStr, ".")
	var keys []sortKey
	for _, p := range parts {
		if p == "" {
			continue
		}
		if i := strings.LastIndex(p, "-"); i > 0 {
			dir := p[i+1:]
			if dir == "asc" {
				keys = append(keys, sortKey{p[:i], true})
				continue
			}
			if dir == "desc" {
				keys = append(keys, sortKey{p[:i], false})
				continue
			}
		}
		keys = append(keys, sortKey{p, ascending})
	}
	if len(keys) == 0 {
		return []sortKey{{"score", ascending}}
	}
	return keys
}

func (s *scan) parseQuery(q url.Values) (filter, int, int, error) {
	f := filter{scored: true}
	if s.layout == LayoutMonth {
		f.window = len(s.meta.Months) // whole period
		f.scored = false
	}
	page, per := 0, 100
	sortStr, ascending := "", false
	indexF := func(values []float64, v string) (int, error) {
		x, err := strconv.ParseFloat(v, 64)
		if err != nil {
			return 0, err
		}
		for i, y := range values {
			if math.Abs(y-x) < 1e-9 {
				return i, nil
			}
		}
		return 0, fmt.Errorf("%s%% is not in the grid", v)
	}
	levIndex := func(values []uint32, v string) (int, error) {
		x, err := strconv.ParseUint(v, 10, 32)
		if err != nil {
			return 0, err
		}
		for i, y := range values {
			if uint64(y) == x {
				return i, nil
			}
		}
		return 0, fmt.Errorf("leverage %s is not in the grid", v)
	}
	get := func(k string) string { return q.Get(k) }
	if v := get("window"); v != "" && v != "any" {
		found := -1
		for i, w := range s.meta.Windows {
			if w.Start == v {
				found = i
				break
			}
		}
		if found < 0 {
			return f, 0, 0, fmt.Errorf("no window starting %s", v)
		}
		f.window = found
	}
	if v := get("period"); v != "" && v != "any" {
		if v == "all" {
			f.window = len(s.meta.Months)
		} else {
			found := -1
			for i, m := range s.meta.Months {
				if m == v {
					found = i
					break
				}
			}
			if found < 0 {
				return f, 0, 0, fmt.Errorf("no month %s", v)
			}
			f.window = found
		}
	}
	setInt := func(key string, dst **int) error {
		v := get(key)
		if v == "" || v == "any" {
			return nil
		}
		n, err := strconv.Atoi(v)
		if err != nil {
			return err
		}
		*dst = &n
		return nil
	}
	if err := setInt("tf", &f.tf); err != nil {
		return f, 0, 0, err
	}
	if err := setInt("fast", &f.fast); err != nil {
		return f, 0, 0, err
	}
	if err := setInt("slow", &f.slow); err != nil {
		return f, 0, 0, err
	}
	if v := get("tp"); v != "" && v != "any" {
		i, err := indexF(s.meta.TP, v)
		if err != nil {
			return f, 0, 0, err
		}
		f.tp = &i
	}
	if v := get("sl"); v != "" && v != "any" {
		i, err := indexF(s.meta.SL, v)
		if err != nil {
			return f, 0, 0, err
		}
		f.sl = &i
	}
	if v := get("leverage"); v != "" && v != "any" {
		i, err := levIndex(s.meta.Leverage, v)
		if err != nil {
			return f, 0, 0, err
		}
		f.lev = &i
	}
	if v := get("min_trades"); v != "" && v != "any" {
		n, err := strconv.ParseUint(v, 10, 32)
		if err != nil {
			return f, 0, 0, err
		}
		f.minTrades = uint32(n)
	}
	if v := get("scored"); v != "" {
		f.scored = v == "1"
	}
	if v := get("profitable"); v != "" {
		f.profitable = v == "1"
	}
	if v := get("sort"); v != "" {
		sortStr = v
	}
	if v := get("dir"); v == "asc" {
		ascending = true
	}
	if v := get("page"); v != "" {
		page, _ = strconv.Atoi(v)
	}
	if v := get("per"); v != "" {
		per, _ = strconv.Atoi(v)
		if per < 1 {
			per = 1
		}
		if per > 1000 {
			per = 1000
		}
	}
	f.sort = parseSort(sortStr, ascending)
	return f, page, per, nil
}

func filterEqual(a, b filter) bool {
	eqPtr := func(x, y *int) bool {
		if x == nil && y == nil {
			return true
		}
		if x == nil || y == nil {
			return false
		}
		return *x == *y
	}
	if a.window != b.window || a.minTrades != b.minTrades || a.scored != b.scored || a.profitable != b.profitable {
		return false
	}
	if !eqPtr(a.tf, b.tf) || !eqPtr(a.fast, b.fast) || !eqPtr(a.slow, b.slow) ||
		!eqPtr(a.tp, b.tp) || !eqPtr(a.sl, b.sl) || !eqPtr(a.lev, b.lev) {
		return false
	}
	if len(a.sort) != len(b.sort) {
		return false
	}
	for i := range a.sort {
		if a.sort[i] != b.sort[i] {
			return false
		}
	}
	return true
}

func (s *scan) groups(q filter) []int {
	var out []int
	for f := 0; f < len(s.meta.Timeframes); f++ {
		if q.tf != nil && s.meta.Timeframes[f] != *q.tf {
			continue
		}
		for p := 0; p < len(s.pairs); p++ {
			i, j := s.pairs[p][0], s.pairs[p][1]
			if q.fast != nil && s.meta.Lengths[i] != *q.fast {
				continue
			}
			if q.slow != nil && s.meta.Lengths[j] != *q.slow {
				continue
			}
			out = append(out, f*len(s.pairs)+p)
		}
	}
	// For staged window scans, file groups are survivor indices — map original groups to file indices.
	if s.layout == LayoutWindow && len(s.meta.SurvivorGroups) > 0 {
		want := map[int]struct{}{}
		for _, g := range out {
			want[g] = struct{}{}
		}
		mapped := make([]int, 0)
		for fi, g := range s.meta.SurvivorGroups {
			if _, ok := want[g]; ok {
				mapped = append(mapped, fi)
			}
		}
		return mapped
	}
	return out
}

func (s *scan) queryWindow(q filter) ([]windowRow, int, error) {
	groups := s.groups(q)
	nSL := len(s.meta.SL)
	nLev := max(1, len(s.meta.Leverage))
	nSLLev := nSL * nLev
	file := s.files[q.window]
	type part struct {
		rows    []windowRow
		matched int
	}
	var mu sync.Mutex
	var parts []part
	var wg sync.WaitGroup
	var next atomic.Int32
	workers := 4
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			buf := make([]byte, s.settings*windowRecord)
			var kept []windowRow
			matched := 0
			for {
				g := int(next.Add(1) - 1)
				if g >= len(groups) {
					break
				}
				group := groups[g]
				first := group * s.settings
				_, err := file.ReadAt(buf, int64(first*windowRecord))
				if err != nil && err != io.EOF {
					continue
				}
				for setting := 0; setting < s.settings; setting++ {
					if q.tp != nil && setting/nSLLev != *q.tp {
						continue
					}
					if q.sl != nil && (setting/nLev)%nSL != *q.sl {
						continue
					}
					if q.lev != nil && setting%nLev != *q.lev {
						continue
					}
					b := buf[setting*windowRecord : (setting+1)*windowRecord]
					st := score.FromBytes(b)
					sc, ok := st.Score()
					if !ok {
						sc = math.Inf(-1)
					}
					r := windowRow{id: uint32(first + setting), stats: st, sc: sc}
					if st.Trades == 0 || uint32(st.Trades) < q.minTrades {
						continue
					}
					if q.scored && math.IsInf(sc, -1) {
						continue
					}
					if q.profitable && st.Profit <= 0 {
						continue
					}
					matched++
					kept = append(kept, r)
					if len(kept) >= 2*maxRows {
						sortWindow(kept, q)
						kept = kept[:maxRows]
					}
				}
			}
			mu.Lock()
			parts = append(parts, part{kept, matched})
			mu.Unlock()
		}()
	}
	wg.Wait()
	matched := 0
	var rows []windowRow
	for _, p := range parts {
		matched += p.matched
		rows = append(rows, p.rows...)
	}
	if len(rows) > maxRows {
		sortWindow(rows, q)
		rows = rows[:maxRows]
	}
	sortWindow(rows, q)
	return rows, matched, nil
}

func sortWindow(rows []windowRow, q filter) {
	key := func(r windowRow, name string) float64 {
		st := r.stats
		switch name {
		case "profit":
			return float64(st.Profit)
		case "drop":
			return float64(st.Drop)
		case "trades":
			return float64(st.Trades)
		case "win_rate":
			return st.WinRate()
		case "trusted":
			return st.TrustedWinRate()
		case "worst":
			return float64(st.Worst)
		case "hold":
			return float64(st.HoldHours)
		case "tf", "fast", "slow", "tp", "sl", "leverage":
			// filled below via decode — handled by caller needing scan; use score fallback
			return r.sc
		default:
			return r.sc
		}
	}
	_ = key
	sort.SliceStable(rows, func(i, j int) bool {
		a, b := rows[i], rows[j]
		for _, sk := range q.sort {
			var ca, cb float64
			switch sk.name {
			case "profit":
				ca, cb = float64(a.stats.Profit), float64(b.stats.Profit)
			case "drop":
				ca, cb = float64(a.stats.Drop), float64(b.stats.Drop)
			case "trades":
				ca, cb = float64(a.stats.Trades), float64(b.stats.Trades)
			case "win_rate":
				ca, cb = a.stats.WinRate(), b.stats.WinRate()
			case "trusted":
				ca, cb = a.stats.TrustedWinRate(), b.stats.TrustedWinRate()
			case "worst":
				ca, cb = float64(a.stats.Worst), float64(b.stats.Worst)
			case "hold":
				ca, cb = float64(a.stats.HoldHours), float64(b.stats.HoldHours)
			default:
				ca, cb = a.sc, b.sc
			}
			if ca != cb {
				if sk.asc {
					return ca < cb
				}
				return ca > cb
			}
		}
		return a.id < b.id
	})
}

func (s *scan) queryMonth(q filter) ([]monthRow, int, error) {
	groups := s.groups(q)
	nSL := len(s.meta.SL)
	nLev := max(1, len(s.meta.Leverage))
	nSLLev := nSL * nLev
	file := s.files[q.window]
	type part struct {
		rows    []monthRow
		matched int
	}
	var mu sync.Mutex
	var parts []part
	var wg sync.WaitGroup
	var next atomic.Int32
	workers := 4
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			buf := make([]byte, s.settings*monthRecord)
			var kept []monthRow
			matched := 0
			for {
				g := int(next.Add(1) - 1)
				if g >= len(groups) {
					break
				}
				group := groups[g]
				first := group * s.settings
				_, err := file.ReadAt(buf, int64(first*monthRecord))
				if err != nil && err != io.EOF {
					continue
				}
				for setting := 0; setting < s.settings; setting++ {
					if q.tp != nil && setting/nSLLev != *q.tp {
						continue
					}
					if q.sl != nil && (setting/nLev)%nSL != *q.sl {
						continue
					}
					if q.lev != nil && setting%nLev != *q.lev {
						continue
					}
					b := buf[setting*monthRecord : (setting+1)*monthRecord]
					profit := math.Float32frombits(binary.LittleEndian.Uint32(b[0:4]))
					drop := math.Float32frombits(binary.LittleEndian.Uint32(b[4:8]))
					trades := binary.LittleEndian.Uint32(b[8:12])
					sc := float32(float64(profit) / math.Max(float64(drop), s.minDrop))
					r := monthRow{id: uint32(first + setting), profit: profit, drop: drop, trades: trades, sc: sc}
					if trades == 0 || trades < q.minTrades {
						continue
					}
					if q.profitable && profit <= 0 {
						continue
					}
					matched++
					kept = append(kept, r)
					if len(kept) >= 2*maxRows {
						sortMonth(kept, q)
						kept = kept[:maxRows]
					}
				}
			}
			mu.Lock()
			parts = append(parts, part{kept, matched})
			mu.Unlock()
		}()
	}
	wg.Wait()
	matched := 0
	var rows []monthRow
	for _, p := range parts {
		matched += p.matched
		rows = append(rows, p.rows...)
	}
	if len(rows) > maxRows {
		sortMonth(rows, q)
		rows = rows[:maxRows]
	}
	sortMonth(rows, q)
	return rows, matched, nil
}

func sortMonth(rows []monthRow, q filter) {
	sort.SliceStable(rows, func(i, j int) bool {
		a, b := rows[i], rows[j]
		for _, sk := range q.sort {
			var ca, cb float64
			switch sk.name {
			case "profit":
				ca, cb = float64(a.profit), float64(b.profit)
			case "drop":
				ca, cb = float64(a.drop), float64(b.drop)
			case "trades":
				ca, cb = float64(a.trades), float64(b.trades)
			default:
				ca, cb = float64(a.sc), float64(b.sc)
			}
			if ca != cb {
				if sk.asc {
					return ca < cb
				}
				return ca > cb
			}
		}
		return a.id < b.id
	})
}

// Serve starts the browse HTTP server (blocking).
func Serve(opts Options) error {
	if opts.Port == 0 {
		if opts.Layout == LayoutMonth {
			opts.Port = 8090
		} else {
			opts.Port = 8091
		}
	}
	if opts.MinDrop == 0 {
		opts.MinDrop = score.MinDrop
	}
	layout, err := DetectLayout(opts.ScanDir, opts.Layout)
	if err != nil {
		return err
	}
	s, err := openScan(opts.ScanDir, layout, opts.MinDrop)
	if err != nil {
		return err
	}
	defer s.Close()

	var (
		mu          sync.Mutex
		cacheFilter filter
		cacheW      []windowRow
		cacheM      []monthRow
		cacheMatch  int
		cacheValid  bool
	)

	pageHTML := func() (string, error) {
		name := "static/browse_window_scan.html"
		if layout == LayoutMonth {
			name = "static/browse_full_scan.html"
		}
		html, err := staticFS.ReadFile(name)
		if err != nil {
			return "", err
		}
		js, err := staticFS.ReadFile("static/table_sort.js")
		if err != nil {
			return "", err
		}
		return strings.Replace(string(html), "/*__TABLE_SORT__*/", string(js), 1), nil
	}

	writeJSON := func(w http.ResponseWriter, code int, v any) {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		w.WriteHeader(code)
		_ = json.NewEncoder(w).Encode(v)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		body, err := pageHTML()
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.Header().Set("Cache-Control", "no-store")
		_, _ = io.WriteString(w, body)
	})

	mux.HandleFunc("/api/meta", func(w http.ResponseWriter, r *http.Request) {
		m := s.meta
		if layout == LayoutWindow {
			wins := make([]map[string]string, 0, len(m.Windows))
			for _, win := range m.Windows {
				wins = append(wins, map[string]string{"start": win.Start, "end": win.End})
			}
			hours := m.Days * 24
			if m.Hours != nil {
				hours = *m.Hours
			}
			writeJSON(w, 200, map[string]any{
				"timeframes": m.Timeframes, "lengths": m.Lengths, "tp": m.TP, "sl": m.SL,
				"leverage": m.Leverage, "windows": wins, "days": m.Days, "hours": hours,
				"combinations": m.Combinations, "min_drop": score.MinDrop, "min_trades": score.MinTrades,
				"max_rows": maxRows, "layout": "window",
			})
			return
		}
		writeJSON(w, 200, map[string]any{
			"timeframes": m.Timeframes, "lengths": m.Lengths, "tp": m.TP, "sl": m.SL,
			"leverage": m.Leverage, "months": m.Months, "combinations": m.Combinations,
			"min_drop": s.minDrop, "max_rows": maxRows, "layout": "month",
		})
	})

	mux.HandleFunc("/api/reports", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, map[string]any{"reports": wfReportListing(s.dir)})
	})

	mux.HandleFunc("/report/", func(w http.ResponseWriter, r *http.Request) {
		name := strings.TrimPrefix(r.URL.Path, "/report/")
		allowed := false
		for _, stem := range wfReportStems {
			if name == stem+".html" || name == stem+"_fa.html" {
				allowed = true
				break
			}
		}
		if !allowed {
			http.Error(w, "unknown report", 400)
			return
		}
		path := filepath.Join(s.dir, name)
		b, err := os.ReadFile(path)
		if err != nil {
			http.Error(w, "missing "+path, 404)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		_, _ = w.Write(b)
	})

	resultsWindow := func(q filter) ([]windowRow, int, float64, error) {
		mu.Lock()
		defer mu.Unlock()
		if cacheValid && filterEqual(cacheFilter, q) && cacheW != nil {
			return cacheW, cacheMatch, 0, nil
		}
		start := time.Now()
		rows, matched, err := s.queryWindow(q)
		if err != nil {
			return nil, 0, 0, err
		}
		cacheFilter, cacheW, cacheM, cacheMatch, cacheValid = q, rows, nil, matched, true
		return rows, matched, time.Since(start).Seconds(), nil
	}
	resultsMonth := func(q filter) ([]monthRow, int, float64, error) {
		mu.Lock()
		defer mu.Unlock()
		if cacheValid && filterEqual(cacheFilter, q) && cacheM != nil {
			return cacheM, cacheMatch, 0, nil
		}
		start := time.Now()
		rows, matched, err := s.queryMonth(q)
		if err != nil {
			return nil, 0, 0, err
		}
		cacheFilter, cacheW, cacheM, cacheMatch, cacheValid = q, nil, rows, matched, true
		return rows, matched, time.Since(start).Seconds(), nil
	}

	mux.HandleFunc("/api/query", func(w http.ResponseWriter, r *http.Request) {
		q, page, per, err := s.parseQuery(r.URL.Query())
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": err.Error()})
			return
		}
		if layout == LayoutWindow {
			rows, matched, secs, err := resultsWindow(q)
			if err != nil {
				writeJSON(w, 400, map[string]string{"error": err.Error()})
				return
			}
			from := page * per
			if from > len(rows) {
				from = len(rows)
			}
			to := from + per
			if to > len(rows) {
				to = len(rows)
			}
			shown := make([]map[string]any, 0, to-from)
			for i, row := range rows[from:to] {
				shown = append(shown, s.windowRowJSON(from+i+1, row))
			}
			writeJSON(w, 200, map[string]any{
				"matched": matched, "kept": len(rows), "page": page, "per": per,
				"rows": shown, "seconds": secs,
			})
			return
		}
		rows, matched, secs, err := resultsMonth(q)
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": err.Error()})
			return
		}
		from := page * per
		if from > len(rows) {
			from = len(rows)
		}
		to := from + per
		if to > len(rows) {
			to = len(rows)
		}
		shown := make([]map[string]any, 0, to-from)
		for i, row := range rows[from:to] {
			shown = append(shown, s.monthRowJSON(from+i+1, row))
		}
		writeJSON(w, 200, map[string]any{
			"matched": matched, "kept": len(rows), "page": page, "per": per,
			"rows": shown, "seconds": secs,
		})
	})

	mux.HandleFunc("/api/csv", func(w http.ResponseWriter, r *http.Request) {
		q, _, _, err := s.parseQuery(r.URL.Query())
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": err.Error()})
			return
		}
		var buf bytes.Buffer
		if layout == LayoutWindow {
			rows, _, _, err := resultsWindow(q)
			if err != nil {
				writeJSON(w, 400, map[string]string{"error": err.Error()})
				return
			}
			buf.WriteString("rank,timeframe_min,rsi_fast,rsi_slow,tp_pct,sl_pct,leverage,profit_usdt,largest_drop_usdt,win_rate_pct,trusted_win_rate_pct,biggest_loss_usdt,avg_hold_hours,trades,wins,score\n")
			for k, row := range rows {
				f, p, tp, sl, li := s.decode(row.id)
				i, j := s.pairs[p][0], s.pairs[p][1]
				st := row.stats
				scStr := ""
				if sc, ok := st.Score(); ok {
					scStr = fmt.Sprintf("%.4f", sc)
				}
				fmt.Fprintf(&buf, "%d,%d,%d,%d,%g,%g,%d,%g,%g,%g,%g,%g,%g,%d,%d,%s\n",
					k+1, s.meta.Timeframes[f], s.meta.Lengths[i], s.meta.Lengths[j],
					s.meta.TP[tp], s.meta.SL[sl], s.meta.Leverage[li],
					round2(float64(st.Profit)), round2(float64(st.Drop)),
					round2(100*st.WinRate()), round2(100*st.TrustedWinRate()),
					round2(float64(st.Worst)), round2(float64(st.HoldHours)),
					st.Trades, st.Wins, scStr)
			}
		} else {
			rows, _, _, err := resultsMonth(q)
			if err != nil {
				writeJSON(w, 400, map[string]string{"error": err.Error()})
				return
			}
			buf.WriteString("rank,timeframe_min,rsi_fast,rsi_slow,tp_pct,sl_pct,leverage,profit_usdt,largest_drop_usdt,score,trades\n")
			for k, row := range rows {
				f, p, tp, sl, li := s.decode(row.id)
				i, j := s.pairs[p][0], s.pairs[p][1]
				fmt.Fprintf(&buf, "%d,%d,%d,%d,%g,%g,%d,%g,%g,%g,%d\n",
					k+1, s.meta.Timeframes[f], s.meta.Lengths[i], s.meta.Lengths[j],
					s.meta.TP[tp], s.meta.SL[sl], s.meta.Leverage[li],
					round2(float64(row.profit)), round2(float64(row.drop)),
					round2(float64(row.sc)), row.trades)
			}
		}
		w.Header().Set("Content-Type", "text/csv")
		w.Header().Set("Content-Disposition", `attachment; filename="combinations.csv"`)
		w.Header().Set("Cache-Control", "no-store")
		_, _ = w.Write(buf.Bytes())
	})

	mux.HandleFunc("/api/combo/", func(w http.ResponseWriter, r *http.Request) {
		idStr := strings.TrimPrefix(r.URL.Path, "/api/combo/")
		id64, err := strconv.ParseUint(idStr, 10, 32)
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": err.Error()})
			return
		}
		id := uint32(id64)
		if int(id) >= s.meta.Combinations {
			writeJSON(w, 400, map[string]string{"error": fmt.Sprintf("no combination %d", id)})
			return
		}
		if layout == LayoutWindow {
			windows := make([]map[string]any, 0, len(s.files))
			var last [windowRecord]byte
			for wi, f := range s.files {
				var b [windowRecord]byte
				if _, err := f.ReadAt(b[:], int64(id)*windowRecord); err != nil {
					writeJSON(w, 400, map[string]string{"error": err.Error()})
					return
				}
				last = b
				v := s.windowStatsJSON(score.FromBytes(b[:]))
				v["start"] = s.meta.Windows[wi].Start
				v["end"] = s.meta.Windows[wi].End
				windows = append(windows, v)
			}
			st := score.FromBytes(last[:])
			sc, ok := st.Score()
			if !ok {
				sc = math.Inf(-1)
			}
			writeJSON(w, 200, map[string]any{
				"combo":   s.windowRowJSON(0, windowRow{id: id, stats: st, sc: sc}),
				"windows": windows,
			})
			return
		}
		months := make([]map[string]any, 0, len(s.files))
		readMonth := func(period int) (monthRow, error) {
			var b [monthRecord]byte
			if _, err := s.files[period].ReadAt(b[:], int64(id)*monthRecord); err != nil {
				return monthRow{}, err
			}
			profit := math.Float32frombits(binary.LittleEndian.Uint32(b[0:4]))
			drop := math.Float32frombits(binary.LittleEndian.Uint32(b[4:8]))
			trades := binary.LittleEndian.Uint32(b[8:12])
			sc := float32(float64(profit) / math.Max(float64(drop), s.minDrop))
			return monthRow{id: id, profit: profit, drop: drop, trades: trades, sc: sc}, nil
		}
		for mi := 0; mi < len(s.files); mi++ {
			row, err := readMonth(mi)
			if err != nil {
				writeJSON(w, 400, map[string]string{"error": err.Error()})
				return
			}
			name := "whole period"
			if mi < len(s.meta.Months) {
				name = s.meta.Months[mi]
			}
			months = append(months, map[string]any{
				"period": name, "profit": round2(float64(row.profit)),
				"drop": round2(float64(row.drop)), "score": round2(float64(row.sc)), "trades": row.trades,
			})
		}
		whole, err := readMonth(len(s.meta.Months))
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": err.Error()})
			return
		}
		writeJSON(w, 200, map[string]any{"combo": s.monthRowJSON(0, whole), "months": months})
	})

	addr := fmt.Sprintf("127.0.0.1:%d", opts.Port)
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return fmt.Errorf("port %d is busy; try --port %d: %w", opts.Port, opts.Port+1, err)
	}
	fmt.Printf("Browse (%s): http://%s  (Ctrl+C to stop)\n", layout, addr)
	if layout == LayoutWindow && len(wfReportListing(s.dir)) > 0 {
		fmt.Printf("Walk-forward reports: http://%s/report/nm_walk_forward.html\n", addr)
	}
	return http.Serve(ln, mux)
}

func wfReportListing(dir string) []map[string]any {
	var out []map[string]any
	for _, stem := range wfReportStems {
		jsonPath := filepath.Join(dir, stem+".json")
		htmlPath := filepath.Join(dir, stem+".html")
		htmlFa := filepath.Join(dir, stem+"_fa.html")
		_, errJ := os.Stat(jsonPath)
		_, errH := os.Stat(htmlPath)
		if errJ != nil && errH != nil {
			continue
		}
		var total any
		policy := any(stem)
		if b, err := os.ReadFile(jsonPath); err == nil {
			var v map[string]any
			if json.Unmarshal(b, &v) == nil {
				if chosen, ok := v["chosen"].(map[string]any); ok {
					if t, ok := chosen["total"]; ok {
						total = t
					}
					if p, ok := chosen["short_oos_policy"]; ok {
						policy = p
					}
				}
				if meta, ok := v["meta"].(map[string]any); ok {
					if p, ok := meta["short_oos_policy"]; ok {
						policy = p
					}
				}
			}
		}
		_, errFa := os.Stat(htmlFa)
		out = append(out, map[string]any{
			"stem": stem, "policy": policy, "total": total,
			"html": errH == nil, "html_fa": errFa == nil, "json": errJ == nil,
			"href": "/report/" + stem + ".html", "href_fa": "/report/" + stem + "_fa.html",
		})
	}
	return out
}
