#!/usr/bin/env python3
"""Render docs/ARCHITECTURE.md to docs/ARCHITECTURE.pdf (simple text PDF)."""
from __future__ import annotations

import re
from pathlib import Path

from fpdf import FPDF

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "docs" / "ARCHITECTURE.md"
OUT = ROOT / "docs" / "ARCHITECTURE.pdf"


class DocPDF(FPDF):
    def footer(self) -> None:
        self.set_y(-12)
        self.set_font("Helvetica", size=8)
        self.set_text_color(100, 100, 100)
        self.cell(0, 8, f"Dynamic Trader architecture - page {self.page_no()}", align="C")


def clean(text: str) -> str:
    repl = {
        "\u2014": "-",
        "\u2013": "-",
        "\u2018": "'",
        "\u2019": "'",
        "\u201c": '"',
        "\u201d": '"',
        "\u2192": "->",
        "\u2190": "<-",
        "\u00d7": "x",
        "\u2502": "|",
        "\u2500": "-",
        "\u250c": "+",
        "\u2510": "+",
        "\u2514": "+",
        "\u2518": "+",
        "\u251c": "+",
        "\u2524": "+",
        "\u25bc": "v",
        "\u25b2": "^",
        "\u2022": "-",
        "\u2550": "=",
        "\u2026": "...",
        "\u2212": "-",
        "\u00a0": " ",
    }
    for a, b in repl.items():
        text = text.replace(a, b)
    text = text.replace("**", "").replace("__", "")
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
    return text.encode("latin-1", "replace").decode("latin-1")


def write_line(pdf: DocPDF, font: str, style: str, size: int, h: float, text: str) -> None:
    pdf.set_font(font, style, size)
    pdf.set_text_color(20, 20, 20)
    usable = max(pdf.epw, 1)
    text = clean(text)
    if not text.strip():
        pdf.ln(h)
        return
    width = int(max(40, usable / (size * 0.45)))
    for i in range(0, len(text), width):
        pdf.multi_cell(usable, h, text[i : i + width])


def main() -> None:
    lines = SRC.read_text(encoding="utf-8").splitlines()
    pdf = DocPDF(format="A4")
    pdf.set_auto_page_break(auto=True, margin=18)
    pdf.add_page()
    pdf.set_left_margin(16)
    pdf.set_right_margin(16)

    in_code = False
    for raw in lines:
        line = raw.rstrip()
        if line.startswith("```"):
            in_code = not in_code
            continue
        if not line.strip():
            pdf.ln(3)
            continue
        stripped = line.replace("|", " ").strip()
        if set(stripped) <= set("-: ") and len(stripped) >= 3:
            continue
        if in_code:
            write_line(pdf, "Courier", "", 7, 3.6, line)
            continue
        if line.startswith("# "):
            write_line(pdf, "Helvetica", "B", 16, 8, line[2:])
            pdf.ln(2)
        elif line.startswith("## "):
            pdf.ln(2)
            write_line(pdf, "Helvetica", "B", 12, 6.5, line[3:])
            pdf.ln(1)
        elif line.startswith("### "):
            write_line(pdf, "Helvetica", "B", 10, 5.5, line[4:])
        elif line.startswith("|"):
            cells = [c.strip() for c in line.strip("|").split("|")]
            write_line(pdf, "Helvetica", "", 8, 4.5, " | ".join(cells))
        else:
            write_line(pdf, "Helvetica", "", 10, 5, line)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    pdf.output(str(OUT))
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
