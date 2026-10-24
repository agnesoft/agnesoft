//! [internal]
//!
//! The asynchronous parts of [`ProcessImpl`](super::ProcessImpl) running
//! on [`tokio`].

use std::future::Future;
use std::pin::Pin;
use std::pin::pin;
use std::process::ExitStatus;
use std::task::Poll;
use std::time::Instant;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::ChildStdin;

use super::Sink;
use super::capture;

const BUFFER_SIZE: usize = 64 * 1024;

#[cfg(unix)]
type TreeHandle = u32;
#[cfg(windows)]
type TreeHandle = std::os::windows::io::OwnedHandle;

/// The process tree of a child, killed when dropped unless disarmed, e.g.
/// when the run is cancelled.
struct Tree(Option<TreeHandle>);

impl Tree {
    /// The process group of `child` if it leads one, see
    /// [`configure`](super::unix::configure).
    #[cfg(unix)]
    fn new(child: &Child, group: bool) -> Self {
        Self(child.id().filter(|_| group))
    }

    /// A new job holding `child`, see [`job`](super::windows::job).
    #[cfg(windows)]
    fn new(child: &Child, _group: bool) -> Self {
        Self(child.raw_handle().and_then(super::windows::job))
    }

    fn kill(&mut self) {
        if let Some(tree) = self.0.take() {
            #[cfg(unix)]
            super::unix::kill_group(tree);
            #[cfg(windows)]
            super::windows::terminate(&tree);
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Writes `input` to the standard input of `child` and reads its standard
/// output and error concurrently until they close, then waits for it to
/// exit. Past `deadline` or when cancelled it is killed with its
/// descendants if `group` (always on Windows), and `None` is returned as
/// the status on timeout.
pub(super) async fn wait_with_output(
    child: &mut Child,
    group: bool,
    input: &[u8],
    deadline: Option<Instant>,
    mut stdout_sink: Sink<'_>,
    mut stderr_sink: Sink<'_>,
) -> std::io::Result<(Option<ExitStatus>, Vec<u8>, Vec<u8>)> {
    let mut tree = Tree::new(child, group);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout_output = Vec::new();
    let mut stderr_output = Vec::new();

    let communicate = async {
        join([
            pin!(write(stdin, input)),
            pin!(read(stdout, &mut stdout_output, &mut stdout_sink)),
            pin!(read(stderr, &mut stderr_output, &mut stderr_sink)),
        ])
        .await;
        child.wait().await
    };

    let status = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), communicate)
            .await
            .ok(),
        None => Some(communicate.await),
    };

    let status = match status {
        Some(status) => status.map(Some),
        None => {
            tree.kill();
            let _ = child.start_kill();
            child.wait().await.map(|_| None)
        }
    };
    tree.0 = None;

    status.map(|status| (status, stdout_output, stderr_output))
}

/// Polls all `futures` until they complete.
async fn join(mut futures: [Pin<&mut (dyn Future<Output = ()> + Send)>; 3]) {
    let mut done = [false; 3];

    std::future::poll_fn(|context| {
        for (future, done) in futures.iter_mut().zip(&mut done) {
            *done = *done || future.as_mut().poll(context).is_ready();
        }

        if done.iter().all(|done| *done) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Reads `pipe` into `output` and `sink` until it closes or fails.
async fn read<R: AsyncRead + Unpin>(pipe: Option<R>, output: &mut Vec<u8>, sink: &mut Sink<'_>) {
    if let Some(mut pipe) = pipe {
        let mut buffer = vec![0; BUFFER_SIZE];

        while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
            capture(output, sink, &buffer[..read]);
        }
    }
}

/// Writes `input` to `stdin` and closes it, stopping on an error, e.g. when
/// the child does not read it.
async fn write(stdin: Option<ChildStdin>, input: &[u8]) {
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(input).await;
    }
}
