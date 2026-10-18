# sdigest: sparse digests

## Summary

`sdigest` fingerprints files by hashing their **size** and a deterministic
**sample** of their content, instead of every byte. A sample of a few blocks
is enough to tell most non-identical files apart, at a tiny fraction of the
I/O cost of a full digest. A *level* (0–9) or a *coverage* (0–100%) controls
how much content is sampled.

```
$ sdigest -l 0 *.flac
sd1:31457806:0:9f2c…41e7  a.flac
sd1:31457806:0:9f2c…41e7  b.flac
sd1:28011203:0:03ab…c9d0  c.flac
```

### Interpreting sdigests

An sdigest is **evidence that content differs**, never proof that it is
the same.

| Fingerprints A and B | Conclusion |
|---|---|
| Sizes differ | Content differs. **Certain.** |
| Same size and coverage, digests differ | Content differs. **Certain.** |
| Same size and coverage, digests equal | Content *may* be identical. |
| Same size, different coverage, same plan¹ | Same as for equal coverage. |
| Same size, different coverage, different plans | No conclusion. |

¹ The size and coverage determine exactly which bytes are hashed. If two
coverages produce the same set of blocks for a size, their digests are
comparable. `may_equal()` checks this.

How likely "may be identical" is to be right depends on how files of the
same size tend to differ, not only on coverage:

* **Unrelated content** (different recordings, encodings, builds) differs
  almost everywhere, so level 0 (size + first and last 64 KiB) almost
  always tells files apart.
* **Small in-place edits** that keep the size are missed when they fall
  between samples, with probability ≈ 1 − k·(B+L)/N for k blocks of size B,
  an edit of length L, and a file of N bytes. Only high coverage catches
  these reliably.
* **Adversarial content**: sparse digests are not collision-resistant.
  Anyone who knows the algorithm can change unsampled bytes. Use level 9
  (100%) or a cryptographic digest when that matters.

Confirm with `cmp` whenever a false "identical" would be costly. Use
sdigest to rule out non-identical files cheaply.

## Goals and design implications

* Orders of magnitude faster than full digests on large files
  ⇒ hash a sparse sample, not the whole file
  ⇒ require seekable input; reject stdin
* Unequal fingerprints must prove unequal content
  ⇒ the sample depends only on (size, coverage)
  ⇒ fingerprints carry the size and coverage so comparability can be checked
* Stable fingerprints across machines, runs and releases
  ⇒ fixed block size, not inferred from the device
  ⇒ deterministic integer plan; canonical text for coverage
  ⇒ version tag (`sd1`) on every fingerprint
* Cheap discrimination of same-size files
  ⇒ the size is always hashed first: level 0 is "size plus head and tail"
  ⇒ the first and last blocks are always sampled (headers, tags, trailers)
* Tunable from "nearly free" up to "full digest"
  ⇒ coverage 0–100%, with levels 0–9 as a logarithmic shorthand
* Usable from Python, including on streams
  ⇒ the core is generic over `Read + Seek`; pyo3 adapts Python file objects

## Design

### Sampling plan

For a file of `N` bytes, block size `B` = 64 KiB and coverage `c` (percent):

1. `k = clamp(ceil(N·c/100 / B), 2, ceil(N/B))` blocks.
2. If `N ≤ 2B` or `k·B ≥ N`, read the whole file.
3. Otherwise read:
   * the first block, `[0, B)`;
   * `k−2` interior blocks at `floor(i·(N−B)/(k−1) / B)·B` for
     `i = 1..k−2`, which are aligned to `B` and evenly spaced;
   * the last block, `[N−B, N)`, trimmed so it doesn't overlap the
     previous block.

Ranges are ascending and disjoint, so a hard drive reads them in order. The
plan is all-integer arithmetic, apart from computing `k`, which uses IEEE
`f64` (deterministic on every platform). It is a pure function
(`sdigest::plan`), kept separate from I/O.

### Hash input and output format

```
BLAKE3( ascii_decimal(N) ‖ bytes of each planned range, in order )
```

```
sd1:<size>:<coverage>:<blake3-256 hex>
```

* `coverage` is the shortest decimal that round-trips (`0`, `0.03`, `5`,
  `100`), so equal coverages produce equal strings.
* Level 9 (100%) hashes every byte, but because of the size prefix the
  digest is not equal to `b3sum` output.
* CLI output is `sha512sum`-style (`<fingerprint>  <path>`) and supports
  `--check`.

### Levels

| level | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|---|---|---|---|
| coverage % | 0 | 0.01 | 0.03 | 0.1 | 0.3 | 1 | 3 | 10 | 30 | 100 |

Levels are only a shorthand. Fingerprints record the coverage, so `-l 5` and
`-c 1` give identical output. The default is level 5.

### Parallelism

The CLI reads files in parallel; each thread reads one file at a time, in
plan order. The best thread count depends on the storage and on how sparse
the reads are ([#3](https://github.com/reece/sdigest/issues/3),
benchmark 3), so `--threads auto` (the default) chooses it per device:

| | Sparse (coverage < 10%) | Dense (coverage ≥ 10%) |
|---|---|---|
| Identified device | `min(queue depth, 16)` | rotational: 1; otherwise `min(queue depth, CPUs)` |
| Unidentified | CPUs | CPUs |

* **Sparse reads are bound by seeks.** They gain from many reads in flight
  on devices that can queue them: up to 4× on UAS hard disks and 3.8× on
  NVMe. They're capped at 16 because beyond that, hard disks gained only
  7–20% more while sdigest held most of the disk's command queue, delaying
  other work on that disk.
* **Dense reads on a hard disk are fastest one file at a time.**
  Interleaving files only adds seeks; 64 threads cost 20–46%. On SSDs,
  hashing becomes the limit, so dense reads use one thread per CPU.
* **A queue depth of 1** (for example, USB disks on the old `usb-storage`
  driver) gives 1 throughout. Any parallelism was 15–46% slower there.
* **Detection** (Linux only) follows the file's device through sysfs:
  partitions to their disk, dm-crypt, LVM and md to the disks beneath. It
  reads `queue/rotational` and `device/queue_depth`, or `queue/nr_requests`
  for NVMe. With several disks it takes the worst case. Anything else,
  including network filesystems, FUSE and other systems, keeps one thread
  per CPU, the behaviour before detection existed.
* Files on different devices are read concurrently. On one device, files
  needing different counts (mixed coverages in a `--check` list) are read
  one group after another. `--threads N` overrides everything.

The 10% boundary between sparse and dense is an estimate: benchmarks
covered levels 0 and 5 (sparse) and 9 (dense), not levels 6–8.

### Components

| Path | Role |
|---|---|
| `src/lib.rs` | `Coverage`, `plan`, `digest_reader`, `digest_path`, `Fingerprint` (parse, format, `may_equal`) |
| `src/cli.rs` | CLI (`cli` feature, default): parallel over files (one rayon pool per device), progress bar, summary, `--check`, names on stdin |
| `src/medium.rs` | Storage detection (Linux sysfs) and the thread-count policy |
| `src/main.rs` | Rust binary: calls `cli::run` |
| `src/python.rs` | pyo3 module `sdigest` (`sdigest`, `may_equal`) and the `sdigest` script entry point, which also calls `cli::run`; enabled with the `python` feature |
| `tests/cli.rs`, `tests/python/` | CLI and Python tests; golden digests live in the unit tests |

Python accepts a path (it releases the GIL and reads the file in Rust) or any
binary file object with `read`/`seek`:

```python
from sdigest import may_equal, sdigest

fp = sdigest("a.flac", level=0)
may_equal(fp, sdigest(open("b.flac", "rb"), level=0))  # True/False/None
```

## Rationale and alternatives

* **BLAKE3 over SHA-512.** BLAKE3 is several times faster and still
  cryptographic. Sparse sampling already gives up adversarial robustness, so
  a non-cryptographic hash (xxh3) would also be defensible. BLAKE3 keeps
  level 9 meaningful as a content digest.
* **Fixed block size rather than one inferred from the device.** Inferring it
  would make fingerprints depend on where a file is stored. 64 KiB is large
  enough to amortize a seek and small enough to keep level 0 cheap.
* **Evenly spaced rather than pseudo-random blocks.** Random positions catch
  nothing more against non-adversarial differences, and give up sequential
  read order. Against an adversary, neither helps.
* **Always sampling the first and last block.** Formats concentrate
  metadata there (ID3v2/FLAC headers, ID3v1 trailers, container indexes),
  and tag edits often keep the size because of padding.
* **Coverage and levels rather than a block or byte budget.** A per-file
  budget (`--blocks`, `--max-bytes`) caps cost on huge files but makes high
  coverage awkward to request. Coverage is precise but more precise than the
  false-positive rate justifies. Levels on a 1-3-10 scale put most steps
  where the useful trade-offs are (below 1%).
* **Parameters in the fingerprint.** Without them, a mismatch between
  fingerprints made at different settings would wrongly look like proof of
  difference.
* **Whole-file hashing for small files.** Below two blocks, sampling saves
  nothing.

## Results

### Speed

All times are with a cold page cache: each run's files were dropped from
the cache with `posix_fadvise(POSIX_FADV_DONTNEED)` first, and the disk was
idle. Earlier versions of this document quoted 9.6 s for level 0 over a
whole 176 GiB archive; that run was mostly served from the page cache.

Conditions: sdigest 0.2.1, release build; 4-core Linux host; Seagate
Expansion Desk 4.5 TB hard disk (`uas`, queue depth 30, mq-deadline), ext4;
mean of two runs. Threads were set with `RAYON_NUM_THREADS`; the default is
one per core (4 here). Full data and method are in
[#3](https://github.com/reece/sdigest/issues/3).

Level 0, 3,534 audio files, 28.1 GiB (7,068 reads of 64 KiB):

| Threads | USB 2 | USB 3 |
|---|---|---|
| 1 | 68.4 s | 58.7 s |
| 4 (default) | 52.2 s | 56.1 s |
| 16 | 44.8 s | 41.1 s |

Level 9 (full reads), a random sample of about 265 of those files, 2.0 GiB:

| Threads | USB 2 | USB 3 |
|---|---|---|
| 1 | 38.9 MiB/s | 122 MiB/s |
| 4 (default) | 36.9 MiB/s | 138 MiB/s |
| 16 | 37.6 MiB/s | 122 MiB/s |

Level 0 is limited by seek time, so it costs about the same per file
whatever the file size; more reads in flight help. Level 9 is limited by the
USB 2 link (about 40 MiB/s), or on USB 3 by the drive; parallelism barely
helps it. On cached data, hashing is not the bottleneck: level 9 runs at
1.6–3 GiB/s, against ~0.4 GiB/s for `sha512sum`.

### Accuracy

Audio archive on the same disk, 16,167 files, 176 GiB. 911 files share a
size with at least one other file (389 sizes). Level 0 and level 9 sort
those 911 files into exactly the same possibly-identical groups (336
files): **no false positives**. (Level 9 read 16.7 GiB in 412 s over USB 2,
about the link's limit.)

## Unresolved questions and future work

* Per-file byte cap for very large files (e.g. 1 TB at 1% is 10 GB).
* Faster level 9: multithreaded BLAKE3 with mmap.
* More I/O strategies: parallel `pread` within large files, read-pattern
  hints (`posix_fadvise(RANDOM)` for sparse levels; readahead inflated sparse
  reads 1.5–2.5× in benchmark 3), and object stores. The plan/I/O split is
  there to support this.
* Measure levels 6–8 to place the sparse/dense boundary.
* `--check` does not escape paths containing newlines or double spaces.
* A duplicate-grouping mode (`--dups`) that applies the comparability rules
  above.
