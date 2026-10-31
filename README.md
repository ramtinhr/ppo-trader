# Dynamic Trader

Rust research and backtesting for perpetual futures with NautilusTrader `0.65.0`. The market is the
OKX BTC-USDT-SWAP perpetual (linear, settled in USDT). Strategy signals use one-hour bars built from
every recorded trade.

## Safety

- Paper trading only: configuration validation rejects `paper_only = false`.
- Leverage is capped at 3x by configuration validation.
- Credentials come only from `.env`, which git ignores. Nothing secret is stored in source code.
- Future testnet execution must use a trade-only key with no withdrawal permission. Never use a
  master key or seed phrase.

## Every OKX trade

Downloads OKX's free daily trade archives for BTC-USDT-SWAP into
`data/okx/BTC-USDT-SWAP/archives/`. No API key is needed. An OKX archive day runs from 16:00 UTC on
the previous day to 16:00 UTC, the same window as OKX's official daily candle. Each archive is
checked before it is marked verified:

- every trade lies inside the archive's window;
- trade IDs have no gaps, both within the archive and with the previous archive;
- the summed contracts equal OKX's official daily candle volume exactly.

```bash
# The archives covering the previous calendar month (UTC)
cargo run --release --bin download
# or: dt download

# Custom UTC range: START inclusive, END exclusive
cargo run --release --bin download -- 2026-09-01 2026-10-01
```

Verified days are skipped, so an interrupted run resumes when rerun.

OKX's archive for 2026-08-01 holds three trades (IDs 2818465146 to 2818465148, 5.78 contracts) that
OKX's own trade API and candles leave out: OKX voided them after the fact. They are listed in
`VOIDED` in `src/okx_history.rs` and skipped everywhere, so that day matches OKX's candle exactly.

OKX's archive for 2026-01-14 is the one accepted exception: it lacks the trades behind two ID gaps,
2105958516 to 2105979022 and 2106031988 to 2106045535, all within 2026-01-13 22:10 UTC. They add up
to 44,527.47 contracts (archive 10,395,636.17 against OKX's 1D candle 10,440,163.64), all in that
minute's 1m candle, whose high (96777) and low (94655.1) still match OKX's, so prices are complete
and only volume is short. OKX's trade API no longer returns trades that old, so they cannot be
recovered. `KNOWN_GAPS` in `src/okx_history.rs` allows exactly these two jumps on that day, and only
when the archive is short by exactly that amount.

## Backtest

Replays the verified trades through NautilusTrader's backtest engine, one archive at a time. The
strategy builds 1-hour bars from the trades exactly like TradingView. A signal on a bar's close fills
completely at the next bar's open (the first trade of the next bar), with OKX's regular-user fees
from `config/testnet.toml` (2 bps maker, 5 bps taker; market orders pay taker). Sizes are in BTC, in steps
of 0.0001 BTC (one OKX contract is 0.01 BTC, and the minimum order is 0.01 contracts).

```bash
cargo run --release --bin backtest                     # RSI(14) on the previous calendar month
cargo run --release --bin backtest -- --start 2026-09-01 --end 2026-10-01 --rsi-period 14,50
cargo run --release --bin backtest -- --size-btc 0.005 --balance-usdt 500
cargo run --release --bin backtest -- --help           # all options
```

`PineRsiStrategy` is a Rust port of TradingView's built-in "RSI Strategy": long when RSI crosses up
through `--oversold` (30), short when it crosses down through `--overbought` (70). Each signal
reverses the position. Several comma-separated `--rsi-period` values run one backtest each and print
a comparison table. Not yet modelled: the bid/ask spread (the data has trades only), funding
payments, and slippage beyond the trade price.

## RSI cross combos

`rsi_combos` tests every pair of RSI lengths, fast < slow. It goes long when the fast RSI crosses up
through the slow RSI and short when it crosses down. Each signal reverses the position and fills at
the next bar's open with the taker fee. The default lengths are 1 to 50, then 60 to 400 in steps of
10: 85 lengths, 3,570 pairs. Swapping a pair gives exactly the opposite trades, so only fast < slow
is tested.

Every pair is tested with every filter and every take-profit / stop-loss setting.

**Filters** decide whether a cross may open a position. A cross a filter rejects still closes the
position it is against; it only opens nothing.

- Trend (`--trend`, default none, 50, 100, 200): a long only while the close is above its EMA of
  that many bars, a short only while it is below.
- Volatility (`--atr`, default none, 0.2, 0.4, 0.8): only while the ATR(14) is at least that % of
  the close, so moves are big enough to pay the fees.

That is 16 filters. EMA and ATR follow TradingView's `ta.ema` and `ta.atr`, both starting from a
simple average.

**TP/SL settings:**

- one stop-loss for the whole position, a distance in % of the entry price (`--stops`, default
  0.5, 1, 2, 3);
- take-profits at multiples (R) of the stop distance (`--ladders`, default 1/2/3 and 2/4/6), each
  closing its share of the position (`--shares`, default 50/25/25 %). Together they close 100% of
  the position;
- optionally, the stop moves to the entry price (break-even) once the first take-profit fills
  (`--breakeven yes|no|both`, default both).

That is 16 settings plus "no TP/SL". With 16 filters, each pair gets 272 settings, about 971,000
combinations. Take-profits are limit orders and pay the maker fee; the stop and the signals are
market orders and pay the taker fee. After a take-profit or stop closes the whole position, it stays
flat until the next cross. A cross while a position is open cancels its exits and reverses (or,
when filtered, only closes) the position.

**Training and test.** With this many combinations, some win by luck. So the period is split in two
(`--test-from`, default two thirds of the way in). Combinations are ranked on the training part
only, then each one is also run on the test part, starting flat with a fresh balance. The test part
plays no role in picking, so it shows whether a result holds up on months the search never saw. The
indicators carry on across the split, as they would in live trading.

```bash
# all the data we have: 2026-03-16 to 2026-10-10, test from 2026-08-01
cargo run --release --bin rsi_combos -- --start 2026-03-16 --end 2026-10-10 --test-from 2026-08-01 --timeframe 60
cargo run --release --bin rsi_combos -- --trend none --atr none --stops none   # no filters, no TP/SL
cargo run --release --bin rsi_combos -- --help                                 # all options
```

A fast replay checks the exits against every trade price. The best combinations (`--verify`,
default 1) are then rerun through NautilusTrader on both parts, and the run fails unless every fill
(time, kind, side, size, price, fee) and the PnL agree exactly. Picked first are the best in
training that also made money in the test part. The NautilusTrader test run starts on its first day
with the indicators warmed up on the training candles. Once the cash balance drops below zero,
orders that open a position are refused, as Nautilus refuses them. Orders that only close are
refused too while the position's exits are still being cancelled. Every combination goes to
`data/combos/rsi_cross_filters_<timeframe>m_<start>_<end>.csv`, best in training first.

The run also writes `data/combos/rsi_cross_filters_<timeframe>m_<start>_<end>.html`. Open it in a
browser; the charts load from the internet. It has:

- how many combinations made money in training, and how many of those also made money in the
  test part;
- a heatmap of every pair, in training or in the test part, with its best setting or any one;
- a table of the 272 settings: how many pairs each one makes profitable in training and in both
  parts, its best pair and medians;
- a sortable, filterable table of every pair with its best setting in training and that setting's
  test result;
- for the best `--report` combinations (default 50): either part, with price, entries and exits,
  both RSIs, balance before and after fees, and every trade with each exit (take-profit,
  stop-loss, break-even or signal), its fees and its profit after fees.

To watch a run live, add `--serve` (default port 8080, or `--serve 9000`) and open
`http://127.0.0.1:8080`:

```bash
cargo run --release --bin rsi_combos -- --start 2026-09-01 --end 2026-10-01 --timeframe 1 --serve
```

The page shows the current stage, a progress bar, the run log and each NautilusTrader check as it
finishes. The results appear as soon as the scan is done, and every pair can be opened with all its
trades and fees with any setting, not only the best `--report` combinations. The server keeps running after the run
finishes; press Ctrl+C to stop it.

## Walk-forward test

One training/test split can still be lucky. `walk_forward` repeats the choice every month, as a
trader would. The period is cut into calendar months. For each month after the training window:

1. Every `rsi_combos` combination (pair × filter × TP/SL setting, about 971,000) is run on the
   training window, the `--train-months` before that month (default 2, 4 and 6, each tested on
   its own).
2. Combinations with fewer than `--min-trades` closed trades (default 10), or with refused
   orders, are skipped. The one with the highest profit after fees is the pick. The `--top-k`
   best (default 10) are kept too, to see whether the winner was just lucky.
3. The picks are traded on the month itself, which played no part in choosing them. Each month
   starts flat with the full balance, with every indicator warmed up on all earlier candles. A
   position still open at the month's end counts as closed at the last price with the taker fee.

Adding up these unseen months gives the walk-forward profit. Every month is compared with buying
the same BTC at the month's first price and selling at its last, with both fees. Fixed candidates
(`--candidates fast/slow/trend/atr`; defaults are the earlier 4h and 1h candidates) are also
traded on every month. Months before 2026-03-16 are marked "unseen": the earlier `rsi_combos` runs
never saw them.

The whole period's trade tape is loaded once; a year takes about 3 GB of memory. The pick of the
last `--verify` windows (default 1) of the first training length is rerun through NautilusTrader on
its test month and on its training window. The run fails unless every fill and the PnL agree.

```bash
# a year of archives first
cargo run --release --bin download_okx_trades -- 2025-10-01 2026-10-01
cargo run --release --bin walk_forward -- --timeframe 240   # 4h
cargo run --release --bin walk_forward -- --timeframe 60    # 1h
cargo run --release --bin walk_forward -- --help            # all options
```

The run writes `data/walkforward/walk_forward_<timeframe>m_<start>_<end>.html`, a page that works
offline, and the same data as `.json`. The page has the verdict in plain words, the result per
training length, a chart and a table month by month (the pick, its training and test profit,
the top-k average, buy-and-hold), the candidates month by month, and the NautilusTrader check.

## Check against TradingView

`tv_check` compares the TradingView chart `OKX:BTCUSDT.P` with our data and code. The default limit
is 0.001%. It needs a free TradingView login, because TradingView refuses indicators to anonymous
users. Put the `sessionid` and `sessionid_sign` cookies from tradingview.com in `.env` (see
`.env.example`). The checker runs three checks:

1. **Candles.** TradingView's candles are compared with ours, which are built from every OKX trade.
   When they differ, OKX's official candle decides which one is wrong.
2. **RSI.** TradingView's RSI is compared with our `Rsi` for lengths 10, 50, 100, 150 and 200. Each
   RSI is checked twice: on TradingView's own candles, which tests the formula, and on our candles,
   which tests the data. Differences on our candles pass only when OKX's official candles prove that
   TradingView's candles were wrong.
3. **Trades.** TradingView's RSI Strategy is compared with our port for the same RSI lengths, with
   oversold 30 and overbought 70: entry time, side and fill price. As with RSI, the check runs on
   TradingView's own candles and on ours.

```bash
cargo run --release --bin tv_check -- --timeframe 3 --out data/tradingview/check_3m
cargo run --release --bin tv_check -- --timeframe 1 --out data/tradingview/check_1m
```

TradingView's free plan loads about 9,000 one-minute or 6,400 three-minute bars. Only the part
covered by downloaded archives is compared. Download the recent days first. CSV reports for every
bar and trade go to `--out`.

## Cost model

Configured fees are per side, in basis points. The round-trip estimate is:

```text
2 × trading fee + spread + 2 × slippage + absolute funding
```

## Product CLI (`dt`)

Go owns the product layer (browse HTTP, N/M score walk-forward, HTML reports). Rust remains the
engine (`download`, `scan`, `simulate`, `verify`, …). See [docs/DT.md](docs/DT.md).

```bash
./scripts/build_dt.sh   # macOS: needs CGO / external linker (see docs/DT.md)
./dt browse --scan data/windowscan/… --port 8091
./dt walk-forward --scan data/windowscan/… --require-fit 0
./dt report --json data/windowscan/…/nm_walk_forward.json
```

Download two years of OKX trades (no Go needed):

```bash
cargo run --release --bin download -- 2024-10-31 2026-10-31
```

## Next steps

1. Model funding, spread, and slippage.
2. Run OKX demo trading before enabling any live configuration.
