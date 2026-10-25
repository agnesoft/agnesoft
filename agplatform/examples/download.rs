//! A download of a large file, written to disk as it arrives and resumed
//! when the connection is lost in the middle.
//!
//! It needs both the filesystem and requests, so it takes `&impl Platform`
//! and its tests a `TestPlatform`. Its `TestRequest` drops connections
//! after a part of the body on demand and answers the resumed request by
//! its `Range` header. Its `TestFs` holds the downloaded file and can make
//! the disk fail.
//!
//! ```bash
//! cargo run --example download --features fs,request -- http://example.com page.html
//! ```

use std::io::Write;
use std::path::Path;

use agplatform::BlockingRequestBuilderExt;
use agplatform::Env;
use agplatform::Fs;
use agplatform::OpenWriteOptions;
use agplatform::Platform;
use agplatform::Request;
use agplatform::TransportError;
use agplatform::reqwest::header::ACCEPT_RANGES;
use agplatform::reqwest::header::RANGE;

/// Downloads `url` to `path` in up to `attempts` and returns its size. An
/// attempt that loses the connection is resumed where it stopped when the
/// server accepts ranges, and started again otherwise. Other failures are
/// not retried.
fn download(
    platform: &impl Platform,
    url: &str,
    path: &Path,
    attempts: u32,
) -> agplatform::Result<u64> {
    let request = platform.request();
    let client = request.blocking_client()?;
    let mut offset = 0;

    for attempt in 1.. {
        let (builder, options) = match offset {
            0 => (client.get(url), OpenWriteOptions::new().create().truncate()),
            _ => (
                client.get(url).header(RANGE, format!("bytes={offset}-")),
                OpenWriteOptions::new().append(),
            ),
        };
        let mut file = platform.fs().open_write_options(path, options)?;
        let mut received = offset;

        let result = builder.send_blocking_to(request, &mut |chunk| {
            file.write_all(chunk)
                .map_err(|error| agplatform::Error::fs(format!("{}: {error}", path.display())))?;
            received += chunk.len() as u64;
            Ok(())
        });

        let error = match result {
            Ok(_) => return Ok(received),
            Err(error) if attempt < attempts => error,
            Err(error) => return Err(error),
        };
        let Some(interrupted) = error.source_as::<TransportError>() else {
            return Err(error);
        };
        let resumable = interrupted
            .output()
            .and_then(|output| output.headers.get(ACCEPT_RANGES))
            .is_some_and(|ranges| ranges == "bytes");
        offset = if resumable { received } else { 0 };
    }

    unreachable!("the attempts end with a return")
}

fn main() -> agplatform::Result<()> {
    let platform = agplatform::platform();
    let args: Vec<&str> = platform.env().args().skip(1).map(String::as_str).collect();

    match args.as_slice() {
        [url, path] => println!(
            "downloaded {} bytes",
            download(&platform, url, Path::new(path), 5)?
        ),
        _ => println!("usage: download <url> <path>"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agplatform::RequestOutput;
    use agplatform::TransportErrorKind;
    use agplatform::reqwest::header::HeaderMap;
    use agplatform::test_platform::test_fs::FailMode;
    use agplatform::test_platform::test_platform;

    const URL: &str = "https://example.com/big.iso";

    fn ok(status: u16, accept_ranges: bool) -> RequestOutput {
        let mut headers = HeaderMap::new();
        if accept_ranges {
            headers.insert(ACCEPT_RANGES, "bytes".parse().unwrap());
        }
        RequestOutput {
            status,
            headers,
            ..Default::default()
        }
    }

    mod download {
        use super::*;

        #[test]
        fn download_should_write_body_to_file_as_it_arrives() {
            let platform = test_platform();
            platform.request.set_chunks(
                "/big.iso",
                ok(200, false),
                [b"first ".to_vec(), b"second".to_vec()],
            );

            let size = download(&platform, URL, Path::new("big.iso"), 1).unwrap();

            assert_eq!(size, 12);
            assert_eq!(platform.fs.read("big.iso").unwrap(), b"first second");
        }

        #[test]
        fn download_interrupted_should_resume_from_received_part() {
            // GIVEN a server accepting ranges that drops the connection
            // after a part of the file, and answers the rest when asked
            let platform = test_platform();
            platform.request.set_interrupted(
                "/big.iso",
                ok(200, true),
                [b"first ".to_vec()],
                TransportErrorKind::Other,
            );
            platform.request.set_response_if(
                |call| {
                    call.headers
                        .get(RANGE)
                        .is_some_and(|range| range == "bytes=6-")
                },
                RequestOutput {
                    body: b"second".to_vec(),
                    ..ok(206, true)
                },
            );

            // WHEN downloading the file
            let size = download(&platform, URL, Path::new("big.iso"), 2).unwrap();

            // THEN the second attempt asks only for the rest and appends it
            assert_eq!(size, 12);
            assert_eq!(platform.fs.read("big.iso").unwrap(), b"first second");
            assert_eq!(
                platform.request.call_details()[1].headers[RANGE],
                "bytes=6-"
            );
        }

        #[test]
        fn download_interrupted_without_ranges_should_start_again() {
            // GIVEN a server not accepting ranges that always drops the
            // connection after a part of the file
            let platform = test_platform();
            platform.request.set_interrupted(
                "/big.iso",
                ok(200, false),
                [b"first ".to_vec()],
                TransportErrorKind::Other,
            );

            // WHEN downloading the file
            let result = download(&platform, URL, Path::new("big.iso"), 3);

            // THEN every attempt starts again from an empty file, until
            // they run out
            assert!(result.unwrap_err().source_as::<TransportError>().is_some());
            assert_eq!(platform.fs.read("big.iso").unwrap(), b"first ");
            let calls = platform.request.call_details();
            assert_eq!(calls.len(), 3);
            assert!(calls.iter().all(|call| !call.headers.contains_key(RANGE)));
        }

        #[test]
        fn download_failed_status_should_fail_without_retrying() {
            let platform = test_platform();
            platform.request.set_response("/big.iso", ok(404, false));

            let error = download(&platform, URL, Path::new("big.iso"), 3).unwrap_err();

            assert_eq!(error.request_output().unwrap().status, 404);
            assert_eq!(platform.request.calls().len(), 1);
        }

        #[test]
        fn download_failing_disk_should_fail_without_retrying() {
            // GIVEN a disk failing all writes
            let mut platform = test_platform();
            platform.fs.set_fail_mode(Some(FailMode::Write));
            platform
                .request
                .set_chunks("/big.iso", ok(200, true), [b"data".to_vec()]);

            // WHEN downloading a file
            let error = download(&platform, URL, Path::new("big.iso"), 3).unwrap_err();

            // THEN the error of the disk is returned at once
            assert_eq!(error.kind(), agplatform::ErrorKind::Fs);
            assert_eq!(platform.request.calls().len(), 1);
        }
    }
}
