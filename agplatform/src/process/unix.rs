//! [internal]
//!
//! The Unix specific parts of [`ProcessImpl`](super::ProcessImpl).

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::time::Duration;
use std::time::Instant;

use super::Sink;
use super::capture;
use super::millis_until;

#[cfg(target_os = "linux")]
type NFds = std::ffi::c_ulong;
#[cfg(not(target_os = "linux"))]
type NFds = std::ffi::c_uint;

#[cfg(target_os = "linux")]
const PIPE_BUF: usize = 4096;
#[cfg(not(target_os = "linux"))]
const PIPE_BUF: usize = 512;

/// The default capacity of a Linux pipe, read at once.
const BUFFER_SIZE: usize = 64 * 1024;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const SIGKILL: i32 = 9;
const WAIT_INTERVAL_MS: u32 = 10;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn poll(fds: *mut PollFd, count: NFds, timeout: i32) -> i32;
}

/// Makes the child the leader of a new process group when it may need to
/// be killed with its descendants.
pub(super) fn configure(command: &mut Command, timeout: Option<Duration>) {
    if timeout.is_some() {
        std::os::unix::process::CommandExt::process_group(command, 0);
    }
}

/// Writes `input` to the standard input of `child` and reads its standard
/// output and error on the current thread until they close, then waits for
/// it to exit. Past `deadline` it is killed with its descendants and
/// `None` is returned as the status.
pub(super) fn wait_with_output(
    child: &mut Child,
    mut input: &[u8],
    deadline: Option<Instant>,
    mut stdout_sink: Sink,
    mut stderr_sink: Sink,
) -> std::io::Result<(Option<ExitStatus>, Vec<u8>, Vec<u8>)> {
    let mut stdin = child.stdin.take().map(OwnedFd::from).map(File::from);
    let mut stdout = child.stdout.take().map(OwnedFd::from).map(File::from);
    let mut stderr = child.stderr.take().map(OwnedFd::from).map(File::from);
    let mut stdout_output = Vec::new();
    let mut stderr_output = Vec::new();
    let mut buffer = vec![0; BUFFER_SIZE];

    let timed_out = loop {
        if input.is_empty() {
            stdin = None;
        }

        if stdin.is_none() && stdout.is_none() && stderr.is_none() {
            break false;
        }

        let timeout = match deadline.map(millis_until) {
            Some(0) => break true,
            Some(millis) => millis as i32,
            None => -1,
        };
        let mut fds = [
            poll_fd(&stdin, POLLOUT),
            poll_fd(&stdout, POLLIN),
            poll_fd(&stderr, POLLIN),
        ];

        // WHY: a failed poll (EINTR, ENOMEM) leaves all revents zero, so it
        // is simply retried.
        // SAFETY: `fds` is an array of pollfd alive for the duration of the
        // call. Closed pipes have a negative fd, which poll ignores.
        let _ = unsafe { poll(fds.as_mut_ptr(), fds.len() as NFds, timeout) };

        if fds[0].revents != 0 {
            input = write_chunk(&mut stdin, input);
        }

        if fds[1].revents != 0 {
            read_chunk(
                &mut stdout,
                &mut buffer,
                &mut stdout_output,
                &mut stdout_sink,
            );
        }

        if fds[2].revents != 0 {
            read_chunk(
                &mut stderr,
                &mut buffer,
                &mut stderr_output,
                &mut stderr_sink,
            );
        }
    };

    let status = if timed_out {
        kill_and_wait(child)
    } else {
        wait_timeout(child, deadline)
    };

    status.map(|status| (status, stdout_output, stderr_output))
}

fn kill_tree(child: &Child) {
    // SAFETY: a negative pid signals the whole process group, which the
    // child leads since it was configured with `process_group(0)`.
    let _ = unsafe { kill(-(child.id() as i32), SIGKILL) };
}

fn kill_and_wait(child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
    kill_tree(child);
    child.wait().map(|_| None)
}

fn poll_fd(pipe: &Option<File>, events: i16) -> PollFd {
    PollFd {
        fd: pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        events,
        revents: 0,
    }
}

/// Reads what is available from `pipe`, closing it at its end or on an
/// error.
fn read_chunk(pipe: &mut Option<File>, buffer: &mut [u8], output: &mut Vec<u8>, sink: &mut Sink) {
    match pipe.as_mut().map(|pipe| pipe.read(buffer)) {
        Some(Ok(read @ 1..)) => capture(output, sink, &buffer[..read]),
        _ => *pipe = None,
    }
}

/// Waits for `child` to exit at most until `deadline`, otherwise kills it
/// with its descendants and returns `None`.
fn wait_timeout(
    child: &mut Child,
    deadline: Option<Instant>,
) -> std::io::Result<Option<ExitStatus>> {
    let Some(deadline) = deadline else {
        return child.wait().map(Some);
    };

    loop {
        match child.try_wait() {
            Ok(None) => match millis_until(deadline) {
                0 => return kill_and_wait(child),
                millis => {
                    std::thread::sleep(Duration::from_millis(millis.min(WAIT_INTERVAL_MS).into()))
                }
            },
            result => return result,
        }
    }
}

/// Writes the next chunk of `input` to `stdin` and returns the rest, none
/// on an error.
fn write_chunk<'a>(stdin: &mut Option<File>, input: &'a [u8]) -> &'a [u8] {
    // WHY: once poll reports a pipe writable, writing at most PIPE_BUF bytes
    // does not block.
    match stdin
        .as_mut()
        .map(|stdin| stdin.write(&input[..input.len().min(PIPE_BUF)]))
    {
        Some(Ok(written)) => &input[written..],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod wait_timeout {
        use super::*;

        #[test]
        fn wait_timeout_exceeding_deadline_should_kill_child() {
            // GIVEN a running child leading its process group
            let mut command = Command::new("sleep");
            command.arg("10");
            configure(&mut command, Some(Duration::ZERO));
            let mut child = command.spawn().unwrap();
            let start = Instant::now();

            // WHEN its deadline passes
            let status = wait_timeout(&mut child, Some(start + Duration::from_millis(100)));

            // THEN it is killed right away
            assert_eq!(status.unwrap(), None);
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }
}
