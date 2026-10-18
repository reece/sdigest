# sdigest

[![CI](https://github.com/reece/sdigest/actions/workflows/ci.yml/badge.svg)](https://github.com/reece/sdigest/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sdigest.svg)](https://crates.io/crates/sdigest)
[![PyPI](https://img.shields.io/pypi/v/sdigest.svg)](https://pypi.org/project/sdigest/)

Fast, tunable **sparse digests**. `sdigest` fingerprints a file by hashing
its size and a deterministic sample of its content with BLAKE3, instead of
every byte. That is often orders of magnitude faster than `sha512sum`,
and enough to tell most non-identical files apart.

```console
$ sdigest -l 0 *.flac
sd1:31457806:0:9f2c…41e7  a.flac
sd1:31457806:0:9f2c…41e7  b.flac
sd1:28011203:0:03ab…c9d0  c.flac
sdigest: 3 files, 85.7 MiB in 0.01 s (8.4 GiB/s); coverage 0%, read 384 KiB (0.437%)
```

On a USB 3 hard disk with a cold page cache, level 0 fingerprinted 3,534
audio files (28.1 GiB) in 56 s. Its cost depends on the number of files,
not their size: two 64 KiB reads per file, at the 100–170 reads per second
this disk managed. Full reads on the same drive ran at about 140 MiB/s, so
level 0 wins for files over about 2 MB, by more as files grow. On a 176 GiB
archive, level 0 found exactly the same possible duplicates as full digests
did. Details and conditions are in the
[design notes](https://github.com/reece/sdigest/blob/main/doc/design.md#results)
and [#3](https://github.com/reece/sdigest/issues/3).

## Interpreting sdigests

An sdigest is evidence that content **differs**, never proof that it is the
same.

* **Different sizes, or the same coverage with different digests:** the
  content differs. This is certain.
* **Equal fingerprints:** the content *may* be identical. Unrelated files
  almost always differ even at level 0, but small edits that keep the size
  can fall between samples. Sparse digests are not collision-resistant
  against deliberate tampering. Confirm with `cmp` when a false match would
  be costly.
* **Same size, different coverage:** no conclusion, unless both coverages
  sample the same blocks (`may_equal` checks this).

## Install

```sh
uvx sdigest FILE...        # run without installing (prebuilt wheels, no Rust needed)
uv tool install sdigest    # install the sdigest command
pip install sdigest        # Python module and command
cargo install sdigest      # build the command from source
```

The `sdigest` command is identical however it is installed.

## Use

```sh
sdigest -l 0 FILE...                     # levels 0-9; default 5 (1% coverage)
sdigest -c 2.5 FILE...                   # or an exact coverage percent
find . -type f -print0 | sdigest -0      # names on stdin
sdigest FILE... > sums && sdigest -k sums   # check later, like sha512sum -c
```

| level | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|---|---|---|---|
| coverage % | 0 | 0.01 | 0.03 | 0.1 | 0.3 | 1 | 3 | 10 | 30 | 100 |

Level 0 hashes the size plus the first and last 64 KiB. Level 9 hashes
everything.

```python
from sdigest import may_equal, sdigest

a = sdigest("a.flac", level=0)  # path
b = sdigest(open("b.flac", "rb"), level=0)  # or any seekable binary file object
may_equal(a, b)  # True, False, or None (not comparable)
```

```rust
let fp = sdigest::digest_path(path, sdigest::Coverage::from_level(0)?)?;
```

For Rust library use without the CLI dependencies, use
`default-features = false`.

## Documentation

* [`doc/design.md`](doc/design.md): motivation, design, decisions and
  rationale
* [`doc/sdigest.1`](doc/sdigest.1): man page (`man -l doc/sdigest.1`)

## Development

Tasks are [`just`](https://just.systems) recipes, and CI runs the same ones.
Install `just` with `uv tool install rust-just`, `cargo install just`, or
your package manager.

```sh
just                        # list recipes
just devready               # create .venv (builds the extension) and install git hooks
just cqa                    # lockfile, lint and format checks on all files
just cqa --from-ref main    # ... or only on files changed since main
just test                   # Rust and Python tests
just build                  # check packaging: crate, sdist, wheel
```

Releases: push a semver tag, e.g. `git tag 1.2.3 && git push --tags` (no
`v`; pre-releases look like `1.2.3-rc.1`). The tag is the only source of
truth for the version: `Cargo.toml` holds `0.0.0`, CI writes the tag into it
before building, and CI publishes the crate and the Python wheels using
trusted publishing (no stored tokens). Development builds describe
themselves relative to the latest tag, e.g. `0.2.1-dev.3+g4b551b3`.
