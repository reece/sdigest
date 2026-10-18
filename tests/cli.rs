#![cfg(feature = "cli")]

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn sdigest(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sdigest"))
        .args(args)
        .output()
        .unwrap()
}

fn sdigest_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sdigest"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

#[test]
fn prints_sha512sum_style_lines() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"hello").unwrap();
    let a = a.to_str().unwrap();

    let o = sdigest(&["-c", "5", a]);
    assert!(o.status.success());
    let line = stdout(&o);
    let (fp, path) = line.trim_end().split_once("  ").unwrap();
    assert_eq!(path, a);
    assert!(fp.starts_with("sd1:5:5:"), "{fp}");
}

#[test]
fn check_detects_changes() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let sums = dir.path().join("sums");
    fs::write(&a, b"hello").unwrap();
    let a = a.to_str().unwrap();
    let sums = sums.to_str().unwrap();

    fs::write(sums, stdout(&sdigest(&["--coverage", "0", a]))).unwrap();
    let o = sdigest(&["--check", sums]);
    assert!(o.status.success());
    assert_eq!(stdout(&o), format!("{a}: OK\n"));

    fs::write(a, b"jello").unwrap();
    let o = sdigest(&["--check", sums]);
    assert!(!o.status.success());
    assert_eq!(stdout(&o), format!("{a}: FAILED\n"));
}

#[test]
fn rejects_stdin_and_directories_but_continues() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"x").unwrap();
    let d = dir.path().to_str().unwrap();

    let o = sdigest(&["-", d, a.to_str().unwrap()]);
    assert!(!o.status.success());
    assert_eq!(stdout(&o).lines().count(), 1);
    let err = String::from_utf8(o.stderr).unwrap();
    assert!(err.contains("stdin is not seekable"), "{err}");
    #[cfg(unix)]
    assert!(err.contains("is a directory"), "{err}");
}

#[test]
fn rejects_invalid_coverage_and_level() {
    assert!(!sdigest(&["-c", "101", "x"]).status.success());
    assert!(!sdigest(&["-c", "abc", "x"]).status.success());
    assert!(!sdigest(&["-l", "10", "x"]).status.success());
    assert!(!sdigest(&["-l", "3", "-c", "1", "x"]).status.success());
}

#[test]
fn levels_map_to_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"a").unwrap();
    let a = a.to_str().unwrap();
    assert_eq!(
        stdout(&sdigest(&["-l", "7", a])),
        stdout(&sdigest(&["-c", "10", a]))
    );
    assert_eq!(stdout(&sdigest(&[a])), stdout(&sdigest(&["-l", "5", a])));
    assert!(stdout(&sdigest(&[a])).starts_with("sd1:1:1:"));
}

#[test]
fn reads_names_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b c");
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    let expected = stdout(&sdigest(&[a, b]));

    let o = sdigest_stdin(&[], format!("{a}\n{b}\n").as_bytes());
    assert!(o.status.success());
    assert_eq!(stdout(&o), expected);

    let o = sdigest_stdin(&["-0"], format!("{a}\0{b}\0").as_bytes());
    assert!(o.status.success());
    assert_eq!(stdout(&o), expected);
}

#[test]
fn check_reads_list_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"a").unwrap();
    let a = a.to_str().unwrap();
    let o = sdigest_stdin(&["--check"], &sdigest(&[a]).stdout);
    assert!(o.status.success());
    assert_eq!(stdout(&o), format!("{a}: OK\n"));
}

#[test]
fn summary_on_stderr_unless_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, vec![0u8; 1000]).unwrap();
    let a = a.to_str().unwrap();

    let err = stderr(&sdigest(&["-c", "5", a, a]));
    assert!(err.starts_with("sdigest: 2 files, 1.95 KiB in "), "{err}");
    assert!(
        err.contains("coverage 5%, read 1.95 KiB (100.000%)"),
        "{err}"
    );

    assert_eq!(stderr(&sdigest(&["-q", a])), "");
}

// APFS refuses names that aren't valid UTF-8, so macOS can't create the file.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn check_handles_non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join(OsStr::from_bytes(b"a\xff"));
    let sums = dir.path().join("sums");
    fs::write(&a, b"hello").unwrap();

    let o = Command::new(env!("CARGO_BIN_EXE_sdigest"))
        .arg(&a)
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(o.stdout.ends_with(b"a\xff\n"), "{:?}", o.stdout);
    fs::write(&sums, o.stdout).unwrap();

    let o = sdigest(&["--check", sums.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(String::from_utf8_lossy(&o.stdout).ends_with(": OK\n"));
}

#[test]
fn check_skips_malformed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let sums = dir.path().join("sums");
    fs::write(&a, b"hello").unwrap();
    let a = a.to_str().unwrap();
    let sums = sums.to_str().unwrap();
    let line = stdout(&sdigest(&[a]));
    // CRLF line ending, a blank line, and a truncated line.
    let crlf = line.replace('\n', "\r\n");
    fs::write(sums, format!("{crlf}\n{}", &line[..20])).unwrap();

    let o = sdigest(&["--check", sums]);
    assert!(!o.status.success());
    assert_eq!(stdout(&o), format!("{a}: OK\n"));
    let err = stderr(&o);
    assert!(err.contains(&format!("{sums}:2: malformed line")), "{err}");
    assert!(err.contains(&format!("{sums}:3: malformed line")), "{err}");
    assert!(err.contains("2 line(s) are improperly formatted"), "{err}");
}

#[test]
fn closed_output_pipe_exits_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"a").unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);

    let o = Command::new(env!("CARGO_BIN_EXE_sdigest"))
        .arg("-q")
        .arg(&a)
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(stderr(&o), "");
    assert_eq!(o.status.code(), Some(141));
}

#[test]
fn threads_option_takes_a_number_or_auto() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"a").unwrap();
    let a = a.to_str().unwrap();

    let o = sdigest(&["-t", "2", a]);
    assert!(o.status.success());
    assert!(
        stderr(&o).contains("; threads 2 (--threads)"),
        "{}",
        stderr(&o)
    );
    assert!(sdigest(&["--threads", "auto", a]).status.success());
    assert!(stderr(&sdigest(&[a])).contains("; threads "));
    assert!(!sdigest(&["-t", "0", a]).status.success());
    assert!(!sdigest(&["-t", "many", a]).status.success());
}

#[test]
fn output_is_the_same_for_any_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..20)
        .map(|i| {
            let p = dir.path().join(format!("f{i:02}"));
            fs::write(&p, vec![i as u8; 1000 + 70_000 * i]).unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect();
    let args = |t: &'static str| {
        let mut v = vec!["-q", "-l", "7", "-t", t];
        v.extend(names.iter().map(String::as_str));
        stdout(&sdigest(&v))
    };
    let one = args("1");
    assert_eq!(one.lines().count(), 20);
    assert_eq!(args("8"), one);
    assert_eq!(args("auto"), one);
}
