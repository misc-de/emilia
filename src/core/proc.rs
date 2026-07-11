//! Subprocess helpers with a hard **wall-clock timeout**.
//!
//! `std::process::Command::output()`/`status()` wait *forever* for the child to
//! exit. A hung helper binary (`fpcalc`, `yt-dlp`, `ffmpeg` – a corrupt input,
//! YouTube throttling, a stalled network read) would then block the calling
//! worker thread permanently, with no way to recover. These wrappers spawn the
//! child, wait at most `timeout`, and on expiry **kill and reap** it before
//! returning an error. As with the raw `Command` calls they replace, only call
//! them from worker/background threads.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// How often the child is polled for completion while waiting. `std` has no
/// blocking "wait with timeout", so we poll; 50 ms keeps the worst-case kill
/// latency low without busy-spinning.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Like [`Command::output`], but kills the child and errors if it does not
/// finish within `timeout`. `stdout`/`stderr` are captured and drained in
/// background threads (so a full pipe buffer can never deadlock the child);
/// `stdin` is closed.
pub fn output_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Drain both pipes concurrently: a child that fills one pipe's buffer blocks
    // on write while we'd be waiting on the other — a classic deadlock.
    let mut out_pipe = child.stdout.take().expect("stdout was set to piped");
    let mut err_pipe = child.stderr.take().expect("stderr was set to piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let status = wait_or_kill(&mut child, timeout)?;
    // The child has exited, so both pipes are closed and the readers return.
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Like [`Command::status`], but kills the child and errors if it does not
/// finish within `timeout`. The child inherits the parent's stdio (callers using
/// this don't parse its output).
pub fn status_timeout(cmd: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    wait_or_kill(&mut child, timeout)
}

/// Like [`status_timeout`], but streams the child's **stdout** line by line to
/// `on_line` while it runs (stderr is discarded). Used to surface live progress
/// (yt-dlp's `--newline` output). A helper thread reads the pipe into an
/// unbounded channel — so a fast producer can never fill the pipe buffer and
/// deadlock — while this thread polls the child for exit/timeout exactly as
/// [`status_timeout`] does; `on_line` runs on the calling thread. Same
/// kill-on-timeout guarantee. Worker threads only.
pub fn status_timeout_lines(
    cmd: &mut Command,
    timeout: Duration,
    mut on_line: impl FnMut(&str),
) -> Result<ExitStatus> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout was set to piped");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => break, // EOF: child closed stdout
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            on_line(&line);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Timed out: kill and reap the child, then return promptly WITHOUT
            // joining the reader. `child.kill()` only signals the direct child;
            // a grandchild it spawned (e.g. yt-dlp's ffmpeg) can inherit the
            // stdout pipe and hold it open, so joining the reader would block on
            // `read_line` until *that* process exits — defeating this timeout.
            // The detached reader ends on its own once the pipe finally closes
            // (its next `tx.send` fails because `rx` is dropped, or it hits EOF).
            let _ = child.kill();
            let _ = child.wait();
            // Drain whatever the reader already buffered, then error out.
            while let Ok(line) = rx.try_recv() {
                on_line(&line);
            }
            bail!("subprocess timed out after {timeout:?} and was killed");
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    // Child exited: the reader hits EOF and returns; drain whatever is left.
    let _ = reader.join();
    while let Ok(line) = rx.try_recv() {
        on_line(&line);
    }
    Ok(status)
}

/// Polls `child` until it exits or `timeout` elapses; on expiry kills it and
/// reaps it (so it never lingers as a zombie), then errors.
fn wait_or_kill(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait(); // reap, so no zombie is left behind
            bail!("subprocess timed out after {timeout:?} and was killed");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_succeeds_and_captures_stdout() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        let out = output_timeout(&mut cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn hung_command_times_out_and_is_killed() {
        // `sleep 60` never finishes within the 200 ms budget → must be killed.
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let start = Instant::now();
        let res = status_timeout(&mut cmd, Duration::from_millis(200));
        assert!(res.is_err(), "expected a timeout error");
        // Returned promptly rather than waiting out the full sleep.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn nonzero_exit_is_reported_in_status() {
        let mut cmd = Command::new("false");
        let out = output_timeout(&mut cmd, Duration::from_secs(10)).unwrap();
        assert!(!out.status.success());
    }

    #[test]
    fn lines_are_streamed_in_order() {
        let mut cmd = Command::new("printf");
        cmd.arg("a\nb\nc\n");
        let mut got = Vec::new();
        let status = status_timeout_lines(&mut cmd, Duration::from_secs(10), |l| {
            got.push(l.to_string())
        })
        .unwrap();
        assert!(status.success());
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn lines_timeout_kills_and_still_drains() {
        // Emits one line, then hangs → the line must arrive before the kill.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'first\\n'; sleep 60"]);
        let mut got = Vec::new();
        let start = Instant::now();
        let res = status_timeout_lines(&mut cmd, Duration::from_millis(300), |l| {
            got.push(l.to_string())
        });
        assert!(res.is_err(), "expected a timeout error");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(got, vec!["first"]);
    }
}
