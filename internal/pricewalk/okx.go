package pricewalk

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"time"

	"github.com/ramtin/dynamic-trader/internal/candles"
	"github.com/ramtin/dynamic-trader/internal/logx"
)

const candleURL = "https://www.okx.com/api/v5/market/history-candles"

// OfficialCandles fetches OKX history candles. Soft-fails (nil, err) when the
// network is blocked — callers treat that as empty actuals.
func OfficialCandles(barMs, fromMs, toMs int64) ([]candles.Candle, error) {
	if toMs < fromMs {
		return nil, nil
	}
	minutes := barMs / 60_000
	var bar string
	switch minutes {
	case 1, 3, 5, 15, 30:
		bar = fmt.Sprintf("%dm", minutes)
	case 60, 120, 240:
		bar = fmt.Sprintf("%dH", minutes/60)
	default:
		return nil, fmt.Errorf("OKX publishes no %d-minute candles", minutes)
	}

	client := &http.Client{Timeout: 30 * time.Second}
	var out []candles.Candle
	after := toMs + barMs + 1
	for after > fromMs {
		var reply struct {
			Code string     `json:"code"`
			Data [][]string `json:"data"`
		}
		var lastErr error
		for attempt := 1; attempt <= 5; attempt++ {
			u, _ := url.Parse(candleURL)
			q := u.Query()
			q.Set("instId", candles.Instrument)
			q.Set("bar", bar)
			q.Set("after", strconv.FormatInt(after, 10))
			q.Set("limit", "100")
			u.RawQuery = q.Encode()
			resp, err := client.Get(u.String())
			if err != nil {
				lastErr = err
				time.Sleep(time.Duration(attempt) * time.Second)
				continue
			}
			body, err := io.ReadAll(resp.Body)
			resp.Body.Close()
			if err != nil {
				lastErr = err
				time.Sleep(time.Duration(attempt) * time.Second)
				continue
			}
			if err := json.Unmarshal(body, &reply); err != nil {
				lastErr = err
				time.Sleep(time.Duration(attempt) * time.Second)
				continue
			}
			if reply.Code == "0" {
				lastErr = nil
				break
			}
			lastErr = fmt.Errorf("OKX candle request failed: code=%s", reply.Code)
			if attempt >= 5 {
				return nil, lastErr
			}
			time.Sleep(time.Duration(attempt) * time.Second)
		}
		if lastErr != nil {
			return nil, lastErr
		}
		if len(reply.Data) == 0 {
			break
		}
		oldest := after
		for _, row := range reply.Data {
			c, err := parseOKXCandle(row)
			if err != nil {
				return nil, err
			}
			if c.TimeMs < oldest {
				oldest = c.TimeMs
			}
			if c.TimeMs >= fromMs && c.TimeMs <= toMs {
				out = append(out, c)
			}
		}
		if len(reply.Data) < 100 || oldest <= fromMs {
			break
		}
		after = oldest
	}
	// Sort + dedup by time.
	if len(out) > 1 {
		sortCandles(out)
		j := 0
		for i := 1; i < len(out); i++ {
			if out[i].TimeMs != out[j].TimeMs {
				j++
				out[j] = out[i]
			}
		}
		out = out[:j+1]
	}
	return out, nil
}

func parseOKXCandle(row []string) (candles.Candle, error) {
	if len(row) < 5 {
		return candles.Candle{}, fmt.Errorf("short candle row")
	}
	tm, err := strconv.ParseInt(row[0], 10, 64)
	if err != nil {
		return candles.Candle{}, err
	}
	f := func(i int) (float64, error) { return strconv.ParseFloat(row[i], 64) }
	o, e1 := f(1)
	h, e2 := f(2)
	l, e3 := f(3)
	c, e4 := f(4)
	if e1 != nil || e2 != nil || e3 != nil || e4 != nil {
		return candles.Candle{}, fmt.Errorf("candle field")
	}
	return candles.Candle{TimeMs: tm, Open: o, High: h, Low: l, Close: c}, nil
}

func sortCandles(cs []candles.Candle) {
	for i := 1; i < len(cs); i++ {
		for j := i; j > 0 && cs[j].TimeMs < cs[j-1].TimeMs; j-- {
			cs[j], cs[j-1] = cs[j-1], cs[j]
		}
	}
}

// softOfficialCandles never fails the run — logs and returns empty on network errors.
func softOfficialCandles(barMs, fromMs, toMs int64) []candles.Candle {
	cs, err := OfficialCandles(barMs, fromMs, toMs)
	if err != nil {
		logx.Warnf("    OKX actuals unavailable (%v)", err)
		return nil
	}
	if len(cs) > 0 {
		logx.Infof("    OKX actuals for forward-until-1%%: %d candles %s → %s",
			len(cs), utc(fromMs), utc(toMs))
	}
	return cs
}
