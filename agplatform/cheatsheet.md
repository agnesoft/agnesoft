# agplatform cheat sheet

Snippets for the common tasks, free of setup. Each one assumes the code takes the platform (or one of its interfaces) as a parameter, returns `agplatform::Result`, and has these in scope:

```rust
use agplatform::*; // the traits must be in scope: Env, Fs, Process, CommandExt, Request, ...
use std::process::Command;
use std::time::Duration;

let platform = agplatform::platform(); // tests: agplatform::test_platform::test_platform()
let env = platform.env();
let fs = platform.fs();
let process = platform.process();
let request = platform.request();
```

Rules of thumb:

- Never call `std::env`, `std::fs`, `std::process` or `reqwest` sending directly. Go through the platform so that tests can swap in the in-memory doubles.
- Prefer taking the platform over the interfaces: `&impl Platform` or `&Arc<impl Platform>`.
- Platforms are `Send + Sync`, and `Arc<P: Platform>` is a `Platform` too.
- All errors are `agplatform::Error`. Check them with `error.kind()` (`ErrorKind::{Env, Fs, Process, Request, Other}`).
- You can get the source error with convenience `Error::command_output()`, `Error::request_output()` when the respective interface calls fail to get the error output.

## Features

| Feature       | Enables                                                                                     |
| ------------- | ------------------------------------------------------------------------------------------- |
| `env`         | `Env`, `Platform`, `platform()`                                                             |
| `fs`          | `Fs` (+ `env`)                                                                              |
| `process`     | `Process`, `CommandExt` (+ `env`, `interceptor`)                                            |
| `request`     | `Request`, blocking reqwest (+ `env`, `interceptor`)                                        |
| `tls`         | rustls for `request` (`native-tls` for the OS TLS)                                          |
| `tokio`       | `run_async`, `send_async`, async `client()`                                                 |
| `interceptor` | `agplatform::interceptor` (pulled in by `process` and `request`)                            |
| `testing`     | `test_platform()`, `TestEnv`, `TestFs`, `TestProcess`, `TestRequest`, `TestDir`, `TestFile` |

```toml
agplatform = { version = "0.4", features = ["fs", "process", "request", "tls"] }

[dev-dependencies]
agplatform = { version = "0.4", features = ["fs", "process", "request", "tls", "testing"] }
```

## Platform

```rust
fn tool(platform: &impl Platform) -> agplatform::Result { /* platform.env(), .fs(), ... */ }

// Share with threads
let platform = Arc::new(agplatform::platform());
let shared = platform.clone();
std::thread::spawn(move || tool(&shared));

// Share with scoped threads, no Arc needed
std::thread::scope(|scope| { scope.spawn(|| tool(&platform)); });

// Share with tokio tasks
let platform = Arc::new(agplatform::platform());
let p = platform.clone();
tasks.spawn(async move { run(p.as_ref()).await });
```

## Env

The env is the platform's own copy, loaded from the process on first use. Changing it never touches the process environment. Fs, process and request all see the changes: relative paths resolve against `current_dir()`, children get exactly `vars()`, and proxies are read from it.

```rust
let args: Vec<&OsString> = env.args().skip(1).collect();        // OsString, never lossy
let flag = env.args().any(|arg| arg == "--verbose");
let value: Option<OsString> = env.var("KEY");
let value: String = env.var("KEY").map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
let verbose: bool = env.var_bool("VERBOSE")?;                    // true/on/1, false/off/0/""/missing, else Err
let previous: Option<OsString> = env.set_var("KEY", "value")?;   // Err on empty key, '=' or NUL
let removed: Option<OsString> = env.remove_var("KEY");
let all: Vec<(OsString, OsString)> = env.vars().collect();

let cwd: PathBuf = env.current_dir();
let previous: PathBuf = env.set_current_dir("sub/dir")?;        // relative to current_dir, must exist
let home: Option<PathBuf> = env.home_dir();                      // HOME / USERPROFILE, None if unset/empty
let tmp: PathBuf = env.tmp_dir();
let exe: &Path = env.current_exe();

Err(agplatform::Error::env("HOME is not set"))?;                  // own Env error
```

## Fs

Relative paths resolve against `env.current_dir()`. Errors are `ErrorKind::Fs`, name the path, and carry the `std::io::Error` (`error.source_as::<std::io::Error>()`).

```rust
// Whole files
fs.write("out.txt", "text")?;                       // creates/truncates, any AsRef<[u8]>
let bytes: Vec<u8> = fs.read("out.txt")?;
let text: String = fs.read_to_string("out.txt")?;
let exists: bool = fs.exists("out.txt")?;

// Atomic replace: write a temp file, then rename it over the old one
fs.write(dir.join(".note.tmp"), content)?;
fs.rename(dir.join(".note.tmp"), dir.join("note.md"))?;

// Handles (std Read/Write/Seek + positional I/O and locks)
let mut file = fs.open_read("in.bin")?;             // impl ReadFile
file.read_to_end(&mut buf)?;                        // std::io::Read
file.read_exact_at(&mut header, 0)?;
let mut file = fs.open_write("out.bin")?;           // create + truncate, impl WriteFile
file.write_all(b"data")?;                           // std::io::Write
file.sync_all()?;
let mut log = fs.open_write_options("app.log", OpenWriteOptions::new().create().append())?;
let fresh = fs.open_write_options("lock", OpenWriteOptions::new().create_new())?; // Err if exists
let locked: bool = file.try_lock()?;                // exclusive, also lock()/lock_shared()/unlock()

// Directories
fs.create_dir_all("a/b/c")?;
fs.create_dir("a/d")?;
for entry in fs.read_dir("a")? {
    let entry = entry?;                             // DirEntry: path(), file_name(), file_type()
    if entry.file_type() == FileType::File { /* entry.path() */ }
}
fs.copy("a.txt", "b.txt")?;                         // returns bytes copied
fs.copy_dir_all("src_dir", "dst_dir")?;
fs.remove_file("b.txt")?;
fs.remove_dir("a/d")?;                              // must be empty
fs.remove_dir_all("a")?;

// Metadata and attributes
let meta: Metadata = fs.metadata("a.txt")?;        // follows symlinks; symlink_metadata() does not
let (len, modified, is_dir) = (meta.len, meta.modified, meta.file_type.is_dir());
fs.set_readonly("a.txt", true)?;
fs.set_executable("run.sh", true)?;
fs.set_modified("a.txt", SystemTime::now())?;
fs.set_mode("a.txt", 0o644)?;                       // Unix only, as are meta.mode/uid/gid

// Links
fs.create_symlink_file("link", "a.txt")?;           // create_symlink_dir for directories
let target: PathBuf = fs.read_link("link")?;
let real: PathBuf = fs.canonicalize("link")?;
fs.create_hard_link("hard", "a.txt")?;

Err(agplatform::Error::fs(format!("{}: {error}", path.display())))?; // own Fs error, e.g. in a sink
```

## Process

Build a `std::process::Command`, optionally configure the run through `CommandExt`, then run it on the process. Output is captured by default. A non-success exit is an `Err` carrying the output. Use `Error::command_output()` to get the error output.

```rust
// Run and read the output
let output = Command::new("git").args(["rev-parse", "HEAD"]).run_blocking(process)?;
let stdout: &str = output.stdout_str()?;            // Err if not UTF-8; stdout_lossy() never fails
let stderr = output.stderr_lossy();
let (status, took) = (output.status, output.duration); // Option<i32>, Duration
process.run_blocking(Command::new("git").arg("status"))?; // same thing, called on the process

// Command settings are std's
Command::new("cargo").arg("build").current_dir("crate").env("RUSTFLAGS", "-D warnings").run_blocking(process)?;

// Run options (chainable, each returns a Run)
Command::new("git").args(["commit", "--file", "-"]).input(message).run_blocking(process)?; // stdin
Command::new("git").arg("push").timeout(Duration::from_secs(60)).run_blocking(process)?;   // kills the process group
Command::new("cargo").arg("build").stream().run_blocking(process)?;      // live output to stdout/stderr, still captured
Command::new("git").arg("--version").silent().run_blocking(process)?;   // no output captured or shown
Command::new("vim").interactive().run_blocking(process)?;               // inherits the terminal
Command::new("env").clear_env().run_blocking(process)?;                 // instead of Command::env_clear (ignored)
Command::new("make").display_lines(20).run_blocking(process)?;          // lines shown when the output is displayed

// Handle failures
match Command::new("git").arg("pull").run_blocking(process) {
    Ok(output) => {}
    Err(error) => match error.command_output() {
        Some(output) if output.timed_out => {}                      // timed out
        Some(output) => eprintln!("{}", output.stderr_lossy()),     // ran and failed (non-zero, signal)
        None => {}                                                  // did not start, e.g. not installed
    },
}
let installed = match Command::new("git").arg("--version").silent().run_blocking(process) {
    Ok(_) => true,
    Err(error) => error.command_output().is_some(),
};

// Large output: stream to a sink instead of capturing
Command::new("git").args(["archive", "HEAD"]).run_blocking_with_sink(process, &mut |stream, chunk| {
    if stream == OutputStream::Stdout { file.write_all(chunk).map_err(agplatform::Error::fs)?; }
    Ok(())
})?;

// Async (feature "tokio"): dropping the future kills the process
let output = Command::new("cargo").arg("test").run_async(process).await?;
let output = Command::new("cargo").arg("test").timeout(Duration::from_secs(600)).run_async(process).await?;
let output = Command::new("cargo").arg("build").run_async_with_sink(process, &mut |_, chunk| Ok(())).await?;
```

## Request

Build the request with the reqwest client from the platform, then send it with `send_blocking(request)` or `send_async(request)`. The whole body is read. A non-2xx status is an `Err` carrying the response. Use the platform's clients so that proxies come from the platform env. Use `Error::request_output()` to get the error output.

```rust
// Blocking (BlockingRequestBuilderExt). Do not use it inside an async context.
let client = request.blocking_client()?;            // cheap handle, shared pool
let output = client.get("https://example.com/api").send_blocking(request)?;
let text: &str = output.text()?;                    // Err if not UTF-8; text_lossy() never fails
let (status, headers, body) = (output.status, &output.headers, &output.body);
client.post(url).bearer_auth(token).body(data).send_blocking(request)?;     // any reqwest builder method
client.post(url).header(CONTENT_TYPE, "text/plain").body(data).send_blocking(request)?;
request.send_blocking(client.get(url))?;            // same thing, called on the request

// Async (RequestBuilderExt, feature "tokio")
let client = request.client()?;
let output = client.get(url).send_async(request).await?;
let output = tokio::time::timeout(deadline, client.get(url).send_async(request)).await; // drop = cancel

// Large body: stream a successful (2xx) body to a sink
client.get(url).send_blocking_with_sink(request, &mut |chunk| {
    file.write_all(chunk).map_err(agplatform::Error::fs)
})?;
client.get(url).send_async_with_sink(request, &mut |chunk| Ok(())).await?;

// Options
client.get(url).keep_partial_body().send_blocking(request)?; // keep the received body in TransportError::output()
client.get(url).display_lines(5).send_blocking(request)?;    // lines shown when the response is displayed

// Handle failures
match client.get(url).send_blocking(request) {
    Ok(output) => {}
    Err(error) if error.request_output().is_some_and(|output| output.status == 404) => {} // got a response
    Err(error) => match error.source_as::<TransportError>() {
        Some(transport) if transport.kind() == TransportErrorKind::Connect => {}         // no connection
        Some(transport) => { let head = transport.output(); }  // Timeout/Other, head if it arrived (e.g. to resume)
        None => {}                                              // invalid request or interceptor error
    },
}

// Custom clients (proxies/timeouts from the platform env are kept)
let client = request.blocking_client_builder().user_agent("tool/1.0").build().map_err(agplatform::Error::request)?;
request.set_blocking_client(request.blocking_client_builder().user_agent("tool/1.0")); // new default
request.set_client(request.client_builder().timeout(Duration::from_secs(60)));          // async default
```

## Interceptors

Every command run and HTTP request passes through a chain of interceptors (middleware). By default a process has `StdStream`, which powers `.stream()`, and a request has none. You can set them on the platform interface, which applies to the runs that start afterwards, or per run.

```rust
use agplatform::interceptor::{DryRun, Interceptor, Interceptors, Retry};

// Retry what is safe to retry. Process: failed exit, but not a timeout or a start failure.
// Request: idempotent methods on transport errors/408/429/5xx, others only on Connect/408/429/503.
// Request retries honour Retry-After.
process.add_interceptor(ProcessCommand::retry(3, Duration::from_secs(5)));
request.add_interceptor(HttpRequest::retry(4, Duration::from_millis(100)).backoff(2).max_delay(Duration::from_secs(10)));

// Retry on your own condition
Retry::<ProcessCommand>::new(3, Duration::from_secs(5))
    .when(|_run, result| result.as_ref().is_ok_and(|o| o.stderr_lossy().contains("Could not resolve host")));
HttpRequest::retry(3, Duration::from_secs(1)).when(|_run, result| {
    result.as_ref().err().and_then(|e| e.source_as::<TransportError>())
        .is_some_and(|e| e.kind() == TransportErrorKind::Connect)
});

// Per run: start from the platform's interceptors and put Retry outermost (index 0)
let mut interceptors = process.interceptors();
interceptors.insert(0, ProcessCommand::retry(3, Duration::from_secs(5)));
Command::new("git").arg("fetch").interceptors(interceptors).run_blocking(process)?;
let mut interceptors = request.interceptors();
interceptors.insert(0, HttpRequest::retry(2, Duration::from_millis(100)));
client.get(url).interceptors(interceptors).send_blocking(request)?;

// Cache outputs (successful GET/HEAD for requests). Keep a clone to inspect it or persist it.
let cache = CommandCache::default();                // or CommandCache::with_vars(["KUBECONFIG"])
process.add_interceptor(cache.clone());
let entries = cache.entries();                      // Vec<(String, CommandOutput)>, restore with cache.load(entries)
request.add_interceptor(RequestCache::default());

// Dry run: nothing is executed, default output or the one attached to the run
process.add_interceptor(DryRun);
Command::new("kubectl").args(["get", "pods"])
    .with(DryRunOutput(CommandOutput { stdout: b"pod-1".to_vec(), ..Default::default() }))
    .run_blocking(process)?;

// Replace the defaults, e.g. your own output printer instead of StdStream
process.clear_interceptors();
process.add_interceptor(MyPrinter::default());      // added as the innermost one

// Attach a value to a run, read it in an interceptor with run.get::<T>()
client.get(url).with(ShowProgress).send_blocking(request)?;
Command::new("cargo").with(Tag("build")).run_blocking(process)?;

// Write an interceptor: implement any of handle / chunk / cancelled
#[derive(Clone, Default)]
struct Progress(Arc<AtomicUsize>);                  // share state through an Arc
impl Interceptor<HttpRequest> for Progress {
    fn chunk(&self, chunk: Chunk<HttpRequest>) -> agplatform::Result {
        self.0.fetch_add(chunk.data().len(), Ordering::Relaxed);
        chunk.forward()                             // chunk.set_data(..) to change it
    }
}
struct Token;
impl Interceptor<ProcessCommand> for Token {
    fn handle<'a>(&'a self, mut next: Next<'a, ProcessCommand>) -> BoxFuture<'a, agplatform::Result<CommandOutput>> {
        next.run_mut().subject_mut().set_env("TOKEN", "secret");  // change the command/request
        next.forward()                              // or: Box::pin(async move { let r = next.attempt().await; ...; r })
    }
}
// Useful in interceptors: run.subject() (ProcessCommand: program(), command_line(), options();
// HttpRequest: method(), url(), target(), headers()), run.id(), run.attempt(), run.get::<T>(),
// chunk.stream() (OutputStream for processes).
```

## Errors

```rust
agplatform::Error::env("..."); Error::fs("..."); Error::process("..."); Error::request("..."); Error::other("...");
Error::other("failed").with_source(io_error);       // attach a cause
error.kind();                                       // ErrorKind
error.description();                                // message without the source
error.source_as::<std::io::Error>();                // typed cause (io::Error, TransportError, ...)
error.command_output();                             // Option<&CommandOutput> of a run that failed
error.request_output();                             // Option<&RequestOutput> of a non-2xx response
error.program();                                    // program of a process error
error.request_target();                             // "METHOD url" of a request error (no credentials/query)
```

## Testing

Enable the `testing` feature in dev-dependencies. Every double is in-memory, deterministic and thread-safe. Retry delays and timeouts are recorded instead of waited for.

```rust
use agplatform::test_platform::{test_platform, ROOT};

// Whole platform, with direct access to each double
let platform = test_platform()
    .with_env(TestEnv::new().with_args(vec!["tool", "--verbose"]).with_vars(vec![("HOME", "/home")]).with_current_dir("/work"))
    .with_fs(TestFs::new())
    .with_process(TestProcess::new())
    .with_request(TestRequest::new());
tool(&platform)?;
platform.fs.read_to_string("/work/out.txt")?;       // fields: env, fs, process, request
platform.process.calls();

// Or a single double where a narrow interface is taken
let fs = TestFs::new();
save(&fs, ...)?;
```

### TestEnv

```rust
TestEnv::new()                                      // no args, no vars, current dir ROOT
    .with_args(vec!["tool", "a.txt"])
    .with_vars(vec![("KEY", "value")])
    .with_current_dir("/work")                      // not checked for existence
    .with_current_exe("/bin/tool");
```

### TestFs

```rust
let fs = TestFs::new();                             // empty, prepare it with the Fs methods
fs.write("/data/a.txt", "x")?;
fs.set_io_error(FsOp::Write, "/data", ErrorKind::StorageFull);   // path prefix, "" = everything
fs.set_io_errors(FsOp::Read, "/a.txt", [None, Some(ErrorKind::Interrupted)]); // per call in order
fs.set_short_io(FsOp::Read, "", 3);                 // reads/writes return at most 3 bytes
fs.set_capacity(Some(1024));                        // disk full beyond 1 KiB
fs.clear_faults();
fs.crash();                                         // drop unsynced data, close all handles
fs.sync();                                          // make everything durable
fs.set_len("/big", 10 << 30)?;                      // huge file without memory
fs.set_created("/a.txt", time)?;
fs.set_owner("/a.txt", 1000, 1000)?;               // Unix only; set_hidden(path, true) is Windows only
```

### TestProcess

Patterns are prefixes of the command line (`"git push"`), and `""` matches everything. The most recently set pattern wins, so set `""` first as a default. A run without a matching output fails.

```rust
let process = TestProcess::new();
process.set_output("git rev-parse", CommandOutput { status: Some(0), stdout: b"main\n".to_vec(), ..Default::default() });
process.set_output("git push", CommandOutput { status: Some(1), stderr: b"rejected".to_vec(), ..Default::default() });
process.set_outputs("curl", [failure, success]);   // one per run, last repeated
process.set_io_error("docker", ErrorKind::NotFound);              // not installed
process.set_output("sleep", CommandOutput { duration: Duration::from_secs(3600), ..Default::default() }); // > timeout = timed out
let gate = process.set_pending("cargo build", CommandOutput::default()); // held until gate.release()

assert_eq!(process.calls(), ["git rev-parse --abbrev-ref HEAD"]);
let call = &process.call_details()[0];              // TestCall: command_line, current_dir, vars, options
assert_eq!(call.options.input.as_deref(), Some(b"msg".as_slice()));
assert_eq!(call.var("KEY"), Some(OsStr::new("value")));
assert_eq!(process.sleeps(), [Duration::from_secs(5)]); // retry delays
process.clear_calls();
```

### TestRequest

Patterns are substrings of `"METHOD URL"` (`"POST https://x/y"`, `"/releases/"`, `""`). The most recently set one wins.

```rust
let request = TestRequest::new();
request.set_response("GET https://example.com/a", RequestOutput { status: 200, body: b"ok".to_vec(), ..Default::default() });
request.set_responses("/latest", [status(503), status(429), status(200)]);  // one per request, last repeated
request.set_response_if(|call| call.body.starts_with(b"query"), output);    // match on TestRequestCall
request.set_transport_error("", TransportErrorKind::Connect);              // server down
request.set_chunks("/big", status(200), [b"ab".to_vec(), b"c".to_vec()]); // body in chunks
request.set_interrupted("/big", status(200), [b"half".to_vec()], TransportErrorKind::Other); // drops mid-body
let gate = request.set_pending("", status(200));    // held until gate.release() / gate.fail(kind)
request.set_client_failure(true);                   // blocking_client()/client() fail

assert_eq!(request.calls(), ["GET https://example.com/a"]);
let call = &request.call_details()[0];              // TestRequestCall: method, url, headers, body, options
assert_eq!(call.headers["authorization"], "Bearer secret");
assert_eq!(request.sleeps(), [Duration::from_millis(100), Duration::from_millis(200)]);
```

### Async tests

`TestProcess` and `TestRequest` answer async runs immediately, so a current-thread runtime is enough. Use `set_pending` gates to observe in-flight work or cancellation.

```rust
#[tokio::test]
async fn run_all_should_pass() {
    let platform = Arc::new(test_platform());
    platform.process.set_output("", CommandOutput::default());
    run_all(&platform).await;
}
```

### Real files

```rust
let dir = TestDir::new();                           // empty dir in the current dir named by the test, removed on drop (kept on panic)
let dir = TestDir::temp();                          // same, in the temp dir
let file = TestFile::new();                         // path named by the test (not created), removed on drop
std::fs::write(file.path(), "x")?;
```
