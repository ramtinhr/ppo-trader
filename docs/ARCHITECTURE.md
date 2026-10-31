# Dynamic Trader — Architecture Guide

**Start here (best reading experience):**

| Format | Path |
|--------|------|
| Interactive HTML | [architecture.html](architecture.html) — open in a browser |
| Printable PDF | [ARCHITECTURE.pdf](ARCHITECTURE.pdf) — regenerates from the HTML |
| CLI cheat sheet | [DT.md](DT.md) |

Regenerate the PDF after editing the HTML:

```bash
# strip remote fonts for offline PDF, then:
python3 - <<'PY'
from pathlib import Path
src = Path('docs/architecture.html').read_text()
# optional: keep as-is if fonts are cached
Path('docs/architecture.print.html').write_text(src)
PY
~/.local/bin/weasyprint docs/architecture.print.html docs/ARCHITECTURE.pdf
# or open architecture.html → Print → Save as PDF
```

## What the guide covers

1. Mental model (Rust engine vs Go product)
2. Full repository map
3. End-to-end data flow (OKX → tape → scan → walk-forward → reports)
4. Every Rust library module (purpose + size)
5. Every remaining binary
6. On-disk formats (archives, 20-byte vs 12-byte records, meta.json)
7. `simulate` JSONL contract
8. Go packages + engine discovery (including `pricewalk` / `pricemodel` / `candles`)
9. Feature `verify` / Nautilus boundary
10. Config & safety
11. Typical workflow commands
12. Cleanup removals vs replacements
13. Where to edit what

**Price path** is pure Go (`dt price-walk`); the former Rust `window_nm_walk_forward` binary and
`price_model.rs` are deleted. See [DT.md](DT.md) and `scripts/price_iter.sh`.

The old plain-text PDF generator (`scripts/md_to_pdf.py`) is obsolete for the main guide; keep the HTML as the source of truth.
