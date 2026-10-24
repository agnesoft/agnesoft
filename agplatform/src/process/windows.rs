//! [internal]
//!
//! The Windows specific parts of [`ProcessImpl`](super::ProcessImpl).

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::RawHandle;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::time::Duration;
use std::time::Instant;

use super::Sink;
use super::capture;
use super::millis_until;

const BUFFER_SIZE: usize = 8192;
const ERROR_IO_PENDING: i32 = 997;
const INFINITE: u32 = u32::MAX;
const PROCESS: usize = 3;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: RawHandle,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;

    fn CancelIoEx(file: RawHandle, overlapped: *mut Overlapped) -> i32;

    fn CreateEventW(
        attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> RawHandle;

    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> RawHandle;

    fn GetOverlappedResult(
        file: RawHandle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;

    fn ReadFile(
        file: RawHandle,
        buffer: *mut u8,
        length: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;

    fn TerminateJobObject(job: RawHandle, exit_code: u32) -> i32;

    fn WaitForMultipleObjects(
        count: u32,
        handles: *const RawHandle,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;

    fn WriteFile(
        file: RawHandle,
        buffer: *const u8,
        length: u32,
        written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

/// A pipe with at most one overlapped operation in flight, signalling its
/// event when the operation completes.
///
/// WHY: std creates its ends of the child pipes for overlapped I/O, which
/// is what its own `Command::output` relies on. It is not a documented
/// guarantee: if it changed, the reads would block one at a time, so
/// re-check `std::sys::pal::windows::pipe::anon_pipe` on toolchain
/// upgrades.
struct Channel {
    pipe: Option<OwnedHandle>,
    event: OwnedHandle,
    // WHY: boxed with the buffer so that both stay in place while an
    // operation is in flight, even if the channel moves.
    overlapped: Box<Overlapped>,
    buffer: Vec<u8>,
    pending: bool,
}

impl Channel {
    fn new(pipe: Option<OwnedHandle>) -> std::io::Result<Self> {
        // SAFETY: no attributes and no name, a manual-reset event that is
        // initially not signalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };

        (!event.is_null())
            .then(|| Self {
                pipe,
                // SAFETY: `event` is a new handle owned by nothing else.
                event: unsafe { OwnedHandle::from_raw_handle(event) },
                overlapped: Box::new(Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    event,
                }),
                buffer: vec![0; BUFFER_SIZE],
                pending: false,
            })
            .ok_or_else(std::io::Error::last_os_error)
    }

    fn is_idle(&self) -> bool {
        self.pipe.is_some() && !self.pending
    }

    fn handle(&self) -> RawHandle {
        self.pipe
            .as_ref()
            .map_or(std::ptr::null_mut(), AsRawHandle::as_raw_handle)
    }

    /// Completes the operation in flight, returning the bytes transferred
    /// or closing the pipe if it failed, e.g. because it was closed by the
    /// child.
    fn finish(&mut self) -> Option<usize> {
        let mut transferred = 0;
        self.pending = false;

        // SAFETY: the operation of `overlapped` on the pipe has completed as
        // its event is signalled.
        let result = unsafe {
            GetOverlappedResult(self.handle(), &mut *self.overlapped, &mut transferred, 0)
        };
        let transferred = (result != 0).then_some(transferred as usize);

        if transferred.is_none() {
            self.pipe = None;
        }

        transferred
    }

    fn start_read(&mut self) {
        // SAFETY: the buffer and `overlapped` stay in place and alive until
        // the operation completes, see `Drop`.
        let result = unsafe {
            ReadFile(
                self.handle(),
                self.buffer.as_mut_ptr(),
                self.buffer.len() as u32,
                std::ptr::null_mut(),
                &mut *self.overlapped,
            )
        };
        self.started(result);
    }

    fn start_write(&mut self, input: &[u8]) {
        // SAFETY: `input` and `overlapped` stay in place and alive until
        // the operation completes, see `Drop` and `wait_with_output`.
        let result = unsafe {
            WriteFile(
                self.handle(),
                input.as_ptr(),
                input.len().min(u32::MAX as usize) as u32,
                std::ptr::null_mut(),
                &mut *self.overlapped,
            )
        };
        self.started(result);
    }

    fn started(&mut self, result: i32) {
        // WHY: an operation that completes right away signals the event as
        // well, so it is finished like a pending one.
        if result != 0 || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_IO_PENDING) {
            self.pending = true;
        } else {
            self.pipe = None;
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if self.pending {
            let mut transferred = 0;

            // SAFETY: cancels the operation in flight and waits for it to
            // complete, so that its buffer and `overlapped` are no longer in
            // use when freed.
            unsafe {
                CancelIoEx(self.handle(), &mut *self.overlapped);
                GetOverlappedResult(self.handle(), &mut *self.overlapped, &mut transferred, 1);
            }
        }
    }
}

pub(super) fn configure(_command: &mut Command, _timeout: Option<Duration>) {}

/// Writes `input` to the standard input of `child` and reads its standard
/// output and error on the current thread until they close and it exits.
/// Past `deadline` it is killed with its descendants and `None` is
/// returned as the status.
pub(super) fn wait_with_output(
    child: &mut Child,
    input: &[u8],
    deadline: Option<Instant>,
    stdout_sink: Sink,
    stderr_sink: Sink,
) -> std::io::Result<(Option<ExitStatus>, Vec<u8>, Vec<u8>)> {
    let job = deadline.and_then(|_| job(child));
    let stdin = child.stdin.take().map(OwnedHandle::from);
    let stdout = child.stdout.take().map(OwnedHandle::from);
    let stderr = child.stderr.take().map(OwnedHandle::from);

    Channel::new(stdin).and_then(|stdin| {
        Channel::new(stdout).and_then(|stdout| {
            Channel::new(stderr).and_then(|stderr| {
                communicate(
                    child,
                    job.as_ref(),
                    [stdin, stdout, stderr],
                    input,
                    deadline,
                    stdout_sink,
                    stderr_sink,
                )
            })
        })
    })
}

/// Returns a new job holding `child`, so that the processes it starts can
/// be killed with it even after it exited, or `None` if it cannot be
/// created.
///
/// WHY: the processes the child starts before being assigned escape the
/// job, which std leaves no way to prevent as it cannot spawn suspended.
fn job(child: &Child) -> Option<OwnedHandle> {
    // SAFETY: no attributes and no name.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };

    (!job.is_null())
        // SAFETY: `job` is a new handle owned by nothing else.
        .then(|| unsafe { OwnedHandle::from_raw_handle(job) })
        .filter(|job| {
            // SAFETY: both are open handles.
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0 }
        })
}

/// Kills `job` with all its processes, and `child` should it be outside of
/// it.
fn kill_tree(child: &mut Child, job: Option<&OwnedHandle>) {
    if let Some(job) = job {
        // SAFETY: `job` is an open job handle.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    }

    let _ = child.kill();
}

fn communicate(
    child: &mut Child,
    job: Option<&OwnedHandle>,
    mut channels: [Channel; 3],
    mut input: &[u8],
    deadline: Option<Instant>,
    mut stdout_sink: Sink,
    mut stderr_sink: Sink,
) -> std::io::Result<(Option<ExitStatus>, Vec<u8>, Vec<u8>)> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exited = false;

    let timed_out = loop {
        if input.is_empty() {
            channels[0].pipe = None;
        }

        if channels[0].is_idle() {
            channels[0].start_write(input);
        }

        for channel in &mut channels[1..] {
            if channel.is_idle() {
                channel.start_read();
            }
        }

        let mut handles = Vec::with_capacity(4);
        let mut sources = Vec::with_capacity(4);

        for (index, channel) in channels.iter().enumerate() {
            if channel.pending {
                handles.push(channel.event.as_raw_handle());
                sources.push(index);
            }
        }

        if !exited {
            handles.push(child.as_raw_handle());
            sources.push(PROCESS);
        }

        if handles.is_empty() {
            break false;
        }

        let timeout = match deadline.map(millis_until) {
            Some(0) => break true,
            Some(millis) => millis,
            None => INFINITE,
        };

        // WHY: a timeout or a failed wait matches no source, so the loop
        // checks the deadline again.
        // SAFETY: `handles` are open handles alive for the duration of the
        // call.
        let signalled =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };

        match sources.get(signalled as usize).copied() {
            Some(PROCESS) => exited = true,
            Some(0) => {
                input = channels[0]
                    .finish()
                    .map_or(&[][..], |written| &input[written..])
            }
            Some(1) => {
                if let Some(read) = channels[1].finish() {
                    capture(&mut stdout, &mut stdout_sink, &channels[1].buffer[..read]);
                }
            }
            Some(_) => {
                if let Some(read) = channels[2].finish() {
                    capture(&mut stderr, &mut stderr_sink, &channels[2].buffer[..read]);
                }
            }
            None => {}
        }
    };

    let status = if timed_out {
        kill_tree(child, job);
        child.wait().map(|_| None)
    } else {
        child.wait().map(Some)
    };
    drop(channels);

    status.map(|status| (status, stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod wait_with_output {
        use super::*;
        use std::process::Stdio;

        #[test]
        fn wait_with_output_exited_child_should_close_broken_pipes() {
            // GIVEN a child that exited, breaking its pipes
            let mut child = Command::new("cmd")
                .args(["/C", "exit 0"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.wait().unwrap();

            // WHEN communicating with it
            let (status, stdout, stderr) =
                wait_with_output(&mut child, b"input", None, None, None).unwrap();

            // THEN the failing writes and reads close the pipes
            assert!(status.unwrap().success());
            assert!(stdout.is_empty());
            assert!(stderr.is_empty());
        }
    }
}
