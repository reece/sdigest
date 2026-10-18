#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["duckdb>=1.1"]
# ///
"""sdbench: benchmark sdigest's I/O across storage media.

Results are append-only JSON Lines under bench/; see doc/benchmarking.md.

    sdbench gen random --seed 1 --n-files 100 --dist loguniform --min-mb 5 --max-mb 50 \\
        --data-dir DIR
    sdbench run --ref 0.2.1 --config bench/configs/firstpass.toml --data-dir DIR
    sdbench ls | report | compare A B | sql QUERY | db PATH | export PATH
"""

import argparse
import contextlib
import dataclasses
import hashlib
import json
import math
import os
import platform
import random
import re
import resource
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from datetime import UTC, datetime
from pathlib import Path

import tomllib

BENCH = Path(__file__).resolve().parent
REPO = BENCH.parent
CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "sdbench"
SCHEMA_VERSION = 1
# Directories whose committed files must never change (see the prek hook).
RECORD_DIRS = ["runs", "hosts", "devices", "datasets", "annotations"]


def log(msg):
    print(f"sdbench: {msg}", file=sys.stderr, flush=True)


def sh(*args, cwd=None, check=True):
    return subprocess.run(args, cwd=cwd, check=check, capture_output=True, text=True).stdout.strip()


def git(*args, check=True):
    return sh("git", *args, cwd=REPO, check=check)


def now_utc():
    return datetime.now(UTC)


def iso(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def write_json(path, obj):
    """Write in the format the pretty-format-json hook expects."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, indent=2) + "\n")


def fmt_num(x):
    """Canonical number text: 5.0 -> '5', 0.25 -> '0.25'. Never exponents."""
    s = f"{float(x):.6f}".rstrip("0").rstrip(".")
    return s or "0"


# ---------------------------------------------------------------- datasets
#
# A dataset is identified by its generator and *all* of its parameters.
# Every parameter is required and has one canonical form, so one spec has
# exactly one name. A change to a generator's output bumps its version.


@dataclasses.dataclass(frozen=True)
class RandomSpec:
    """Files of seeded pseudo-random content with sizes drawn from a distribution."""

    seed: int
    n_files: int
    dist: str  # uniform | loguniform
    min_mb: float  # MB = 10^6 bytes
    max_mb: float

    generator = "random"
    version = 1

    def __post_init__(self):
        if self.dist not in ("uniform", "loguniform"):
            raise ValueError(f"--dist must be uniform or loguniform, got {self.dist!r}")
        if not (0 < self.min_mb <= self.max_mb):
            raise ValueError("need 0 < --min-mb <= --max-mb")
        if self.n_files < 1:
            raise ValueError("--n-files must be >= 1")

    @classmethod
    def add_args(cls, p):
        p.add_argument("--seed", type=int, required=True)
        p.add_argument("--n-files", type=int, required=True)
        p.add_argument("--dist", choices=["uniform", "loguniform"], required=True)
        p.add_argument("--min-mb", type=float, required=True)
        p.add_argument("--max-mb", type=float, required=True)

    @classmethod
    def from_args(cls, a):
        return cls(a.seed, a.n_files, a.dist, a.min_mb, a.max_mb)

    @property
    def id(self):
        return (
            f"random-v{self.version}-s{self.seed}-n{self.n_files}-{self.dist}"
            f"-{fmt_num(self.min_mb)}-{fmt_num(self.max_mb)}mb"
        )

    @property
    def spec(self):
        return (
            f"random --seed {self.seed} --n-files {self.n_files} --dist {self.dist}"
            f" --min-mb {fmt_num(self.min_mb)} --max-mb {fmt_num(self.max_mb)}"
        )

    def params(self):
        return {
            "seed": self.seed,
            "n_files": self.n_files,
            "dist": self.dist,
            "min_mb": float(self.min_mb),
            "max_mb": float(self.max_mb),
        }

    def _rng(self, purpose):
        return random.Random(f"{self.generator}-v{self.version}:{self.seed}:{purpose}")

    def sizes(self):
        rng = self._rng("sizes")
        lo, hi = self.min_mb * 1e6, self.max_mb * 1e6
        if self.dist == "uniform":
            draw = lambda: rng.uniform(lo, hi)  # noqa: E731
        else:
            draw = lambda: math.exp(rng.uniform(math.log(lo), math.log(hi)))  # noqa: E731
        return [max(1, round(draw())) for _ in range(self.n_files)]

    def write_order(self):
        order = list(range(self.n_files))
        self._rng("order").shuffle(order)
        return order

    def chunks(self, i, size, chunk=4 << 20):
        """Content of file i: SHAKE-256 keyed by (spec, file, chunk). Incompressible."""
        for k, off in enumerate(range(0, size, chunk)):
            key = f"{self.generator}-v{self.version}:{self.seed}:{i}:{k}".encode()
            yield hashlib.shake_256(key).digest(min(chunk, size - off))


GENERATORS = {"random": RandomSpec}


def parse_spec(text):
    """Parse a dataset spec string, e.g. from a config file."""
    words = shlex.split(text)
    if not words or words[0] not in GENERATORS:
        raise ValueError(f"unknown generator in {text!r}; known: {', '.join(GENERATORS)}")
    cls = GENERATORS[words[0]]
    p = argparse.ArgumentParser(prog=words[0], exit_on_error=False)
    cls.add_args(p)
    return cls.from_args(p.parse_args(words[1:]))


def file_name(i):
    return f"f{i:06d}.bin"


def dataset_record(spec):
    sizes = spec.sizes()
    qs = sorted(sizes)

    def q(p):
        return qs[min(len(qs) - 1, int(p * len(qs)))]

    return {
        "schema_version": SCHEMA_VERSION,
        "id": spec.id,
        "generator": spec.generator,
        "generator_version": spec.version,
        "spec": spec.spec,
        "params": spec.params(),
        "n_files": len(sizes),
        "total_bytes": sum(sizes),
        "size_bytes": {
            "min": qs[0],
            "p25": q(0.25),
            "median": q(0.5),
            "p75": q(0.75),
            "max": qs[-1],
        },
    }


def ensure_dataset_record(spec):
    rec = dataset_record(spec)
    path = BENCH / "datasets" / f"{spec.id}.json"
    if path.exists():
        if json.loads(path.read_text()) != rec:
            raise SystemExit(
                f"{path} differs from what {spec.spec!r} generates; bump the generator version"
            )
    else:
        write_json(path, rec)
    return rec


def materialize(spec, data_dir):
    """Write the dataset under data_dir (or reuse it) and return its file paths."""
    ensure_dataset_record(spec)
    sizes = spec.sizes()
    root = Path(data_dir) / spec.id
    paths = [root / file_name(i) for i in range(spec.n_files)]
    manifest = root / "manifest.json"
    if manifest.exists():
        m = json.loads(manifest.read_text())
        if m.get("spec") == spec.spec and all(
            p.stat().st_size == s for p, s in zip(paths, sizes, strict=True)
        ):
            return paths
        raise SystemExit(f"{root} exists but doesn't match {spec.spec!r}; remove it")
    part = root.with_name(root.name + ".partial")
    shutil.rmtree(part, ignore_errors=True)
    part.mkdir(parents=True)
    total, t0 = sum(sizes), time.monotonic()
    log(f"writing {spec.id}: {spec.n_files} files, {total / 2**30:.2f} GiB to {root}")
    done = 0
    for n, i in enumerate(spec.write_order(), 1):
        fd = os.open(part / file_name(i), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        try:
            for buf in spec.chunks(i, sizes[i]):
                os.write(fd, buf)
            os.fsync(fd)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
        done += sizes[i]
        if n % 100 == 0 or n == spec.n_files:
            rate = done / 2**20 / (time.monotonic() - t0)
            log(f"  {n}/{spec.n_files} files, {rate:.0f} MiB/s")
    write_json(part / "manifest.json", {"spec": spec.spec, "id": spec.id, "sizes": sizes})
    part.rename(root)
    return paths


# ---------------------------------------------------------------- hosts and devices
#
# Identity holds only what plausibly changes performance, so routine updates
# (a patch-level kernel, a reboot) keep the same ID. Details are recorded per
# session for forensics but never split comparisons.


def read(path, default=None):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return default


def host_identity():
    cpu = read("/proc/cpuinfo", "")
    model = re.search(r"^model name\s*:\s*(.+)$", cpu, re.M)
    mem_kib = int(re.search(r"^MemTotal:\s+(\d+)", read("/proc/meminfo"), re.M).group(1))
    osr = dict(re.findall(r'^(\w+)="?([^"\n]*)"?$', read("/etc/os-release", ""), re.M))
    return {
        "hostname": platform.node().split(".")[0],
        "cpu_model": model.group(1).strip() if model else platform.processor(),
        "cpu_threads": os.cpu_count(),
        "mem_gib": round(mem_kib / 2**20),
        "kernel": ".".join(platform.release().split(".")[:2]),
        "os": f"{osr.get('ID', '?')} {osr.get('VERSION_ID', '?')}",
    }


def host_details():
    return {
        "kernel_release": platform.release(),
        "governor": read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "rustc": sh("rustc", "--version", check=False),
        "python": platform.python_version(),
    }


def assign_id(kind, prefix, identity):
    """Find the record in bench/<kind>/ with this identity, or create the next one."""
    d = BENCH / kind
    d.mkdir(parents=True, exist_ok=True)
    taken = []
    for f in sorted(d.glob(f"{prefix}-[0-9][0-9][0-9][0-9].json")):
        rec = json.loads(f.read_text())
        if rec["identity"] == identity:
            return rec["id"]
        taken.append(int(f.stem.rsplit("-", 1)[1]))
    new = f"{prefix}-{max(taken, default=0) + 1:04d}"
    write_json(
        d / f"{new}.json", {"schema_version": SCHEMA_VERSION, "id": new, "identity": identity}
    )
    return new


def lsblk_chain(source):
    """Device layers from the mounted device down to the disk(s)."""
    out = json.loads(sh("lsblk", "-J", "-s", "-o", "NAME,TYPE,TRAN,MODEL,ROTA,SIZE", source))
    node, layers = out["blockdevices"][0], []
    while True:
        layers.append(node)
        kids = node.get("children") or []
        if not kids:
            return layers
        node = kids[0]


def usb_link_mbps(sysdev):
    p = Path(sysdev).resolve()
    while p != p.parent:
        if (p / "speed").exists() and (p / "idVendor").exists():
            return int(float(read(p / "speed")))
        p = p.parent
    return None


def device_info(data_dir):
    source, fstype = sh("findmnt", "-n", "-o", "SOURCE,FSTYPE", "-T", str(data_dir)).split()
    layers = lsblk_chain(source)
    disk = layers[-1]
    name = disk["name"]
    sysb = Path("/sys/block") / name
    sched = re.search(r"\[(\S+)\]", read(sysb / "queue/scheduler", ""))
    udev = dict(
        line.split("=", 1)
        for line in sh("udevadm", "info", "-q", "property", "-n", f"/dev/{name}").splitlines()
    )
    tran = disk.get("tran") or ""
    link = None
    if tran == "usb":
        mbps = usb_link_mbps(sysb / "device")
        link = f"{mbps} Mb/s" if mbps else None
    elif tran == "nvme":
        speed = read(sysb / "device/device/current_link_speed")
        width = read(sysb / "device/device/current_link_width")
        link = f"{speed} x{width}" if speed else None
    model = (disk.get("model") or "unknown").strip()
    identity = {
        "model": model,
        "transport": tran,
        "usb_driver": udev.get("ID_USB_DRIVER"),
        "link": link,
        "rotational": disk.get("rota") in (True, "1", 1),
        "queue_depth": int(read(sysb / "device/queue_depth", "0")) or None,
        "scheduler": sched.group(1) if sched else None,
        "layers": [n["type"] for n in layers],
        "fstype": fstype,
        "size": disk.get("size"),
    }
    slug = re.sub(r"[^a-z0-9]+", "-", model.lower()).strip("-")[:24].strip("-")
    return assign_id("devices", slug, identity), identity, name


# ---------------------------------------------------------------- building sdigest


@dataclasses.dataclass
class Build:
    binary: Path
    commit: str
    describe: str  # git describe --tags --long, plus -dirty.<hash>
    version: str  # the tag if exact and clean, else describe
    dirty: bool
    release: bool  # results go to runs/ (else runs-dev/)


def build_sdigest(ref):
    """Build sdigest's release binary at ref (a tag or commit, or WORKTREE)."""
    worktree = ref == "WORKTREE"
    commit = git("rev-parse", "HEAD" if worktree else f"{ref}^{{commit}}")
    describe = git("describe", "--tags", "--long", "--match", "[0-9]*", commit)
    dirty_hash = None
    if worktree and git("status", "--porcelain", "--untracked-files=no", "--", ".", ":!bench"):
        diff = subprocess.run(
            ["git", "diff", "HEAD", "--", ".", ":!bench"], cwd=REPO, capture_output=True, check=True
        ).stdout
        dirty_hash = hashlib.sha256(diff).hexdigest()[:4]
        describe += f"-dirty.{dirty_hash}"
    tag, distance = re.match(r"(.+)-(\d+)-g[0-9a-f]+", describe).groups()
    release = distance == "0" and dirty_hash is None
    version = tag if release else describe

    key = commit + (f"-dirty.{dirty_hash}" if dirty_hash else "")
    binary = CACHE / "bin" / key / "sdigest"
    if dirty_hash or not binary.exists():
        env = {**os.environ, "CARGO_TARGET_DIR": str(CACHE / "target")}
        cmd = ["cargo", "build", "--release", "--locked", "--bin", "sdigest"]
        if worktree:
            src = REPO
            ctx = contextlib.nullcontext()
        else:
            tmp = tempfile.mkdtemp(prefix="sdbench-")
            git("worktree", "add", "--detach", tmp, commit)
            src = Path(tmp)
            ctx = contextlib.ExitStack()
            ctx.callback(lambda: git("worktree", "remove", "--force", tmp, check=False))
        with ctx:
            log(f"building sdigest {describe}")
            subprocess.run(cmd, cwd=src, env=env, check=True, stdout=subprocess.DEVNULL)
            binary.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(CACHE / "target/release/sdigest", binary)
    return Build(binary, commit, describe, version, dirty_hash is not None, release)


# ---------------------------------------------------------------- measuring


def diskstats(names):
    """Sum of (read ios, read bytes, write ios) over the named block devices."""
    tot = [0, 0, 0]
    for line in read("/proc/diskstats").splitlines():
        f = line.split()
        if f[2] in names:
            tot[0] += int(f[3])
            tot[1] += int(f[5]) * 512
            tot[2] += int(f[7])
    return tot


def wait_idle(disk, rotational, window=5.0, patience=1800):
    """Wait until the disk does no reads for `window` seconds. Returns reads seen at start."""
    # Only foreign reads compete with a benchmark; background writes (journals,
    # atime) are routine. An SSD tolerates a trickle; a hard disk gets none.
    limit = 0 if rotational else 20
    deadline = time.monotonic() + patience
    first = None
    while True:
        a = diskstats({disk})
        time.sleep(window)
        b = diskstats({disk})
        reads = b[0] - a[0]
        first = reads if first is None else first
        if reads <= limit:
            return first
        if time.monotonic() > deadline:
            raise SystemExit(f"/dev/{disk} not idle after {patience} s ({reads} reads/{window} s)")
        log(f"/dev/{disk} busy ({reads} reads in {window:.0f} s); waiting")
        time.sleep(25)


def evict(paths):
    for p in paths:
        fd = os.open(p, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


def cached_bytes(paths, batch=500):
    total = 0
    for i in range(0, len(paths), batch):
        out = sh(
            "fincore",
            "--bytes",
            "--noheadings",
            "--raw",
            "-o",
            "RES",
            *map(str, paths[i : i + batch]),
        )
        total += sum(int(x) for x in out.split())
    return total


UNITS = {"B": 1, "KiB": 2**10, "MiB": 2**20, "GiB": 2**30, "TiB": 2**40}


def parse_read_bytes(stderr):
    """Bytes requested, from sdigest's summary line (3 significant digits)."""
    m = re.search(r"read ([\d.]+) (\w+) \(", stderr)
    return round(float(m.group(1)) * UNITS[m.group(2)]) if m else None


# ---------------------------------------------------------------- run


def session_order(levels, threads, reps):
    """(rep, level, threads) in alternating order so drift favours no setting."""
    configs = [(lv, t) for lv in levels for t in threads]
    for rep in range(reps):
        seq = configs if rep % 2 == 0 else configs[::-1]
        for lv, t in seq:
            yield rep, lv, t


def cmd_run(a):
    cfg = tomllib.loads(Path(a.config).read_text())
    build = build_sdigest(a.ref)
    host = assign_id("hosts", host_identity()["hostname"], host_identity())
    device, dev_identity, disk = device_info(a.data_dir)
    rotational = dev_identity["rotational"]
    runs_root = BENCH / ("runs" if build.release else "runs-dev")
    harness = git("rev-parse", "HEAD")
    harness_dirty = bool(git("status", "--porcelain", "--", "bench/sdbench.py"))
    log(
        f"sdigest {build.version} ({'runs' if build.release else 'runs-dev'}), host {host}, "
        f"device {device}"
    )

    for sweep in cfg["sweep"]:
        spec = parse_spec(sweep["dataset"])
        paths = materialize(spec, a.data_dir)
        sizes = [p.stat().st_size for p in paths]
        names = b"".join(bytes(p) + b"\0" for p in paths)
        cache = sweep.get("cache", "cold")
        levels, threads = sweep["levels"], sweep.get("threads", cfg.get("threads"))
        reps = sweep.get("reps", cfg.get("reps", 3))
        if cache not in ("cold", "warm"):
            raise SystemExit(f"cache must be cold or warm, got {cache!r}")
        if cache == "warm" and sum(sizes) > 0.5 * os.sysconf("SC_PHYS_PAGES") * os.sysconf(
            "SC_PAGE_SIZE"
        ):
            raise SystemExit(f"{spec.id} is too big to stay cached; use a smaller dataset")

        started = now_utc()
        session = uuid.uuid4().hex[:8]
        out_dir = runs_root / f"version={build.version}" / f"host={host}" / f"dataset={spec.id}"
        stem = f"{started.strftime('%Y-%m-%dT%H-%M-%SZ')}_{device}_{session}"
        out_dir.mkdir(parents=True, exist_ok=True)
        write_json(
            out_dir / f"{stem}.session.json",
            {
                "schema_version": SCHEMA_VERSION,
                "session_id": session,
                "started": iso(started),
                "label": a.label,
                "argv": sys.argv[1:],
                "sweep": sweep,
                "harness_commit": harness,
                "harness_dirty": harness_dirty,
                "sdigest_commit": build.commit,
                "sdigest_describe": build.describe,
                "host": host,
                "host_details": host_details(),
                "device": device,
                "device_identity": dev_identity,
                "data_dir": str(a.data_dir),
            },
        )
        order = list(session_order(levels, threads, reps))
        log(f"{spec.id}: {len(order)} runs ({cache} cache)")
        if cache == "warm":
            subprocess.run(
                [build.binary, "-q", "-0", "-l", "9"],
                input=names,
                stdout=subprocess.DEVNULL,
                check=True,
            )
        with open(out_dir / f"{stem}.jsonl", "a") as out:
            for idx, (rep, level, nthreads) in enumerate(order):
                rec = run_one(build, paths, sizes, names, level, nthreads, cache, disk, rotational)
                rec = {
                    "schema_version": SCHEMA_VERSION,
                    "run_id": uuid.uuid4().hex,
                    "session_id": session,
                    "version": build.version,
                    "sdigest_commit": build.commit,
                    "sdigest_describe": build.describe,
                    "sdigest_dirty": build.dirty,
                    "host": host,
                    "device": device,
                    "dataset": spec.id,
                    "n_files": len(paths),
                    "total_bytes": sum(sizes),
                    "level": level,
                    "threads": nthreads,
                    "cache": cache,
                    "rep": rep,
                    "order": idx,
                    **rec,
                    "label": a.label,
                }
                out.write(json.dumps(rec) + "\n")
                out.flush()
                log(
                    f"  [{idx + 1}/{len(order)}] level {level} threads {nthreads:>2} "
                    f"rep {rep}: {rec['elapsed_s']:.2f} s"
                    + (" (foreign I/O)" if rec["foreign_io"] else "")
                )


def run_one(build, paths, sizes, names, level, threads, cache, disk, rotational):
    if cache == "cold":
        wait_idle(disk, rotational)
        evict(paths)
    residual = cached_bytes(paths)  # 0 for cold runs, the whole dataset for warm ones
    before_disk = diskstats({disk})
    before_ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    timestamp = now_utc()
    t0 = time.monotonic()
    r = subprocess.run(
        [build.binary, "-0", "-l", str(level)],
        input=names,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        env={**os.environ, "RAYON_NUM_THREADS": str(threads)},
    )
    elapsed = time.monotonic() - t0
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    after_disk = diskstats({disk})
    stderr = r.stderr.decode(errors="replace")
    requested = parse_read_bytes(stderr)
    io_read = (ru.ru_inblock - before_ru.ru_inblock) * 512
    dev_read = after_disk[1] - before_disk[1]
    reads = after_disk[0] - before_disk[0]
    total = sum(sizes)
    return {
        "timestamp": iso(timestamp),
        "elapsed_s": round(elapsed, 4),
        "exit_status": r.returncode,
        "requested_bytes": requested,
        "io_read_bytes": io_read,
        "device_read_bytes": dev_read,
        "device_read_ios": reads,
        "rate_effective_mib_s": round(total / 2**20 / elapsed, 2),
        "rate_read_mib_s": round(dev_read / 2**20 / elapsed, 2),
        "device_reads_per_s": round(reads / elapsed, 1),
        "cpu_user_s": round(ru.ru_utime - before_ru.ru_utime, 3),
        "cpu_sys_s": round(ru.ru_stime - before_ru.ru_stime, 3),
        "cache_residual_bytes": residual,
        # The disk read noticeably more than sdigest did: someone else was reading.
        "foreign_io": dev_read > io_read * 1.05 + 4 * 2**20,
        "stderr_tail": stderr.strip().splitlines()[-1][:300] if stderr.strip() else "",
    }


def cmd_gen(a):
    spec = GENERATORS[a.generator].from_args(a)
    paths = materialize(spec, a.data_dir)
    print(spec.id)
    log(f"{len(paths)} files in {Path(a.data_dir) / spec.id}")


# ---------------------------------------------------------------- analysis


def connect(dev=False, path=":memory:"):
    """DuckDB with the raw_* tables loaded and queries/views.sql applied."""
    import duckdb

    con = duckdb.connect(str(path))

    def load(name, files, empty_schema=None):
        files = sorted(str(f) for f in files)
        if files:
            lst = ", ".join(f"'{f}'" for f in files)
            con.execute(
                f"CREATE OR REPLACE TABLE {name} AS "
                f"SELECT * FROM read_json([{lst}], union_by_name = true)"
            )
        elif empty_schema:
            con.execute(f"CREATE OR REPLACE TABLE {name} ({empty_schema})")
        else:
            raise SystemExit(f"no records for {name} yet")

    roots = [BENCH / "runs"] + ([BENCH / "runs-dev"] if dev else [])
    load("raw_runs", [f for r in roots for f in r.glob("**/*.jsonl")])
    load("raw_sessions", [f for r in roots for f in r.glob("**/*.session.json")])
    load(
        "raw_annotations",
        (BENCH / "annotations").glob("*.jsonl"),
        "run_id VARCHAR, action VARCHAR, reason VARCHAR, timestamp VARCHAR",
    )
    for kind in ("hosts", "devices", "datasets"):
        load(f"raw_{kind}", (BENCH / kind).glob("*.json"))
    con.execute((BENCH / "queries/views.sql").read_text())
    return con


def table(con, query, params=None):
    rel = con.execute(query, params or [])
    cols = [d[0] for d in rel.description]
    rows = rel.fetchall()
    out = ["| " + " | ".join(cols) + " |", "|" + "---|" * len(cols)]
    for r in rows:
        out.append("| " + " | ".join("" if v is None else str(v) for v in r) + " |")
    return "\n".join(out)


def cmd_ls(a):
    con = connect(a.dev)
    print(table(con, "SELECT * FROM sessions ORDER BY started"))


def cmd_report(a):
    con = connect(a.dev)
    where, params = [], []
    for col in ("version", "host", "device", "dataset"):
        if getattr(a, col):
            where.append(f"{col} = ?")
            params.append(getattr(a, col))
    if a.level is not None:
        where.append("level = ?")
        params.append(a.level)
    w = ("WHERE " + " AND ".join(where)) if where else ""
    groups = con.execute(
        f"SELECT DISTINCT version, host, device, dataset, level, cache FROM summary {w} "
        "ORDER BY ALL",
        params,
    ).fetchall()
    for version, host, device, dataset, level, cache in groups:
        print(f"\n### {device}, level {level}, {cache} cache\n")
        print(f"sdigest {version}, host {host}, dataset `{dataset}`\n")
        rate = (
            "median_effective_mib_s"
            if cache == "warm"
            else "median_reads_per_s"
            if level < 9
            else "median_read_mib_s"
        )
        print(
            table(
                con,
                f"""
            SELECT threads, n, median_s, min_s, max_s, speedup, {rate}
            FROM summary
            WHERE version=? AND host=? AND device=? AND dataset=? AND level=? AND cache=?
            ORDER BY threads""",
                [version, host, device, dataset, level, cache],
            )
        )


def cmd_compare(a):
    con = connect(dev=True)
    print(
        table(
            con,
            """
        SELECT a.host, a.device, a.dataset, a.level, a.cache, a.threads,
               a.median_s AS a_s, b.median_s AS b_s, round(b.median_s / a.median_s, 3) AS ratio
        FROM summary a JOIN summary b USING (host, device, dataset, level, cache, threads)
        WHERE a.version = ? AND b.version = ?
        ORDER BY ALL""",
            [a.a, a.b],
        )
    )


def cmd_db(a):
    Path(a.path).unlink(missing_ok=True)
    connect(a.dev, a.path).close()
    log(f"wrote {a.path}; open with: duckdb {a.path}  (or duckdb -ui {a.path})")


def cmd_export(a):
    """Write the views a static page needs as one JSON file."""
    con = connect(a.dev)

    def rows(query):
        rel = con.execute(query)
        cols = [d[0] for d in rel.description]
        return [dict(zip(cols, r, strict=True)) for r in rel.fetchall()]

    data = {
        "generated": iso(now_utc()),
        "summary": rows("SELECT * FROM summary ORDER BY ALL"),
        "runs": rows("""SELECT version, host, device, dataset, level, cache, threads, rep,
                               elapsed_s, device_reads_per_s, rate_read_mib_s
                        FROM good_runs ORDER BY ALL"""),
        "devices": rows("SELECT * FROM devices ORDER BY device"),
        "datasets": rows("SELECT * FROM datasets ORDER BY dataset"),
        "hosts": rows("SELECT * FROM hosts ORDER BY host"),
    }
    Path(a.path).write_text(json.dumps(data, default=str) + "\n")
    log(f"wrote {a.path}: {len(data['runs'])} runs")


def cmd_sql(a):
    print(table(connect(a.dev), a.query))


def main():
    p = argparse.ArgumentParser(
        prog="sdbench", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("gen", help="write a synthetic dataset")
    gsub = g.add_subparsers(dest="generator", required=True)
    for name, cls in GENERATORS.items():
        gp = gsub.add_parser(name, help=cls.__doc__)
        cls.add_args(gp)
        gp.add_argument("--data-dir", required=True)
    g.set_defaults(func=cmd_gen)

    r = sub.add_parser("run", help="run a benchmark config")
    r.add_argument("--ref", required=True, help="release tag, commit, or WORKTREE")
    r.add_argument("--config", required=True)
    r.add_argument("--data-dir", required=True, type=Path)
    r.add_argument("--label")
    r.set_defaults(func=cmd_run)

    for name, fn, h in [("ls", cmd_ls, "list sessions"), ("report", cmd_report, "tables")]:
        s = sub.add_parser(name, help=h)
        s.add_argument("--dev", action="store_true", help="include runs-dev/")
        if name == "report":
            for col in ("version", "host", "device", "dataset"):
                s.add_argument(f"--{col}")
            s.add_argument("--level", type=int)
        s.set_defaults(func=fn)

    c = sub.add_parser("compare", help="compare two sdigest versions")
    c.add_argument("a")
    c.add_argument("b")
    c.set_defaults(func=cmd_compare)

    q = sub.add_parser("sql", help="run a query against the views")
    q.add_argument("query")
    q.add_argument("--dev", action="store_true")
    q.set_defaults(func=cmd_sql)

    e = sub.add_parser("export", help="write results as JSON for the results page")
    e.add_argument("path")
    e.add_argument("--dev", action="store_true")
    e.set_defaults(func=cmd_export)

    d = sub.add_parser("db", help="write all results to a DuckDB file for other tools")
    d.add_argument("path")
    d.add_argument("--dev", action="store_true")
    d.set_defaults(func=cmd_db)

    a = p.parse_args()
    a.func(a)


if __name__ == "__main__":
    main()
