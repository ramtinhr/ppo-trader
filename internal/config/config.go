// Package config loads paper-trading settings from TOML.
package config

import (
	"fmt"
	"os"

	"github.com/pelletier/go-toml/v2"
)

type App struct {
	Risk   Risk   `toml:"risk"`
	Venues Venues `toml:"venues"`
}

type Risk struct {
	PaperOnly   bool `toml:"paper_only"`
	MaxLeverage int  `toml:"max_leverage"`
}

type Venues struct {
	OKX OKX `toml:"okx"`
}

type OKX struct {
	MakerFeeBps float64 `toml:"maker_fee_bps"`
	TakerFeeBps float64 `toml:"taker_fee_bps"`
}

// Load reads path and rejects live trading.
func Load(path string) (*App, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var cfg App
	if err := toml.Unmarshal(b, &cfg); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	if !cfg.Risk.PaperOnly {
		return nil, fmt.Errorf("%s: risk.paper_only must be true", path)
	}
	if cfg.Risk.MaxLeverage < 1 || cfg.Risk.MaxLeverage > 10 {
		return nil, fmt.Errorf("%s: risk.max_leverage out of range", path)
	}
	return &cfg, nil
}

func (c *App) Taker() float64 { return c.Venues.OKX.TakerFeeBps / 10_000 }
func (c *App) Maker() float64 { return c.Venues.OKX.MakerFeeBps / 10_000 }
