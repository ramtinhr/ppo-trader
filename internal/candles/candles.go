// Package candles handles OHLC bars, M1C1 disk cache, and OKX archive aggregation.
package candles

import (
	"archive/zip"
	"bufio"
	"encoding/binary"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/ramtin/dynamic-trader/internal/logx"
)

const Instrument = "BTC-USDT-SWAP"

var voided = map[uint64]struct{}{
	2_818_465_146: {},
	2_818_465_147: {},
	2_818_465_148: {},
}

type Candle struct {
	TimeMs int64
	Open   float64
	High   float64
	Low    float64
	Close  float64
}

// Aggregate 1m (or finer) candles into bar_ms buckets.
func Aggregate(minutes []Candle, barMs int64) []Candle {
	if len(minutes) == 0 || barMs <= 0 {
		return nil
	}
	var out []Candle
	var cur *Candle
	bucket := int64(math.MinInt64)
	for _, c := range minutes {
		b := floorDiv(c.TimeMs, barMs) * barMs
		if cur != nil && b == bucket {
			cur.High = math.Max(cur.High, c.High)
			cur.Low = math.Min(cur.Low, c.Low)
			cur.Close = c.Close
			continue
		}
		if cur != nil {
			out = append(out, *cur)
		}
		bucket = b
		cc := c
		cc.TimeMs = b
		cur = &cc
	}
	if cur != nil {
		out = append(out, *cur)
	}
	return out
}

func floorDiv(a, b int64) int64 {
	q := a / b
	if (a^b) < 0 && a%b != 0 {
		q--
	}
	return q
}

func CachePath(scan string, fromMs, toMs int64) string {
	return filepath.Join(scan, ".cache", fmt.Sprintf("minutes_1m_%d_%d.bin", fromMs, toMs))
}

func WriteCache(path string, fromMs, toMs int64, cs []Candle) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	if _, err := f.Write([]byte("M1C1")); err != nil {
		return err
	}
	w := func(v uint64) error { return binary.Write(f, binary.LittleEndian, v) }
	if err := binary.Write(f, binary.LittleEndian, fromMs); err != nil {
		return err
	}
	if err := binary.Write(f, binary.LittleEndian, toMs); err != nil {
		return err
	}
	if err := w(uint64(len(cs))); err != nil {
		return err
	}
	for _, c := range cs {
		if err := binary.Write(f, binary.LittleEndian, c.TimeMs); err != nil {
			return err
		}
		for _, x := range []float64{c.Open, c.High, c.Low, c.Close} {
			if err := w(math.Float64bits(x)); err != nil {
				return err
			}
		}
	}
	return nil
}

func ReadCache(path string, fromMs, toMs int64) ([]Candle, bool, error) {
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, false, nil
		}
		return nil, false, err
	}
	defer f.Close()
	mag := make([]byte, 4)
	if _, err := io.ReadFull(f, mag); err != nil {
		return nil, false, err
	}
	if string(mag) != "M1C1" {
		return nil, false, nil
	}
	var cachedFrom, cachedTo int64
	var n uint64
	if err := binary.Read(f, binary.LittleEndian, &cachedFrom); err != nil {
		return nil, false, err
	}
	if err := binary.Read(f, binary.LittleEndian, &cachedTo); err != nil {
		return nil, false, err
	}
	if cachedFrom != fromMs || cachedTo != toMs {
		return nil, false, nil
	}
	if err := binary.Read(f, binary.LittleEndian, &n); err != nil {
		return nil, false, err
	}
	out := make([]Candle, 0, n)
	for i := uint64(0); i < n; i++ {
		var c Candle
		var bits uint64
		if err := binary.Read(f, binary.LittleEndian, &c.TimeMs); err != nil {
			return nil, false, err
		}
		for _, dst := range []*float64{&c.Open, &c.High, &c.Low, &c.Close} {
			if err := binary.Read(f, binary.LittleEndian, &bits); err != nil {
				return nil, false, err
			}
			*dst = math.Float64frombits(bits)
		}
		out = append(out, c)
	}
	return out, true, nil
}

// LoadMinutes returns 1m candles, using M1C1 cache when present.
func LoadMinutes(dataDir, scanDir string, fromMs, toMs int64) ([]Candle, error) {
	cache := CachePath(scanDir, fromMs, toMs)
	if cs, ok, err := ReadCache(cache, fromMs, toMs); err != nil {
		return nil, err
	} else if ok {
		st, _ := os.Stat(cache)
		mb := 0.0
		if st != nil {
			mb = float64(st.Size()) / 1e6
		}
		logx.Infof("loaded %d 1m candles from cache %s (%.1f MB)", len(cs), cache, mb)
		return cs, nil
	}
	logx.Infof("building 1m candles from archives %s → %s …", UTC(fromMs), UTC(toMs))
	t0 := time.Now()
	cs, err := FromArchives(dataDir, 60_000, fromMs, toMs)
	if err != nil {
		return nil, err
	}
	if err := WriteCache(cache, fromMs, toMs, cs); err != nil {
		return nil, err
	}
	logx.Infof("%d candles in %.0fs → wrote %s", len(cs), time.Since(t0).Seconds(), cache)
	return cs, nil
}

func UTC(ms int64) string {
	return time.UnixMilli(ms).UTC().Format("2006-01-02 15:04")
}

func DayMs(day string) (int64, error) {
	t, err := time.ParseInLocation("2006-01-02", day, time.UTC)
	if err != nil {
		return 0, err
	}
	return t.UnixMilli(), nil
}

func ParseWindowMs(s string) (int64, error) {
	// window_scan / hand-written meta use several shapes
	layouts := []string{
		"2006-01-02 15:04:05",
		"2006-01-02 15:04",
		"2006-01-02T15:04:05Z",
		"2006-01-02T15:04:05",
		"2006-01-02T15:04:05.000Z",
		"2006-01-02",
	}
	for _, l := range layouts {
		if t, err := time.ParseInLocation(l, s, time.UTC); err == nil {
			return t.UnixMilli(), nil
		}
	}
	return 0, fmt.Errorf("bad window time %q", s)
}

// FromArchives builds candles from verified OKX trade zips (same rules as Rust okx_history::candles).
func FromArchives(dir string, barMs, fromMs, toMs int64) ([]Candle, error) {
	first := utc8Day(fromMs)
	last := utc8Day(toMs - 1)
	var out []Candle
	for d := first; !d.After(last); d = d.AddDate(0, 0, 1) {
		day := d.Format("2006-01-02")
		zipPath := filepath.Join(dir, fmt.Sprintf("%s-trades-%s.zip", Instrument, day))
		sumPath := filepath.Join(dir, fmt.Sprintf("%s-trades-%s.verified.json", Instrument, day))
		if _, err := os.Stat(sumPath); err != nil {
			return nil, fmt.Errorf("%s missing or unverified; run download first", zipPath)
		}
		if err := forEachTrade(zipPath, func(t trade) error {
			if t.timeMs < fromMs || t.timeMs >= toMs {
				return nil
			}
			openMs := t.timeMs - modEuclid(t.timeMs, barMs)
			if n := len(out); n > 0 {
				c := &out[n-1]
				if c.TimeMs == openMs {
					c.High = math.Max(c.High, t.price)
					c.Low = math.Min(c.Low, t.price)
					c.Close = t.price
					return nil
				}
				if c.TimeMs > openMs {
					return fmt.Errorf("trade %d out of order", t.id)
				}
			}
			out = append(out, Candle{TimeMs: openMs, Open: t.price, High: t.price, Low: t.price, Close: t.price})
			return nil
		}); err != nil {
			return nil, err
		}
	}
	return out, nil
}

type trade struct {
	id     uint64
	price  float64
	timeMs int64
}

func forEachTrade(path string, f func(trade) error) error {
	zr, err := zip.OpenReader(path)
	if err != nil {
		return err
	}
	defer zr.Close()
	if len(zr.File) == 0 {
		return fmt.Errorf("empty zip %s", path)
	}
	rc, err := zr.File[0].Open()
	if err != nil {
		return err
	}
	defer rc.Close()
	sc := bufio.NewScanner(rc)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	lineNo := 0
	for sc.Scan() {
		lineNo++
		if lineNo == 1 {
			continue
		}
		t, err := parseLine(sc.Text())
		if err != nil {
			return fmt.Errorf("%s line %d: %w", path, lineNo, err)
		}
		if _, skip := voided[t.id]; skip {
			continue
		}
		if err := f(t); err != nil {
			return err
		}
	}
	return sc.Err()
}

func parseLine(line string) (trade, error) {
	parts := strings.Split(line, ",")
	if len(parts) < 6 {
		return trade{}, fmt.Errorf("missing columns")
	}
	if parts[0] != Instrument {
		return trade{}, fmt.Errorf("unexpected instrument")
	}
	id, err := strconv.ParseUint(parts[1], 10, 64)
	if err != nil {
		return trade{}, err
	}
	price, err := strconv.ParseFloat(parts[3], 64)
	if err != nil {
		return trade{}, err
	}
	tm, err := strconv.ParseInt(parts[5], 10, 64)
	if err != nil {
		return trade{}, err
	}
	return trade{id: id, price: price, timeMs: tm}, nil
}

func utc8Day(timeMs int64) time.Time {
	return time.UnixMilli(timeMs + 8*3_600_000).UTC().Truncate(24 * time.Hour)
}

func modEuclid(a, b int64) int64 {
	m := a % b
	if m < 0 {
		m += b
	}
	return m
}
