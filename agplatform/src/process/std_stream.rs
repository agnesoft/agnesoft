use std::io::Write;

use crate::Result;
use crate::interceptor::Chunk;
use crate::interceptor::Interceptor;

use super::OutputStream;
use super::ProcessCommand;

/// The [`Interceptor`] writing the output of the runs with
/// [`CommandOptions::stream`](crate::CommandOptions::stream) to the standard
/// output and error of the current process, the only interceptor of a process
/// by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdStream;

impl Interceptor<ProcessCommand> for StdStream {
    fn chunk(&self, chunk: Chunk<ProcessCommand>) -> Result {
        if chunk.run().subject().options().stream {
            let _ = match chunk.stream() {
                OutputStream::Stdout => write_flushed(&mut std::io::stdout().lock(), chunk.data()),
                OutputStream::Stderr => write_flushed(&mut std::io::stderr().lock(), chunk.data()),
            };
        }

        chunk.forward()
    }
}

/// Writes `data` to `out` and flushes it.
fn write_flushed(out: &mut dyn Write, data: &[u8]) -> std::io::Result<()> {
    out.write_all(data)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interceptor::Interceptors;
    use crate::process::CommandExt;
    use crate::process::Process;
    use crate::process::tests::*;

    mod std_stream {
        use super::*;

        mod interceptor {
            use super::*;

            mod chunk {
                use super::*;

                #[test]
                fn chunk_should_pass_streamed_and_other_chunks_on() {
                    let recorder = Recorder::default();
                    let process = process(std::env::temp_dir());
                    process.add_interceptor(recorder.clone());
                    let mut interceptors = Interceptors::new();
                    interceptors.push(recorder.clone());
                    interceptors.push(StdStream);

                    for run in [
                        shell("echo out&& echo err 1>&2").stream(),
                        shell("echo out&& echo err 1>&2").interceptors(interceptors),
                    ] {
                        process.run_blocking(run).unwrap();
                    }

                    assert_eq!(
                        String::from_utf8_lossy(&recorder.stdout()).lines().count(),
                        2
                    );
                    assert_eq!(
                        String::from_utf8_lossy(&recorder.stderr()).lines().count(),
                        2
                    );
                }
            }
        }
    }

    mod write_flushed {
        use super::*;

        #[test]
        fn write_flushed_failing_should_return_error() {
            let mut buffer = [0_u8; 2];

            let error = write_flushed(&mut buffer.as_mut_slice(), b"data").unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        }
    }
}
