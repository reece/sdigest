//! The `sdigest` command line, shared by the Rust binary and the Python
//! package's `sdigest` script so both behave identically.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::medium;
use crate::{
    Coverage, DEFAULT_LEVEL, Fingerprint, LEVELS, digest_reader, open_seekable, planned_bytes,
};
use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{HumanBytes, ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;

/// Print or check sparse digests of seekable files.
///
/// Fingerprints are `sd1:<size>:<coverage>:<blake3>`. Different fingerprints
/// (at the same coverage) prove the content differs; equal fingerprints only
/// mean it may be identical. Coverage 0 hashes the size plus the first and
/// last 64 KiB; coverage 100 hashes everything.
///
/// Levels 0-9 select coverage on a roughly logarithmic scale:
/// 0, 0.01, 0.03, 0.1, 0.3, 1, 3, 10, 30, 100 percent.
///
/// With no FILE, names are read from stdin, one per line (or NUL-separated
/// with -0). With --check and no FILE, the fingerprint list is read from stdin.
#[derive(Parser)]
#[command(version = crate::PKG_VERSION)]
struct Args {
    /// Stringency level 0-9 [default: 5, i.e. 1% coverage]
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(0..LEVELS.len() as i64))]
    level: Option<u8>,

    /// Percent of content to sample (0-100), instead of a level
    #[arg(short, long, conflicts_with = "level")]
    coverage: Option<Coverage>,

    /// Read fingerprints from FILEs and check them (coverage is taken from each line)
    #[arg(short = 'k', long)]
    check: bool,

    /// File names on stdin are NUL-separated (e.g. from `find -print0`)
    #[arg(short = '0', long)]
    null: bool,

    /// Don't show progress or the summary
    #[arg(short, long)]
    quiet: bool,

    /// Threads, each reading one file at a time, or auto to choose from each
    /// file's storage and the coverage [default: auto]
    #[arg(short, long, value_name = "N", default_value = "auto", hide_default_value = true,
          value_parser = parse_threads)]
    threads: Threads,

    /// Files to fingerprint, or fingerprint lists with --check
    files: Vec<PathBuf>,
}

/// How many threads read files.
#[derive(Clone, Copy, Debug)]
enum Threads {
    Auto,
    Fixed(usize),
}

fn parse_threads(s: &str) -> Result<Threads, String> {
    match s {
        "auto" => Ok(Threads::Auto),
        _ => match s.parse::<usize>() {
            Ok(n) if n > 0 => Ok(Threads::Fixed(n)),
            _ => Err(format!("expected a positive number or auto, got {s:?}")),
        },
    }
}

/// Run the command line with `args` (including the program name) and return
/// the exit status. Usage errors, `--help` and `--version` exit the process
/// directly, as clap does. A closed output pipe ends the run quietly with
/// status 141.
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = Args::parse_from(args);
    let result = if args.check {
        check(&args)
    } else {
        print(&args)
    };
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        // The reader went away (`sdigest ... | head`): stop quietly, with the
        // status a shell reports for a process killed by SIGPIPE.
        Err(e)
            if e.root_cause()
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            141
        }
        Err(e) => {
            eprintln!("sdigest: {e:#}");
            1
        }
    }
}

/// Print a fingerprint per file. Returns false if any file failed.
fn print(args: &Args) -> Result<bool> {
    let paths = if args.files.is_empty() {
        names_from_stdin(args.null)?
    } else {
        args.files.clone()
    };
    let coverage = match (args.coverage, args.level) {
        (Some(c), _) => c,
        (None, level) => {
            Coverage::from_level(level.unwrap_or(DEFAULT_LEVEL)).map_err(anyhow::Error::msg)?
        }
    };
    let jobs: Vec<_> = paths.into_iter().map(|p| (p, coverage)).collect();
    let run = Run::execute(&jobs, args.quiet, args.threads);

    let mut out = BufWriter::new(io::stdout().lock());
    for ((path, _), result) in jobs.iter().zip(&run.results) {
        match result {
            Ok(fp) => {
                write!(out, "{fp}  ")?;
                write_path(&mut out, path)?;
                writeln!(out)?;
            }
            Err(e) => {
                out.flush()?;
                eprintln!("sdigest: {e:#}");
            }
        }
    }
    out.flush()?;
    run.summarize(args.quiet);
    Ok(run.errors() == 0)
}

/// Verify `<fingerprint>  <path>` lines, skipping malformed ones with a
/// warning. Returns false on any mismatch, error or malformed line.
fn check(args: &Args) -> Result<bool> {
    let mut jobs = Vec::new();
    let mut expected = Vec::new();
    let mut malformed = 0;
    let mut add = |name: &str, reader: &mut dyn BufRead| -> Result<()> {
        for (i, line) in reader.split(b'\n').enumerate() {
            match parse_line(&line.with_context(|| name.to_string())?) {
                Ok((fp, path)) => {
                    jobs.push((path, fp.coverage));
                    expected.push(fp);
                }
                Err(e) => {
                    eprintln!("sdigest: {name}:{}: {e:#}", i + 1);
                    malformed += 1;
                }
            }
        }
        Ok(())
    };
    if args.files.is_empty() {
        add("-", &mut stdin_or_bail()?.lock())?;
    }
    for list in &args.files {
        let name = list.display().to_string();
        add(
            &name,
            &mut BufReader::new(File::open(list).with_context(|| name.clone())?),
        )?;
    }

    let run = Run::execute(&jobs, args.quiet, args.threads);
    let mut out = BufWriter::new(io::stdout().lock());
    let mut failed = 0;
    for (((path, _), expected), result) in jobs.iter().zip(&expected).zip(&run.results) {
        match result {
            Ok(fp) if fp == expected => writeln!(out, "{}: OK", path.display())?,
            Ok(_) => {
                writeln!(out, "{}: FAILED", path.display())?;
                failed += 1;
            }
            Err(e) => {
                writeln!(out, "{}: FAILED open or read", path.display())?;
                out.flush()?;
                eprintln!("sdigest: {e:#}");
            }
        }
    }
    out.flush()?;
    if failed > 0 {
        eprintln!("sdigest: WARNING: {failed} computed fingerprint(s) did NOT match");
    }
    if run.errors() > 0 {
        eprintln!(
            "sdigest: WARNING: {} listed file(s) could not be read",
            run.errors()
        );
    }
    if malformed > 0 {
        eprintln!("sdigest: WARNING: {malformed} line(s) are improperly formatted");
    }
    run.summarize(args.quiet);
    Ok(failed == 0 && run.errors() == 0 && malformed == 0)
}

/// Parse a `<fingerprint>  <path>` line, ignoring a trailing CR. The path is taken as raw bytes, so
/// names that aren't valid UTF-8 round-trip on Unix.
fn parse_line(line: &[u8]) -> Result<(Fingerprint, PathBuf)> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let sep = line
        .windows(2)
        .position(|w| w == b"  ")
        .context("malformed line")?;
    let fp = std::str::from_utf8(&line[..sep])
        .ok()
        .context("malformed line")?
        .parse()
        .map_err(anyhow::Error::msg)?;
    Ok((fp, path_from_bytes(&line[sep + 2..])?))
}

/// Write a path as its raw bytes on Unix, so lists hold the real name.
#[cfg(unix)]
fn write_path(out: &mut impl Write, path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    out.write_all(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn write_path(out: &mut impl Write, path: &Path) -> io::Result<()> {
    write!(out, "{}", path.display())
}

fn stdin_or_bail() -> Result<io::Stdin> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        bail!("no files given (pass FILEs or pipe names to stdin)");
    }
    Ok(stdin)
}

fn names_from_stdin(null: bool) -> Result<Vec<PathBuf>> {
    let mut buf = Vec::new();
    stdin_or_bail()?.lock().read_to_end(&mut buf)?;
    let sep = if null { b'\0' } else { b'\n' };
    buf.split(|&b| b == sep)
        .filter(|name| !name.is_empty())
        .map(path_from_bytes)
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(name: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(name)))
}

#[cfg(not(unix))]
fn path_from_bytes(name: &[u8]) -> Result<PathBuf> {
    let name = std::str::from_utf8(name).context("file name on stdin is not UTF-8")?;
    Ok(PathBuf::from(name.trim_end_matches('\r')))
}

/// Fingerprints for a batch of (path, coverage) jobs, plus what it cost.
struct Run {
    results: Vec<Result<Fingerprint>>,
    coverages: BTreeSet<String>,
    /// How each group of files was scheduled, e.g. "16 (sparse, rotational, queue depth 30)".
    schedules: BTreeSet<String>,
    bytes_read: u64,
    elapsed: Duration,
}

impl Run {
    fn execute(jobs: &[(PathBuf, Coverage)], quiet: bool, policy: Threads) -> Run {
        let start = Instant::now();
        let planned: u64 = jobs
            .par_iter()
            .map(|(p, c)| {
                fs::metadata(p)
                    .map(|m| planned_bytes(m.len(), *c))
                    .unwrap_or(0)
            })
            .sum();
        let target = if quiet {
            ProgressDrawTarget::hidden()
        } else {
            ProgressDrawTarget::stderr()
        };
        let pb = ProgressBar::with_draw_target(Some(planned), target).with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] {wide_bar} {bytes}/{total_bytes} ({binary_bytes_per_sec}) {msg}",
            )
            .unwrap(),
        );
        let done = AtomicUsize::new(0);
        pb.set_message(format!("0/{} files", jobs.len()));

        let work = |i: usize| {
            let (path, coverage) = &jobs[i];
            let result = digest(path, *coverage, &pb);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            pb.set_message(format!("{n}/{} files", jobs.len()));
            (i, result)
        };

        // Devices are read concurrently, each with its own number of reads in
        // flight; within a device, groups needing different numbers (mixed
        // coverages in a --check list) run one after another.
        let schedule = schedule(jobs, policy);
        let work = &work;
        let mut indexed: Vec<(usize, Result<Fingerprint>)> = std::thread::scope(|s| {
            let handles: Vec<_> = schedule
                .values()
                .map(|groups| {
                    s.spawn(move || {
                        let mut out = Vec::new();
                        for ((n, _), idxs) in groups {
                            match rayon::ThreadPoolBuilder::new().num_threads(*n).build() {
                                Ok(pool) => out.extend(pool.install(|| {
                                    idxs.par_iter().map(|&i| work(i)).collect::<Vec<_>>()
                                })),
                                Err(_) => out.extend(idxs.iter().map(|&i| work(i))),
                            }
                        }
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("worker thread panicked"))
                .collect()
        });
        indexed.sort_by_key(|(i, _)| *i);
        let bytes_read = pb.position();
        pb.finish_and_clear();
        Run {
            results: indexed.into_iter().map(|(_, r)| r).collect(),
            coverages: jobs.iter().map(|(_, c)| c.to_string()).collect(),
            schedules: schedule
                .values()
                .flat_map(|g| g.keys().map(|(n, why)| format!("{n} ({why})")))
                .collect(),
            bytes_read,
            elapsed: start.elapsed(),
        }
    }

    fn errors(&self) -> usize {
        self.results.iter().filter(|r| r.is_err()).count()
    }

    fn summarize(&self, quiet: bool) {
        if quiet {
            return;
        }
        let ok: Vec<_> = self.results.iter().flatten().collect();
        let total: u64 = ok.iter().map(|fp| fp.size).sum();
        let secs = self.elapsed.as_secs_f64();
        let rate = if secs > 0.0 {
            HumanBytes((total as f64 / secs) as u64).to_string()
        } else {
            "∞".into()
        };
        let coverage = match self.coverages.len() {
            0 => "-".to_string(),
            1 => format!("{}%", self.coverages.first().unwrap()),
            _ => "mixed".to_string(),
        };
        let read_pct = if total > 0 {
            100.0 * self.bytes_read as f64 / total as f64
        } else {
            0.0
        };
        let mut summary = format!(
            "sdigest: {} files, {} in {secs:.2} s ({rate}/s); coverage {coverage}, read {} ({read_pct:.3}%)",
            ok.len(),
            HumanBytes(total),
            HumanBytes(self.bytes_read),
        );
        summary += &format!(
            "; threads {}",
            self.schedules
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        if self.errors() > 0 {
            summary += &format!("; {} errors", self.errors());
        }
        eprintln!("{summary}");
    }
}

/// Indices of jobs grouped by device, then by (threads, reason).
type Schedule = BTreeMap<Option<u64>, BTreeMap<(usize, String), Vec<usize>>>;

fn schedule(jobs: &[(PathBuf, Coverage)], policy: Threads) -> Schedule {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut media = HashMap::new();
    let mut schedule = Schedule::new();
    for (i, (path, coverage)) in jobs.iter().enumerate() {
        let dev = medium::device_of(path);
        let key = match policy {
            Threads::Fixed(n) => (n, "--threads".to_string()),
            Threads::Auto => {
                let m = dev.and_then(|d| *media.entry(d).or_insert_with(|| medium::detect(d)));
                medium::threads(m, *coverage, cpus)
            }
        };
        schedule
            .entry(dev)
            .or_default()
            .entry(key)
            .or_default()
            .push(i);
    }
    schedule
}

fn digest(path: &Path, coverage: Coverage, pb: &ProgressBar) -> Result<Fingerprint> {
    if path.as_os_str() == "-" {
        bail!("{}: stdin is not seekable", path.display());
    }
    let context = || path.display().to_string();
    let file = open_seekable(path).with_context(context)?;
    digest_reader(&mut Counted { inner: file, pb }, coverage).with_context(context)
}

/// Reports bytes read to the progress bar.
struct Counted<'a, R> {
    inner: R,
    pb: &'a ProgressBar,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pb.inc(n as u64);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counted<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}
