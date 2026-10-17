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

### Components

| Path | Role |
|---|---|
| `src/lib.rs` | `Coverage`, `plan`, `digest_reader`, `digest_path`, `Fingerprint` (parse, format, `may_equal`) |
| `src/main.rs` | CLI (`cli` feature, default): parallel over files (rayon), progress bar, summary, `--check`, names on stdin |
| `src/python.rs` | pyo3 module `sdigest` (`sdigest`, `may_equal`), enabled with the `python` feature |
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

Audio archive on a USB hard disk (ext4), 16,167 files, 176 GiB.
911 files share a size with at least one other file (389 sizes).

| Run | Time | Read |
|---|---|---|
| Level 0, all files | 9.6 s | 1.97 GiB |
| Level 9, 911 same-size files | 412 s | 16.7 GiB |

Level 0 and level 9 sort the 911 same-size files into exactly the same
possibly-identical groups (336 files): **no false positives**. For comparison,
`sha512sum` runs at ~0.4 GiB/s on cached data. Level 9 runs at 1.6–3 GiB/s;
level 0 is limited by seeks, not hashing.

## Unresolved questions and future work

* Per-file byte cap for very large files (e.g. 1 TB at 1% is 10 GB).
* Faster level 9: multithreaded BLAKE3 with mmap.
* I/O strategies by media type (parallel `pread` on SSD/NVMe and object
  stores). The plan/I/O split is there to support this.
* `--check` does not escape paths containing newlines or double spaces.
* A duplicate-grouping mode (`--dups`) that applies the comparability rules
  above.
