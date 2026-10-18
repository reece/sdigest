//! Sparse digests: fast, tunable fingerprints of seekable content.
//!
//! A sparse digest hashes the content size followed by a deterministic sample
//! of blocks: always the first and last block, plus evenly spaced interior
//! blocks to reach the requested coverage. The sample depends only on the size
//! and the coverage, so digests are stable across machines and runs.
//!
//! Guarantee: two fingerprints with the same coverage that differ prove the
//! content differs. Equal fingerprints only indicate *possibly* identical
//! content; the false-positive rate falls as coverage rises.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::str::FromStr;

#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "python")]
mod python;

/// Algorithm tag. Bump if the plan, block size, or hash input ever changes.
pub const VERSION: &str = "sd1";

/// Package version, from the git tag (see build.rs).
pub const PKG_VERSION: &str = env!("SDIGEST_VERSION");

/// Sample block size. Part of the algorithm, so it is fixed, not tunable.
pub const BLOCK_SIZE: u64 = 64 * 1024;

/// Coverage percent for each level, a roughly logarithmic 1-3-10 scale.
/// Level 0 hashes only the size and the first and last blocks.
pub const LEVELS: [f64; 10] = [0.0, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0, 10.0, 30.0, 100.0];

pub const DEFAULT_LEVEL: u8 = 5;

const READ_BUF_SIZE: usize = 1 << 20;

/// Percent of content to sample, in `[0, 100]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coverage(f64);

// Never NaN, so equality is total.
impl Eq for Coverage {}

impl Coverage {
    pub fn new(percent: f64) -> Result<Self, String> {
        if !(0.0..=100.0).contains(&percent) {
            return Err(format!("coverage must be between 0 and 100, got {percent}"));
        }
        // Normalize -0.0 so the canonical string is always "0".
        Ok(Coverage(if percent == 0.0 { 0.0 } else { percent }))
    }

    pub fn from_level(level: u8) -> Result<Self, String> {
        let pct = LEVELS
            .get(level as usize)
            .ok_or_else(|| format!("level must be 0-9, got {level}"))?;
        Coverage::new(*pct)
    }

    pub fn percent(self) -> f64 {
        self.0
    }
}

impl Default for Coverage {
    fn default() -> Self {
        Coverage(LEVELS[DEFAULT_LEVEL as usize])
    }
}

/// Canonical form: shortest string that round-trips (e.g. "5", "0.01").
impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Coverage {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let pct: f64 = s.parse().map_err(|_| format!("invalid coverage: {s:?}"))?;
        Coverage::new(pct)
    }
}

/// Byte ranges to hash for content of `size` bytes, in ascending,
/// non-overlapping order.
pub fn plan(size: u64, coverage: Coverage) -> Plan {
    let b = BLOCK_SIZE;
    let total_blocks = size.div_ceil(b);
    let k = if coverage.0 >= 100.0 {
        total_blocks
    } else {
        ((size as f64 * coverage.0 / 100.0 / b as f64).ceil() as u64).clamp(2, total_blocks.max(2))
    };
    // Files no larger than the first and last blocks, or where the samples
    // would cover everything anyway, are read whole.
    let whole = size <= 2 * b || k * b >= size;
    Plan {
        size,
        k,
        i: 0,
        prev_end: 0,
        whole,
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    size: u64,
    k: u64,
    i: u64,
    prev_end: u64,
    whole: bool,
}

impl Iterator for Plan {
    type Item = Range<u64>;

    fn next(&mut self) -> Option<Range<u64>> {
        if self.whole {
            if self.i > 0 || self.size == 0 {
                return None;
            }
            self.i = 1;
            return Some(0..self.size);
        }
        if self.i >= self.k {
            return None;
        }
        let (b, i, k) = (BLOCK_SIZE, self.i, self.k);
        let start = if i == 0 {
            0
        } else if i == k - 1 {
            // The last block is anchored to EOF and may overlap the previous
            // interior block; skip the overlap so no byte is hashed twice.
            (self.size - b).max(self.prev_end)
        } else {
            // Not whole means k*b < size, so spacing exceeds one block and
            // aligned interior blocks never overlap one another.
            let point = i as u128 * (self.size - b) as u128 / (k - 1) as u128;
            (point as u64 / b) * b
        };
        let end = if i == k - 1 { self.size } else { start + b };
        self.i += 1;
        self.prev_end = end;
        Some(start..end)
    }
}

/// A sparse fingerprint, rendered as `sd1:<size>:<coverage>:<blake3 hex>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    pub coverage: Coverage,
    pub digest: [u8; 32],
}

impl Fingerprint {
    /// `Some(false)` if the content is proven different, `Some(true)` if it
    /// may be identical, `None` if the fingerprints aren't comparable.
    ///
    /// Fingerprints of the same size are comparable when their coverages
    /// yield the same plan (e.g. small files are read whole at any coverage),
    /// because then they hash the same bytes.
    pub fn may_equal(&self, other: &Fingerprint) -> Option<bool> {
        if self.size != other.size {
            Some(false)
        } else if self.coverage == other.coverage
            || plan(self.size, self.coverage).eq(plan(other.size, other.coverage))
        {
            Some(self.digest == other.digest)
        } else {
            None
        }
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{VERSION}:{}:{}:", self.size, self.coverage)?;
        for byte in self.digest {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Fingerprint {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("invalid fingerprint: {s:?}");
        let parts: Vec<&str> = s.split(':').collect();
        let [version, size, coverage, hex] = parts[..] else {
            return Err(bad());
        };
        if version != VERSION {
            return Err(format!("unsupported fingerprint version {version:?}"));
        }
        let size = size.parse().map_err(|_| bad())?;
        let coverage: Coverage = coverage.parse()?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let mut digest = [0u8; 32];
        for (i, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        Ok(Fingerprint {
            size,
            coverage,
            digest,
        })
    }
}

/// Fingerprint any seekable source. The source's position is not restored.
pub fn digest_reader<R: Read + Seek>(
    source: &mut R,
    coverage: Coverage,
) -> io::Result<Fingerprint> {
    let size = source.seek(SeekFrom::End(0))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(size.to_string().as_bytes());
    let mut buf = vec![0u8; READ_BUF_SIZE];
    let mut pos = size;
    for range in plan(size, coverage) {
        if pos != range.start {
            source.seek(SeekFrom::Start(range.start))?;
        }
        let mut remaining = range.end - range.start;
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            source.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            remaining -= n as u64;
        }
        pos = range.end;
    }
    Ok(Fingerprint {
        size,
        coverage,
        digest: *hasher.finalize().as_bytes(),
    })
}

/// Number of bytes hashed (excluding the size prefix) for `size` bytes of content.
pub fn planned_bytes(size: u64, coverage: Coverage) -> u64 {
    plan(size, coverage).map(|r| r.end - r.start).sum()
}

/// Open a file or other seekable path (e.g. a block device), rejecting directories.
pub fn open_seekable(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    if file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "is a directory",
        ));
    }
    Ok(file)
}

/// Fingerprint a file or other seekable path.
pub fn digest_path(path: &Path, coverage: Coverage) -> io::Result<Fingerprint> {
    digest_reader(&mut open_seekable(path)?, coverage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const B: u64 = BLOCK_SIZE;

    fn cov(p: f64) -> Coverage {
        Coverage::new(p).unwrap()
    }

    fn ranges(size: u64, p: f64) -> Vec<Range<u64>> {
        plan(size, cov(p)).collect()
    }

    fn bytes_read(size: u64, p: f64) -> u64 {
        planned_bytes(size, cov(p))
    }

    /// Deterministic pseudo-random content.
    fn content(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn fp(data: &[u8], p: f64) -> Fingerprint {
        digest_reader(&mut Cursor::new(data), cov(p)).unwrap()
    }

    #[test]
    fn coverage_validation_and_canonical_form() {
        assert!(Coverage::new(-1.0).is_err());
        assert!(Coverage::new(100.1).is_err());
        assert!(Coverage::new(f64::NAN).is_err());
        assert_eq!(cov(-0.0).to_string(), "0");
        assert_eq!(cov(5.0).to_string(), "5");
        assert_eq!("5.0".parse::<Coverage>().unwrap().to_string(), "5");
        assert_eq!(cov(0.01).to_string(), "0.01");
    }

    #[test]
    fn levels() {
        assert_eq!(Coverage::from_level(0).unwrap(), cov(0.0));
        assert_eq!(Coverage::from_level(9).unwrap(), cov(100.0));
        assert_eq!(Coverage::default(), cov(1.0));
        assert!(Coverage::from_level(10).is_err());
        assert!(LEVELS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn small_files_are_read_whole() {
        assert_eq!(ranges(0, 0.0), vec![]);
        assert_eq!(ranges(1, 0.0), vec![0..1]);
        assert_eq!(ranges(2 * B, 0.0), vec![0..2 * B]);
    }

    #[test]
    fn zero_coverage_reads_first_and_last_block() {
        assert_eq!(ranges(2 * B + 1, 0.0), vec![0..B, B + 1..2 * B + 1]);
        assert_eq!(ranges(1 << 30, 0.0), vec![0..B, (1 << 30) - B..1 << 30]);
    }

    #[test]
    fn full_coverage_reads_everything() {
        for size in [2 * B + 1, 10 * B, 10 * B + 7, 1 << 30] {
            assert_eq!(ranges(size, 100.0), vec![0..size]);
        }
    }

    #[test]
    fn plans_are_ordered_disjoint_in_bounds_and_aligned() {
        for size in [2 * B + 1, 3 * B, 100 * B + 3, 12_345_678_901] {
            for p in [0.0, 0.01, 1.0, 5.0, 33.3, 49.9, 50.0, 90.0, 99.9] {
                let rs = ranges(size, p);
                assert_eq!(rs.first().unwrap().start, 0);
                assert_eq!(rs.last().unwrap().end, size);
                for w in rs.windows(2) {
                    assert!(w[0].end <= w[1].start, "{size} {p}: {w:?}");
                }
                for r in &rs[..rs.len() - 1] {
                    assert!(r.start < r.end && r.start % B == 0, "{size} {p}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn bytes_read_tracks_coverage() {
        let size = 10_000 * B;
        let mut prev = 0;
        for p in [0.0, 0.1, 1.0, 5.0, 25.0, 50.0, 75.0, 100.0] {
            let n = bytes_read(size, p);
            assert!(n >= prev, "{p}: {n} < {prev}");
            assert!(n as f64 >= size as f64 * p / 100.0, "{p}: {n} below target");
            prev = n;
        }
        assert_eq!(bytes_read(size, 5.0), 500 * B);
    }

    #[test]
    fn full_coverage_is_blake3_of_size_then_content() {
        let data = content(5 * B as usize + 17, 1);
        let mut h = blake3::Hasher::new();
        h.update(data.len().to_string().as_bytes());
        h.update(&data);
        assert_eq!(fp(&data, 100.0).digest, *h.finalize().as_bytes());
    }

    #[test]
    fn identical_content_gives_identical_fingerprints() {
        let data = content(50 * B as usize + 3, 2);
        for p in [0.0, 5.0, 100.0] {
            assert_eq!(fp(&data, p), fp(&data.clone(), p));
        }
    }

    #[test]
    fn size_always_distinguishes() {
        let data = content(50 * B as usize, 3);
        assert_ne!(
            fp(&data, 0.0).digest,
            fp(&data[..data.len() - 1], 0.0).digest
        );
        assert_ne!(fp(b"", 0.0).digest, fp(b"\0", 0.0).digest);
    }

    #[test]
    fn changes_in_sampled_bytes_are_detected() {
        let size = 50 * B;
        let data = content(size as usize, 4);
        for p in [0.0, 5.0, 100.0] {
            for r in ranges(size, p) {
                for offset in [r.start, r.end - 1] {
                    let mut changed = data.clone();
                    changed[offset as usize] ^= 1;
                    assert_ne!(fp(&data, p), fp(&changed, p), "{p} {offset}");
                }
            }
        }
    }

    #[test]
    fn changes_in_unsampled_bytes_are_not_detected() {
        // Documents the trade-off: sparse digests can give false positives.
        let size = 50 * B;
        let mut data = content(size as usize, 5);
        let a = fp(&data, 0.0);
        data[(size / 2) as usize] ^= 1;
        assert_eq!(a, fp(&data, 0.0));
        assert_ne!(a.digest, fp(&data, 100.0).digest);
    }

    #[test]
    fn fingerprint_string_round_trips() {
        let f = fp(&content(3 * B as usize, 6), 2.5);
        let s = f.to_string();
        assert!(s.starts_with(&format!("sd1:{}:2.5:", 3 * B)), "{s}");
        assert_eq!(s.parse::<Fingerprint>().unwrap(), f);
        assert!("sd2:1:0:00".parse::<Fingerprint>().is_err());
        assert!("sd1:1:0:zz".parse::<Fingerprint>().is_err());
    }

    #[test]
    fn rejects_non_hex_digests_without_panicking() {
        // 64 bytes, but slicing at byte 2 would split the first '€'.
        let euro = format!("sd1:1:0:a{}", "€".repeat(21));
        assert!(euro.parse::<Fingerprint>().is_err());
        // u8::from_str_radix accepts a leading '+'.
        let plus = format!("sd1:1:0:{}", "+f".repeat(32));
        assert!(plus.parse::<Fingerprint>().is_err());
    }

    #[test]
    fn may_equal_semantics() {
        let data = content(50 * B as usize, 7);
        let mut other = data.clone();
        other[0] ^= 1;
        assert_eq!(fp(&data, 5.0).may_equal(&fp(&data, 5.0)), Some(true));
        assert_eq!(fp(&data, 5.0).may_equal(&fp(&other, 5.0)), Some(false));
        assert_eq!(fp(&data, 5.0).may_equal(&fp(&data, 1.0)), None);
        assert_eq!(fp(&data, 5.0).may_equal(&fp(&data[1..], 1.0)), Some(false));
    }

    #[test]
    fn may_equal_compares_across_coverages_with_the_same_plan() {
        // 50 blocks: levels 0-3 all read just the first and last block.
        let data = content(50 * B as usize, 8);
        let mut other = data.clone();
        other[0] ^= 1;
        assert_eq!(fp(&data, 0.0).may_equal(&fp(&data, 0.1)), Some(true));
        assert_eq!(fp(&data, 0.0).may_equal(&fp(&other, 0.1)), Some(false));
        // Small files are read whole at every coverage.
        assert_eq!(fp(b"abc", 0.0).may_equal(&fp(b"abc", 100.0)), Some(true));
        assert_eq!(fp(b"abc", 0.0).may_equal(&fp(b"abd", 100.0)), Some(false));
    }

    #[test]
    fn digests_are_stable() {
        // Golden values: if these change, the algorithm changed and VERSION
        // must be bumped.
        let data = content(100 * B as usize + 12_345, 42);
        let got: Vec<String> = [0.0, 5.0, 100.0]
            .iter()
            .map(|&p| fp(&data, p).to_string())
            .collect();
        assert_eq!(got, GOLDEN);
    }

    const GOLDEN: [&str; 3] = [
        "sd1:6565945:0:6f60b371e988a9da637abae1eb52a71904f839403054fc2e1866977f9894f202",
        "sd1:6565945:5:86b263d8cc32e84db1e49008cddba69e514cd9149f08d23c3398d62119f3cc27",
        "sd1:6565945:100:4803e28c67e151a634d66d7ec80a8b627858f046fd34a7d599937ca1e12e31ab",
    ];
}
