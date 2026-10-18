//! How many files to read at once, chosen from the storage each file is on.
//!
//! Benchmarks (issue #3) found no single good thread count:
//!
//! - Sparse levels are bound by seeks, and gain from many reads in flight on
//!   devices that can queue them (up to 4x on UAS hard disks, 3.8x on NVMe).
//! - Full reads on hard disks are fastest one file at a time; interleaving
//!   files only adds seeks.
//! - A queue depth of 1 (e.g. the old usb-storage driver) makes any
//!   parallelism slower.
//!
//! The policy changes behaviour only for storage it can identify; anything
//! else gets one file per CPU, as before.

use std::path::Path;

use crate::Coverage;

/// Sparse levels never use more than this many reads in flight. Beyond 16,
/// hard disks gained 7-20% more but sdigest would hold most of the disk's
/// queue, delaying everything else using it.
pub const SPARSE_MAX_THREADS: usize = 16;

/// Coverage (percent) from which a read counts as dense: transfer and hashing
/// dominate rather than seeks.
pub const DENSE_COVERAGE: f64 = 10.0;

/// What matters about a block device for scheduling reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Medium {
    pub rotational: bool,
    pub queue_depth: u32,
}

/// The number of threads (each reads one file at a time), and why.
pub fn threads(medium: Option<Medium>, coverage: Coverage, cpus: usize) -> (usize, String) {
    let dense = coverage.percent() >= DENSE_COVERAGE;
    let Some(m) = medium else {
        return (cpus, "unknown storage".into());
    };
    let qd = m.queue_depth.max(1) as usize;
    let kind = if m.rotational {
        "rotational"
    } else {
        "non-rotational"
    };
    let n = match (dense, m.rotational) {
        (false, _) => qd.min(SPARSE_MAX_THREADS),
        (true, true) => 1,
        (true, false) => qd.min(cpus),
    };
    let read = if dense { "dense" } else { "sparse" };
    (n.max(1), format!("{read}, {kind}, queue depth {qd}"))
}

/// Identify the storage of device `dev` (from `stat`), or None if it isn't a
/// local block device that can be identified (network filesystems, FUSE,
/// other OSes).
#[cfg(target_os = "linux")]
pub fn detect(dev: u64) -> Option<Medium> {
    use std::path::PathBuf;

    let (major, minor) = split_dev(dev);
    if major == 0 {
        return None; // anonymous: NFS, tmpfs, btrfs subvolumes, FUSE
    }
    let node = Path::new("/sys/dev/block").join(format!("{major}:{minor}"));
    let node = std::fs::canonicalize(node).ok()?;

    // Walk down through partitions, dm-crypt, LVM and md to the disks.
    let mut disks: Vec<PathBuf> = Vec::new();
    let mut todo = vec![node];
    while let Some(mut n) = todo.pop() {
        if n.join("partition").exists() {
            n = n.parent()?.to_path_buf();
        }
        let slaves: Vec<_> = std::fs::read_dir(n.join("slaves"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| std::fs::canonicalize(e.path()).ok())
                    .collect()
            })
            .unwrap_or_default();
        if slaves.is_empty() {
            disks.push(n);
        } else {
            todo.extend(slaves);
        }
        if disks.len() + todo.len() > 64 {
            return None;
        }
    }

    // The worst case across disks: rotational if any is, the shallowest queue.
    let read = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut medium: Option<Medium> = None;
    for d in &disks {
        let rotational = read(&d.join("queue/rotational"))? == "1";
        // SCSI (SATA, USB) disks report their command queue; NVMe has none,
        // so use the block layer's request queue.
        let queue_depth = read(&d.join("device/queue_depth"))
            .or_else(|| read(&d.join("queue/nr_requests")))?
            .parse()
            .ok()?;
        let m = Medium {
            rotational,
            queue_depth,
        };
        medium = Some(match medium {
            None => m,
            Some(a) => Medium {
                rotational: a.rotational || m.rotational,
                queue_depth: a.queue_depth.min(m.queue_depth),
            },
        });
    }
    medium
}

#[cfg(not(target_os = "linux"))]
pub fn detect(_dev: u64) -> Option<Medium> {
    None
}

/// Major and minor numbers of a Linux dev_t (glibc's encoding).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn split_dev(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// The device a file is on, as a key for grouping; None if it can't be read.
pub fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov(p: f64) -> Coverage {
        Coverage::new(p).unwrap()
    }

    const UAS_HDD: Medium = Medium {
        rotational: true,
        queue_depth: 30,
    };
    const USB_STORAGE_HDD: Medium = Medium {
        rotational: true,
        queue_depth: 1,
    };
    const NVME: Medium = Medium {
        rotational: false,
        queue_depth: 1023,
    };

    #[test]
    fn sparse_reads_fill_the_queue_up_to_a_cap() {
        assert_eq!(threads(Some(UAS_HDD), cov(0.0), 4).0, 16);
        assert_eq!(threads(Some(UAS_HDD), cov(1.0), 4).0, 16);
        assert_eq!(threads(Some(NVME), cov(0.0), 4).0, 16);
        let shallow = Medium {
            rotational: true,
            queue_depth: 8,
        };
        assert_eq!(threads(Some(shallow), cov(0.0), 4).0, 8);
    }

    #[test]
    fn dense_reads_are_serial_on_hard_disks_and_cpu_bound_on_ssds() {
        assert_eq!(threads(Some(UAS_HDD), cov(10.0), 4).0, 1);
        assert_eq!(threads(Some(UAS_HDD), cov(100.0), 4).0, 1);
        assert_eq!(threads(Some(NVME), cov(100.0), 4).0, 4);
    }

    #[test]
    fn queue_depth_one_is_always_serial() {
        for c in [0.0, 1.0, 100.0] {
            assert_eq!(threads(Some(USB_STORAGE_HDD), cov(c), 4).0, 1);
        }
    }

    #[test]
    fn unknown_storage_keeps_one_file_per_cpu() {
        for c in [0.0, 100.0] {
            assert_eq!(threads(None, cov(c), 6), (6, "unknown storage".to_string()));
        }
    }

    #[test]
    fn reasons_name_the_inputs() {
        let (_, why) = threads(Some(UAS_HDD), cov(0.0), 4);
        assert_eq!(why, "sparse, rotational, queue depth 30");
    }

    #[test]
    fn splits_linux_dev_numbers() {
        assert_eq!(split_dev(0x0830), (8, 48)); // sdd
        assert_eq!(split_dev(0x10301), (259, 1)); // nvme0n1p1 style
        assert_eq!(split_dev(0xfd00), (253, 0)); // dm-0
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detects_without_panicking() {
        if let Some(dev) = device_of(Path::new(env!("CARGO_MANIFEST_DIR"))) {
            let _ = detect(dev);
        }
    }
}
