//! Python bindings: `sdigest.sdigest(source, level=None, *, coverage=None)`.

use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::{Coverage, DEFAULT_LEVEL, Fingerprint, digest_path, digest_reader};

/// Adapts a Python binary file-like object (`read`/`seek`) to `Read + Seek`.
/// The first Python exception is kept so it can be re-raised unchanged.
struct PyFileLike<'py> {
    obj: Bound<'py, PyAny>,
    err: Option<PyErr>,
}

impl PyFileLike<'_> {
    fn stash(&mut self, e: PyErr) -> io::Error {
        let msg = e.to_string();
        self.err.get_or_insert(e);
        io::Error::other(msg)
    }
}

impl Read for PyFileLike<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let data = self
            .obj
            .call_method1("read", (buf.len(),))
            .map_err(|e| self.stash(e))?;
        let data = data
            .cast::<PyBytes>()
            .map_err(|e| self.stash(e.into()))?
            .as_bytes();
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

impl Seek for PyFileLike<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(n) => (n as i128, 0),
            SeekFrom::Current(n) => (n as i128, 1),
            SeekFrom::End(n) => (n as i128, 2),
        };
        let r = self
            .obj
            .call_method1("seek", (offset, whence))
            .and_then(|r| r.extract());
        r.map_err(|e| self.stash(e))
    }
}

/// Sparse digest of a path (str or os.PathLike) or a seekable binary file
/// object, as "sd1:<size>:<coverage>:<blake3 hex>". Give either a level
/// (0-9, default 5) or a coverage percent (0-100). File objects are read
/// through their read()/seek() methods and left at an unspecified position.
#[pyfunction]
#[pyo3(signature = (source, level = None, *, coverage = None))]
fn sdigest(
    py: Python<'_>,
    source: &Bound<'_, PyAny>,
    level: Option<i64>,
    coverage: Option<f64>,
) -> PyResult<String> {
    let coverage = match (level, coverage) {
        (Some(_), Some(_)) => {
            return Err(PyValueError::new_err("give level or coverage, not both"));
        }
        (None, Some(c)) => Coverage::new(c),
        (None, None) => Coverage::from_level(DEFAULT_LEVEL),
        (Some(level), None) => u8::try_from(level)
            .map_err(|_| format!("level must be 0-9, got {level}"))
            .and_then(Coverage::from_level),
    }
    .map_err(PyValueError::new_err)?;
    if let Ok(path) = source.extract::<PathBuf>() {
        let fp = py.detach(|| digest_path(&path, coverage))?;
        return Ok(fp.to_string());
    }
    let mut file = PyFileLike {
        obj: source.clone(),
        err: None,
    };
    match digest_reader(&mut file, coverage) {
        Ok(fp) => Ok(fp.to_string()),
        Err(e) => Err(file.err.take().unwrap_or_else(|| e.into())),
    }
}

/// False if two fingerprints prove the content differs, True if it may be
/// identical, None if they aren't comparable (same size, and the two
/// coverages sample different blocks).
#[pyfunction]
fn may_equal(a: &str, b: &str) -> PyResult<Option<bool>> {
    let parse = |s: &str| s.parse::<Fingerprint>().map_err(PyValueError::new_err);
    Ok(parse(a)?.may_equal(&parse(b)?))
}

/// Entry point for the `sdigest` script: the same command line as the Rust
/// binary. Returns the exit status.
#[pyfunction]
fn _main(py: Python<'_>) -> PyResult<u8> {
    // Python defers SIGINT until control returns to the interpreter, which
    // would make Ctrl-C ineffective during a long run. Restore the default.
    let signal = py.import("signal")?;
    signal.call_method1(
        "signal",
        (signal.getattr("SIGINT")?, signal.getattr("SIG_DFL")?),
    )?;
    let argv: Vec<OsString> = py.import("sys")?.getattr("argv")?.extract()?;
    Ok(py.detach(|| crate::cli::run(argv)))
}

#[pymodule]
#[pyo3(name = "sdigest")]
fn sdigest_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sdigest, m)?)?;
    m.add_function(wrap_pyfunction!(may_equal, m)?)?;
    m.add_function(wrap_pyfunction!(_main, m)?)?;
    m.add("__version__", crate::PKG_VERSION)?;
    Ok(())
}
