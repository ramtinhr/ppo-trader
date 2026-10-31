#!/usr/bin/env bash
# Build docs/ARCHITECTURE.pdf from docs/architecture.html (WeasyPrint).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 - <<'PY'
from pathlib import Path
src = Path("docs/architecture.html").read_text(encoding="utf-8")
# Drop remote @import so PDF generation does not hang offline / in CI.
out = []
skip = False
for line in src.splitlines(True):
    if "@import url(" in line and "fonts.googleapis.com" in line:
        continue
    out.append(line)
Path("docs/architecture.print.html").write_text("".join(out), encoding="utf-8")
print("wrote docs/architecture.print.html")
PY
if command -v weasyprint >/dev/null 2>&1; then
  WP=weasyprint
elif [[ -x "$HOME/.local/bin/weasyprint" ]]; then
  WP="$HOME/.local/bin/weasyprint"
else
  echo "install weasyprint: pip install --user weasyprint" >&2
  exit 1
fi
"$WP" docs/architecture.print.html docs/ARCHITECTURE.pdf
ls -la docs/ARCHITECTURE.pdf
echo "Tip: for best on-screen reading open docs/architecture.html in a browser."
