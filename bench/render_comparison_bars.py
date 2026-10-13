#!/usr/bin/env python3
"""Render zccache-style dark README comparison charts.

Reads:  ./benchmark-output/comparison.json
Writes: ./benchmark-stats/benchmark-rust-only.jpg
        ./benchmark-stats/benchmark-rust-c.jpg
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from benchmark_models import ComparisonDocument, ComparisonResult

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError as exc:  # pragma: no cover - exercised in workflow setup
    raise SystemExit(
        "Pillow is required to render benchmark JPGs. Install python3-pil."
    ) from exc


REPO_ROOT = Path(__file__).resolve().parent.parent
INPUT = REPO_ROOT / "benchmark-output" / "comparison.json"
OUT_DIR = REPO_ROOT / "benchmark-stats"


@dataclass(frozen=True)
class BenchmarkStyle:
    title: str
    subtitle: str
    output: str


@dataclass(frozen=True)
class OverlaySection:
    label: str
    cold_key: str
    warm_key: str


@dataclass(frozen=True)
class ColorPair:
    cold: str
    warm: str


BENCHMARKS = {
    "rust-only": BenchmarkStyle(
        "soldr Rust benchmarks",
        "bare cargo vs sccache vs soldr",
        "benchmark-rust-only.jpg",
    ),
    "rust-c": BenchmarkStyle(
        "soldr Rust+C benchmarks", "rust-native fixture", "benchmark-rust-c.jpg"
    ),
}

# The label literal is asserted as source text by tests/test_perf_measurement_contract.py,
# so the call must stay on one line.
# fmt: off
OVERLAY_SECTIONS = [
    OverlaySection("Clean-target rebuild (same workspace; warm compiler cache)", "cold", "warm"),
    OverlaySection("Agent worktree / parent-child share", "cold", "worktree-share"),
]
# fmt: on
TOOL_ORDER = ["bare", "sccache", "soldr"]
TOOL_COLOR_PAIRS = {
    "bare": ColorPair("#3b4046", "#8b949e"),
    "sccache": ColorPair("#1f3a7a", "#79c0ff"),
    "soldr": ColorPair("#5b1f1c", "#f85149"),
}
WARM_VIOLATION_OUTLINE = "#ffd33d"


def load_comparison() -> ComparisonDocument:
    with INPUT.open("r", encoding="utf-8-sig") as f:
        return ComparisonDocument.from_json(json.load(f))


def font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    names = (
        ("DejaVuSans-Bold.ttf", "Arial Bold.ttf")
        if bold
        else ("DejaVuSans.ttf", "Arial.ttf")
    )
    for name in names:
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            pass
    return ImageFont.load_default()


def text_width(draw: ImageDraw.ImageDraw, value: str, fnt: Any) -> int:
    box = draw.textbbox((0, 0), value, font=fnt)
    return int(box[2] - box[0])


def truncate(draw: ImageDraw.ImageDraw, value: str, fnt: Any, max_width: int) -> str:
    if text_width(draw, value, fnt) <= max_width:
        return value
    suffix = "..."
    available = max(0, max_width - text_width(draw, suffix, fnt))
    out = ""
    for char in value:
        if text_width(draw, out + char, fnt) > available:
            break
        out += char
    return out.rstrip() + suffix


def seconds_label(ms: Any) -> str:
    if not isinstance(ms, (int, float)) or ms <= 0:
        return "n/a"
    seconds = ms / 1000.0
    if seconds < 1:
        return f"{ms:.0f}ms"
    return f"{seconds:.3f}s"


def bytes_label(value: Any) -> str:
    if not isinstance(value, (int, float)) or value < 0:
        return "n/a"
    units = ("B", "KiB", "MiB", "GiB")
    amount = float(value)
    unit = units[0]
    for unit in units:
        if amount < 1024 or unit == units[-1]:
            break
        amount /= 1024
    if unit == "B":
        return f"{int(amount)} B"
    if amount < 10:
        return f"{amount:.1f} {unit}"
    return f"{amount:.0f} {unit}"


def find_row(
    rows: list[ComparisonResult], benchmark: str, scenario_key: str, tool: str
) -> ComparisonResult | None:
    """The last row for (benchmark, scenario, tool), or None."""
    for row in reversed(rows):
        if (row.benchmark, row.scenario_key, row.tool) == (
            benchmark,
            scenario_key,
            tool,
        ):
            return row
    return None


@dataclass(frozen=True)
class ScaleMax:
    value: float
    source: str


@dataclass(frozen=True)
class Box:
    x0: int
    y0: int
    x1: int
    y1: int


def overlay_speedup_label(
    rows: list[ComparisonResult],
    benchmark: str,
    warm_key: str,
) -> str:
    sccache_row = find_row(rows, benchmark, warm_key, "sccache")
    soldr_row = find_row(rows, benchmark, warm_key, "soldr")
    sccache = sccache_row.wall_ms if sccache_row else None
    soldr = soldr_row.wall_ms if soldr_row else None
    if not isinstance(sccache, (int, float)) or not isinstance(soldr, (int, float)):
        return ""
    if sccache <= 0 or soldr <= 0:
        return ""
    ratio = sccache / soldr
    if ratio >= 1:
        return f"soldr clean-target {ratio:.1f}x faster than sccache"
    return f"soldr clean-target {(1 / ratio):.1f}x slower than sccache"


def cold_max_for_section(
    rows: list[ComparisonResult],
    benchmark: str,
    cold_key: str,
    warm_key: str,
) -> ScaleMax:
    cold_values = []
    for tool in TOOL_ORDER:
        row = find_row(rows, benchmark, cold_key, tool)
        value = row.wall_ms if row else None
        if isinstance(value, (int, float)) and value > 0:
            cold_values.append(float(value))
    if cold_values:
        return ScaleMax(max(cold_values), "cold")

    warm_values = []
    for tool in TOOL_ORDER:
        row = find_row(rows, benchmark, warm_key, tool)
        value = row.wall_ms if row else None
        if isinstance(value, (int, float)) and value > 0:
            warm_values.append(float(value))
    if warm_values:
        return ScaleMax(max(warm_values), "warm")
    return ScaleMax(1.0, "none")


@dataclass(frozen=True)
class Canvas:
    """A Pillow draw handle plus the supersampling scale applied to every call."""

    draw: ImageDraw.ImageDraw
    scale: int

    # pylint: disable-next=redefined-outer-name
    def text(self, x: int, y: int, value: str, *, font: Any, fill: str) -> None:
        self.draw.text((x * self.scale, y * self.scale), value, font=font, fill=fill)

    def hline(self, x0: int, x1: int, y: int, fill: str) -> None:
        scale = self.scale
        self.draw.line(
            [x0 * scale, y * scale, x1 * scale, y * scale], fill=fill, width=scale
        )

    def rect(
        self, box: Box, fill: str, outline: str | None = None, width_px: int = 1
    ) -> None:
        scale = self.scale
        self.draw.rectangle(
            [box.x0 * scale, box.y0 * scale, box.x1 * scale, box.y1 * scale],
            fill=fill,
            outline=outline,
            width=width_px * scale,
        )


def render_benchmark(doc: ComparisonDocument, benchmark: str) -> Path:
    rows = [row for row in doc.results if row.benchmark == benchmark]
    tool_labels = {item.key: item.label for item in doc.tools}

    scale = 3
    width = 900
    margin = 20
    header_h = 116
    legend_h = 48
    section_h = 240
    section_gap = 16
    footer_h = 50
    height = (
        header_h
        + legend_h
        + (section_h + section_gap) * len(OVERLAY_SECTIONS)
        - section_gap
        + footer_h
    )

    image = Image.new("RGB", (width * scale, height * scale), "#0d1117")
    draw = ImageDraw.Draw(image)

    title_font = font(32 * scale, bold=True)
    subtitle_font = font(14 * scale)
    scenario_font = font(20 * scale, bold=True)
    tool_font = font(16 * scale, bold=True)
    value_font = font(15 * scale, bold=True)
    small_font = font(13 * scale)

    canvas = Canvas(draw, scale)
    meta = BENCHMARKS[benchmark]
    canvas.rect(Box(0, 0, width, header_h), "#161b22")
    canvas.text(margin, 22, meta.title, font=title_font, fill="#f0f6fc")
    generated = doc.ran_at
    versions = " | ".join(
        part
        for part in (
            doc.soldr_version,
            doc.sccache_version,
            doc.rustc_version,
        )
        if part
    )
    header_line = f"{meta.subtitle} | generated {generated}"
    canvas.text(
        margin,
        70,
        truncate(draw, header_line, subtitle_font, (width - margin * 2) * scale),
        font=subtitle_font,
        fill="#8b949e",
    )
    canvas.text(
        margin,
        92,
        truncate(
            draw,
            f"{versions} | cold bars in back; clean-target/warm-cache overlays in front",
            subtitle_font,
            (width - margin * 2) * scale,
        ),
        font=subtitle_font,
        fill="#8b949e",
    )

    legend_y = header_h + 10
    legend_x = margin
    canvas.rect(
        Box(legend_x, legend_y + 2, legend_x + 58, legend_y + 22),
        TOOL_COLOR_PAIRS["soldr"].cold,
    )
    canvas.rect(
        Box(legend_x, legend_y + 7, legend_x + 26, legend_y + 17),
        TOOL_COLOR_PAIRS["soldr"].warm,
    )
    canvas.text(
        legend_x + 70,
        legend_y + 1,
        "cold (back) + clean-target/warm-cache (front)",
        font=small_font,
        fill="#c9d1d9",
    )
    violation_x = width - margin - 236
    canvas.rect(
        Box(violation_x, legend_y + 7, violation_x + 34, legend_y + 17),
        TOOL_COLOR_PAIRS["soldr"].warm,
        outline=WARM_VIOLATION_OUTLINE,
        width_px=2,
    )
    canvas.text(
        violation_x + 44,
        legend_y + 1,
        "warm > cold",
        font=small_font,
        fill=WARM_VIOLATION_OUTLINE,
    )

    chart_x = margin
    chart_w = width - margin * 2
    label_w = 150
    value_w = 184
    bar_x0 = chart_x + label_w
    bar_x1 = chart_x + chart_w - value_w
    bar_w = bar_x1 - bar_x0

    y = header_h + legend_h
    for idx, section in enumerate(OVERLAY_SECTIONS):
        fill = "#0f1620" if idx % 2 == 0 else "#11202d"
        canvas.rect(Box(chart_x, y, chart_x + chart_w, y + section_h), fill)
        title = section.label
        canvas.text(chart_x + 16, y + 14, title, font=scenario_font, fill="#f0f6fc")
        scale_max = cold_max_for_section(
            rows,
            benchmark,
            section.cold_key,
            section.warm_key,
        )
        speedup = overlay_speedup_label(rows, benchmark, section.warm_key)
        if speedup:
            speedup_w = text_width(draw, speedup, subtitle_font) // scale
            canvas.text(
                chart_x + chart_w - 16 - speedup_w,
                y + 20,
                speedup,
                font=subtitle_font,
                fill="#f85149",
            )

        if scale_max.source == "cold":
            scale_note = f"100% = cold max {seconds_label(scale_max.value)}"
        elif scale_max.source == "warm":
            scale_note = (
                f"100% = warm max {seconds_label(scale_max.value)} (no cold data)"
            )
        else:
            scale_note = "no data"
        canvas.text(
            chart_x + 16,
            y + 42,
            scale_note,
            font=small_font,
            fill="#8b949e",
        )

        row_y = y + 76
        for tool in TOOL_ORDER:
            cold_row = find_row(rows, benchmark, section.cold_key, tool)
            warm_row = find_row(rows, benchmark, section.warm_key, tool)
            cold_value = cold_row.wall_ms if cold_row else None
            warm_value = warm_row.wall_ms if warm_row else None
            label = tool_labels.get(tool, tool)
            colors = TOOL_COLOR_PAIRS[tool]
            warm_violation = (
                isinstance(cold_value, (int, float))
                and isinstance(warm_value, (int, float))
                and cold_value > 0
                and warm_value > cold_value * 1.10
            )

            canvas.text(
                chart_x + 16,
                row_y + 14,
                label,
                font=tool_font,
                fill=colors.warm,
            )
            track_y0 = row_y + 20
            canvas.rect(Box(bar_x0, track_y0 + 10, bar_x1, track_y0 + 14), "#21262d")
            if isinstance(cold_value, (int, float)) and cold_value > 0:
                width_fraction = max(
                    0.01, min(1.0, float(cold_value) / scale_max.value)
                )
                bar_end = bar_x0 + max(3, int(bar_w * width_fraction))
                canvas.rect(Box(bar_x0, track_y0, bar_end, track_y0 + 24), colors.cold)
            if isinstance(warm_value, (int, float)) and warm_value > 0:
                width_fraction = max(
                    0.01, min(1.0, float(warm_value) / scale_max.value)
                )
                bar_end = bar_x0 + max(3, int(bar_w * width_fraction))
                canvas.rect(
                    Box(bar_x0, track_y0 + 5, bar_end, track_y0 + 19),
                    colors.warm,
                    outline=WARM_VIOLATION_OUTLINE if warm_violation else None,
                    width_px=2 if warm_violation else 1,
                )
            canvas.text(
                bar_x1 + 12,
                row_y + 2,
                f"cold {seconds_label(cold_value)}",
                font=value_font,
                fill=(
                    colors.cold
                    if isinstance(cold_value, (int, float)) and cold_value > 0
                    else "#6e7681"
                ),
            )
            canvas.text(
                bar_x1 + 12,
                row_y + 22,
                f"warm {seconds_label(warm_value)}",
                font=value_font,
                fill=(
                    WARM_VIOLATION_OUTLINE
                    if warm_violation
                    else colors.warm
                    if isinstance(warm_value, (int, float)) and warm_value > 0
                    else "#6e7681"
                ),
            )
            cache_value = warm_row.cache_bytes if warm_row else None
            canvas.text(
                bar_x1 + 12,
                row_y + 42,
                f"cache {bytes_label(cache_value)}",
                font=small_font,
                fill="#8b949e",
            )
            row_y += 54

        canvas.hline(
            chart_x + 16,
            chart_x + chart_w - 16,
            y + section_h - 1,
            "#30363d",
        )
        y += section_h + section_gap

    footer = "Artifacts: latest.json, benchmark-rust-only.jpg, benchmark-rust-c.jpg"
    canvas.hline(margin, width - margin, height - 34, "#30363d")
    canvas.text(margin, height - 25, footer, font=small_font, fill="#8b949e")

    image = image.resize((width, height), Image.Resampling.LANCZOS)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    output = OUT_DIR / meta.output
    image.save(output, format="JPEG", quality=90, optimize=True)
    return output


def main() -> int:
    if not INPUT.exists():
        print(f"render: missing {INPUT}", file=sys.stderr)
        return 1
    doc = load_comparison()
    for benchmark in BENCHMARKS:
        output = render_benchmark(doc, benchmark)
        print(f"render: wrote {output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
