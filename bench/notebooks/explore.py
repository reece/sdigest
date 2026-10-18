# /// script
# requires-python = ">=3.11"
# dependencies = ["marimo", "duckdb>=1.1", "altair>=5", "polars>=1", "pyarrow"]
# ///
"""Explore sdbench results. Run with:

uv run marimo edit --sandbox bench/notebooks/explore.py
"""

import marimo

__generated_with = "0.16.0"
app = marimo.App(width="medium", app_title="sdbench explorer")


@app.cell
def _():
    import sys
    from pathlib import Path

    import altair as alt
    import marimo as mo

    bench = Path(__file__).resolve().parent.parent
    sys.path.insert(0, str(bench))
    import sdbench

    return alt, mo, sdbench


@app.cell
def _(mo):
    mo.md(
        """
        # sdbench explorer

        Benchmark results from `bench/runs/` (and `bench/runs-dev/` if included),
        through the shared views in `bench/queries/views.sql`. Runs flagged for
        foreign I/O or excluded by an annotation are left out.
        """
    )
    return


@app.cell
def _(mo):
    dev = mo.ui.checkbox(label="include runs-dev/ (unreleased builds)")
    dev
    return (dev,)


@app.cell
def _(dev, sdbench):
    con = sdbench.connect(dev.value)
    runs = con.sql("SELECT * FROM good_runs").pl()
    summary = con.sql("SELECT * FROM summary").pl()
    return con, runs, summary


@app.cell
def _(mo, runs):
    def options(col):
        return sorted(runs[col].unique().to_list())

    version = mo.ui.dropdown(options("version"), value=options("version")[-1], label="version")
    host = mo.ui.dropdown(options("host"), value=options("host")[0], label="host")
    dataset = mo.ui.dropdown(options("dataset"), value=options("dataset")[0], label="dataset")
    cache = mo.ui.dropdown(
        options("cache"),
        value="cold" if "cold" in options("cache") else options("cache")[0],
        label="cache",
    )
    devices = mo.ui.multiselect(options("device"), value=options("device"), label="devices")
    mo.hstack([version, host, dataset, cache, devices], wrap=True)
    return cache, dataset, devices, host, version


@app.cell
def _(cache, dataset, devices, host, runs, summary, version):
    import polars as pl

    def pick(df):
        return df.filter(
            (pl.col("version") == version.value)
            & (pl.col("host") == host.value)
            & (pl.col("dataset") == dataset.value)
            & (pl.col("cache") == cache.value)
            & pl.col("device").is_in(devices.value)
        )

    sel_runs, sel_summary = pick(runs), pick(summary)
    return pl, sel_runs, sel_summary


@app.cell
def _(alt, mo, sel_runs, sel_summary):
    threads_x = alt.X("threads:Q", scale=alt.Scale(type="log", base=2), title="threads")

    def time_panel(level):
        points = (
            alt.Chart(sel_runs.filter(sel_runs["level"] == level))
            .mark_circle(size=30, opacity=0.5)
            .encode(
                threads_x,
                alt.Y("elapsed_s:Q", title="seconds"),
                alt.Color("device:N"),
                tooltip=["device", "threads", "rep", "elapsed_s", "device_reads_per_s"],
            )
        )
        lines = (
            alt.Chart(sel_summary.filter(sel_summary["level"] == level))
            .mark_line(point=True)
            .encode(threads_x, alt.Y("median_s:Q"), alt.Color("device:N"))
        )
        return (points + lines).properties(width=260, height=220, title=f"level {level}")

    levels = sorted(sel_summary["level"].unique().to_list())
    time_chart = alt.hconcat(*[time_panel(lv) for lv in levels]).resolve_scale(y="independent")
    mo.vstack([mo.md("## Time vs. threads\nDots are single runs; lines are medians."), time_chart])
    return (threads_x,)


@app.cell
def _(alt, mo, sel_summary, threads_x):
    speedup = (
        alt.Chart(sel_summary)
        .mark_line(point=True)
        .encode(
            threads_x,
            alt.Y("speedup:Q", title="speedup over 1 thread"),
            alt.Color("device:N"),
            tooltip=["device", "level", "threads", "speedup"],
        )
        .properties(width=260, height=220)
        .facet(column=alt.Column("level:O", title="level"))
    )
    mo.vstack([mo.md("## Speedup over one thread"), speedup])
    return


@app.cell
def _(alt, mo, sel_summary, threads_x):
    sparse = (
        alt.Chart(sel_summary.filter(sel_summary["level"] < 9))
        .mark_line(point=True)
        .encode(
            threads_x,
            alt.Y("median_reads_per_s:Q", title="device reads/s"),
            alt.Color("device:N"),
            alt.StrokeDash("level:N"),
        )
        .properties(width=360, height=240, title="Sparse levels: reads per second")
    )
    full = (
        alt.Chart(sel_summary.filter(sel_summary["level"] == 9))
        .mark_line(point=True)
        .encode(threads_x, alt.Y("median_read_mib_s:Q", title="MiB/s"), alt.Color("device:N"))
        .properties(width=360, height=240, title="Level 9: throughput")
    )
    mo.vstack([mo.md("## What limits each medium"), mo.hstack([sparse, full])])
    return


@app.cell
def _(alt, con, mo):
    trend = con.sql("""
        SELECT version, device, dataset, level, cache, threads, median_s FROM summary
    """).pl()
    by_version = (
        alt.Chart(trend)
        .mark_line(point=True)
        .encode(
            alt.X("version:O"),
            alt.Y("median_s:Q", title="median seconds"),
            alt.Color("device:N"),
            alt.Detail("threads:O"),
            tooltip=["version", "device", "dataset", "level", "threads", "median_s"],
        )
        .properties(width=560, height=220)
        .facet(row=alt.Row("level:O"))
    )
    mo.vstack([mo.md("## Across versions\nOne line per device and thread count."), by_version])
    return


@app.cell
def _(mo, sel_summary):
    mo.vstack([mo.md("## Summary table"), mo.ui.table(sel_summary, selection=None)])
    return


if __name__ == "__main__":
    app.run()
