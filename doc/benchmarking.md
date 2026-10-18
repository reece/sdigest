# Benchmarking sdigest

How sdigest's I/O performance is measured, recorded and compared across
versions, hosts and storage media. The harness is `bench/sdbench.py`
(`just bench …`). The questions it serves are in
[#3](https://github.com/reece/sdigest/issues/3).

## Goals

1. **Repeatable.** Anyone can rerun a measurement and get comparable
   numbers. Repeatability comes before ease of use: the people running
   benchmarks are developers.
2. **Comparable.** Results line up across sdigest versions, hosts, devices
   and datasets without manual bookkeeping.
3. **Trustworthy.** A result says what it measured. Cache effects and
   competing I/O are prevented or flagged, never silent.
4. **Permanent.** Results are kept as a record; they are never edited.
5. **Explorable.** Results can be queried from the terminal, a notebook and
   an interactive page.

## Implications

### Datasets are synthetic and named by their full specification ⇐ 1, 2

A dataset is made by a named *generator* from parameters, all of them
required, each with one canonical form (sizes are always MB = 10⁶ bytes).
One spec has exactly one name:

```
random --seed 1 --n-files 1000 --dist loguniform --min-mb 5 --max-mb 50
  → random-v1-s1-n1000-loguniform-5-50mb
```

- Content is SHAKE-256 output keyed by (seed, file, chunk): incompressible,
  never sparse, identical on every machine.
- Files are written in a shuffled order, then flushed and evicted.
- The same dataset is written to each device under test, under
  `<data-dir>/<dataset-id>/`, with a manifest, and reused while it matches.
- A change to a generator's output bumps its version (`random-v2-…`).
- Archive data is never used, so no private file names reach the repo.

Fresh, unfragmented files are the best case for layout. Aged or fragmented
layouts are future work, as are mutated datasets.

### Identity is coarse; detail is kept but not keyed ⇐ 2, 3

Hosts and devices get structured IDs: `kevin-0001`,
`samsung-ssd-970-evo-plus-0001`. A new number is issued only when an
*identity* field changes:

| Record | Identity (keyed) | Details (recorded per session) |
|---|---|---|
| Host | hostname, CPU model, threads, RAM (GiB), kernel major.minor, OS release | full kernel release, governor, rustc, Python |
| Device | model, transport, USB driver, link, rotational, queue depth, scheduler, layers (e.g. crypt), filesystem, size | mount point, data directory |

A patch-level kernel update or a reboot doesn't split comparisons.

### Versions come from git; only release tags are committed ⇐ 2, 4

`sdbench run --ref REF` builds sdigest's release binary at REF in a
temporary worktree (cached per commit), so any version can be measured from
any checkout. `REF=WORKTREE` builds the working tree, uncommitted changes
included.

- Every run records the commit and `git describe --tags --long`
  (`0.2.1-0-g…`).
- **Exact, clean release tags** (including release candidates such as
  `0.1.2-rc.0`) write to `bench/runs/` and are committed.
- **Everything else** writes to `bench/runs-dev/`, which is gitignored, with
  a version like `0.2.1-6-g01234ab` or `0.2.1-6-g01234ab-dirty.3f9a`. The
  suffix after `dirty.` hashes the uncommitted diff, so runs of the same
  edit group together.

### Records are append-only JSON Lines, laid out version first ⇐ 2, 4

```
bench/
  runs/version=0.2.1/host=kevin-0001/dataset=random-v1-…/<ts>_<device>_<session>.jsonl
                                                         <ts>_<device>_<session>.session.json
  runs-dev/…                    same layout, not committed
  hosts/kevin-0001.json
  devices/<device-id>.json
  datasets/<dataset-id>.json
  annotations/*.jsonl           corrections: {"run_id", "action": "exclude", "reason", "timestamp"}
  configs/*.toml                sweep definitions
  queries/views.sql             shared views
```

- One line per run, one file per session. Each line is self-describing:
  version, host, device and dataset appear in the record as well as in
  the path.
- Version first makes pruning and version comparisons a matter of
  directories. Pruning, such as replacing an `-rc` with its release, deletes
  files. Nothing is ever modified. A prek hook rejects modifications; a bad
  run is excluded with an annotation.
- JSON Lines diffs and merges like code, and DuckDB reads it directly.

Each run records: timestamp, sdigest commit and describe, host, device,
dataset (with file count and total bytes), level, threads, cache state,
repetition and position in the session, elapsed time, bytes requested by
sdigest, bytes read by the process and by the device, device read count,
effective and device rates, CPU time, exit status and a foreign-I/O flag.
Later records add `read_ahead_kb`, and for builds with `--threads`,
`threads_mode` (`fixed` or `auto`) and `threads_chosen` (sdigest's own
report, e.g. `16 (sparse, rotational, queue depth 30)`). A config's
`threads` list may include `"auto"`; those runs have `threads` = null.

### Measurements guard against cache and contention ⇐ 3

- **Cold runs.** Before each cold run, the harness waits until the disk is
  idle: no reads for 5 s on a hard disk, a trickle allowed on an SSD. It
  then evicts the dataset with `posix_fadvise(DONTNEED)` and verifies with
  `fincore` that nothing remains cached. The drive's own cache is not
  cleared.
- **Foreign I/O.** If the disk reads noticeably more than sdigest did, the
  run is flagged as `foreign_io` and left out of summaries.
- **Order.** Configurations alternate order across repetitions (1, 4, 16,
  16, 4, 1) so drift favours none of them.
- **Warm runs** measure the CPU limit; their datasets must fit in RAM.

### Analysis has three layers over one set of views ⇐ 5

1. **Terminal.** `sdbench ls | report | compare A B | sql QUERY` print
   markdown, ready to paste into an issue. `sdbench db FILE` writes
   everything to a DuckDB file for `duckdb` or `duckdb -ui`.
2. **Notebook.** `bench/notebooks/explore.py`, a marimo notebook (plain
   `.py`) with interactive filters and the standard plots:
   `uv run marimo edit --sandbox bench/notebooks/explore.py`.
3. **Page.** `bench/page/index.html`, a self-contained results page.
   `sdbench export bench/page/data.json` writes its data from the views;
   the page only filters and draws. It doesn't run DuckDB in the browser,
   because hosted pages may block the workers and fetches that would need.

All three query the views in `bench/queries/views.sql`.

## Not yet covered

- Network filesystems, and remote Python file objects (object stores).
- SATA devices, and USB 2 links.
- Experimental I/O switches in sdigest (read hints, disk-order reads, mmap),
  recorded in a `params` field when they exist.
