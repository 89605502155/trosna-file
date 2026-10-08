"""Plots of results.csv written by storage_benchmark.py (labels in Russian).

Usage: python plot_benchmark.py results/results.csv --out figures
Produces size.png (bytes per point, two datasets) and speed.png (write and
read time per series, sensor dataset), 300 dpi, plus SVG copies.
"""

from __future__ import annotations

import argparse
import csv
from collections import defaultdict
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import LogLocator  # noqa: E402

# Fixed order: the identity of a format never depends on its rank.
# The journal prints in black and white, so formats are told apart by marker
# and line style first; the gray level only helps.
STYLE = {
    "Trosna": ("#000000", "o", "-"),
    "TsFile": ("#4d4d4d", "s", "--"),
    "Parquet": ("#4d4d4d", "^", "-."),
    "Arrow IPC": ("#7a7a7a", "D", ":"),
    "HDF5": ("#7a7a7a", "v", "--"),
    "CSV.gz": ("#9a9a9a", "P", "-."),
    "CSV": ("#9a9a9a", "X", ":"),
}
INK = "#2b2b2b"
MUTED = "#6b6b6b"
GRID = "#e4e4e0"

plt.rcParams.update(
    {
        "font.family": "serif",
        "font.serif": ["Times New Roman", "Liberation Serif", "DejaVu Serif"],
        "font.size": 10,
        "axes.edgecolor": MUTED,
        "axes.labelcolor": INK,
        "xtick.color": MUTED,
        "ytick.color": MUTED,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "legend.frameon": False,
    }
)


def load(path: Path):
    data = defaultdict(lambda: defaultdict(list))
    with path.open() as f:
        for row in csv.DictReader(f):
            data[row["dataset"]][row["format"]].append(
                (int(row["n"]), float(row["bytes_per_point"]), float(row["write_s"]), float(row["read_s"]))
            )
    for by_format in data.values():
        for rows in by_format.values():
            rows.sort()
    return data


def style_axis(ax, xlabel, ylabel):
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.xaxis.set_major_locator(LogLocator(base=10))
    ax.grid(True, which="major", color=GRID, linewidth=0.6)
    ax.set_axisbelow(True)
    ax.set_xlabel(xlabel)
    ax.set_ylabel(ylabel)


def plot_series(ax, by_format, column):
    for name, (color, marker, line) in STYLE.items():
        rows = by_format.get(name)
        if not rows:
            continue
        xs = [r[0] for r in rows]
        ys = [r[column] for r in rows]
        ax.plot(
            xs,
            ys,
            color=color,
            marker=marker,
            linestyle=line,
            linewidth=2.4 if name == "Trosna" else 1.4,
            markersize=5,
            markerfacecolor="white" if name != "Trosna" else color,
            markeredgecolor=color,
            markeredgewidth=0.9,
            label=name,
            zorder=3 if name == "Trosna" else 2,
        )


def figure_size(data, out: Path):
    panels = [d for d in ("sensor", "noise") if d in data]
    titles = {
        "sensor": "а) показания датчика (шаг 0,01)",
        "noise": "б) шум полной точности float64",
    }
    fig, axes = plt.subplots(1, len(panels), figsize=(7.0, 3.2), sharex=True)
    axes = axes if len(panels) > 1 else [axes]
    for ax, dataset in zip(axes, panels):
        plot_series(ax, data[dataset], 1)
        style_axis(ax, "Число точек ряда N", "Объём на одну точку, байт")
        ax.set_title(titles[dataset], fontsize=10, color=INK)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower center", ncol=len(labels), fontsize=9, bbox_to_anchor=(0.5, -0.02))
    fig.tight_layout(rect=(0, 0.08, 1, 1))
    save(fig, out / "size")


def figure_speed(data, out: Path):
    by_format = data["sensor"]
    fig, axes = plt.subplots(1, 2, figsize=(7.0, 3.2), sharex=True)
    for ax, column, ylabel, title in (
        (axes[0], 2, "Время записи, с", "а) запись с fsync"),
        (axes[1], 3, "Время чтения, с", "б) чтение в память"),
    ):
        plot_series(ax, by_format, column)
        style_axis(ax, "Число точек ряда N", ylabel)
        ax.set_title(title, fontsize=10, color=INK)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower center", ncol=len(labels), fontsize=9, bbox_to_anchor=(0.5, -0.02))
    fig.tight_layout(rect=(0, 0.08, 1, 1))
    save(fig, out / "speed")


def save(fig, stem: Path):
    fig.savefig(stem.with_suffix(".png"), dpi=300, bbox_inches="tight")
    fig.savefig(stem.with_suffix(".svg"), bbox_inches="tight")
    # line art for print: 600 dpi TIFF, as the journal requires
    fig.savefig(stem.with_suffix(".tiff"), dpi=600, bbox_inches="tight", pil_kwargs={"compression": "tiff_lzw"})
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("results", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    data = load(args.results)
    figure_size(data, args.out)
    if "sensor" in data:
        figure_speed(data, args.out)


if __name__ == "__main__":
    main()
