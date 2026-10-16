# Rust Test Rules

Unit tests mirror the code: inside `#[cfg(test)] mod tests`, nested modules name the real types, traits, methods and functions under test, and each test is named `<subject>[_<args>]_should_<behaviour>`. Rust has no built-in test grouping, and generated (LLM) tests in particular drift into inconsistent structure and noisy comments, so the structural rules are enforced deterministically by `agtestcheck` rather than left to judgement. Coverage must be 100 % (see [Coverage](#coverage)).

Checked by `agtestcheck`:

- Modules name real types, traits, methods and functions only: `type → method`, `type → trait → method` or a standalone function.
- Tests are named `<module>[_<args>]_should_<behaviour>`.
- Cross-method tests sit directly in the type module, before its method modules.
- Comments in test bodies are only `GIVEN` / `WHEN` / `THEN` / `AND` steps or `WHY` explanations.

Checked by `cargo llvm-cov`:

- 100 % function, line and region coverage.

Left to judgement:

- Split tests on behaviour, not input; use a matrix for inputs of the same behaviour.
- Each test is self-contained and does only what its name claims.

## Running agtestcheck

```bash
# Install if not present
cargo install agtestcheck
cargo test-check [-- <dir | file.rs>]   # defaults to the current directory
```

Every `.rs` file is checked, except integration tests (`tests/` next to a `Cargo.toml`). Each issue is printed as `file:line: message` and any issue makes the exit code 1.

A test is a `#[test]` function or, within `#[cfg(test)]` code, a function with any attribute ending in `test` (e.g. `#[test]`, `#[tokio::test]`). Modules without tests (helpers) are ignored.

## Structure

```rust
struct Cache { /* ... */ }
impl Cache { fn get(..) {..} fn insert(..) {..} }
impl std::fmt::Display for Cache { fn fmt(..) {..} }
fn normalize_path(..) {..}

#[cfg(test)]
mod tests {
    use super::*;

    mod cache {                                   // type
        use super::*;

        #[test]                                   // cross-method test, before method modules
        fn cache_should_evict_oldest_when_full() { /* insert + get */ }

        mod get {                                 // method
            use super::*;

            #[test]
            fn get_should_return_inserted_value() { /* ... */ }

            #[test]
            fn get_missing_key_should_return_none() { /* <args> qualify the subject */ }
        }

        mod std_fmt_display {                     // trait implemented by the type
            use super::*;

            mod fmt {                             // trait method
                use super::*;

                #[test]
                fn fmt_should_list_entries() { /* ... */ }
            }
        }
    }

    mod normalize_path {                          // standalone function or macro
        use super::*;

        #[test]
        fn normalize_path_should_resolve_parent_references() { /* ... */ }
    }
}
```

- **Modules** name only items of the same file, plus the default methods of traits the file's types implement: `type → method`, `type → trait → method`, or a standalone function / `macro_rules!` macro. Names are snake cased (`FsImpl` → `fs_impl`). Never invent concept modules (`lifecycle`, `edge_cases`). A method gets its own module even for a single test.
- **Tests** live in the module of their subject and are named after it: `<module>_should_…` or `<module>_<args>_should_…`. The name repeats the module so flat `cargo test` output stays readable.
- **Cross-method tests** of a type sit directly in the type module, before its method modules. Cross-type tests belong in integration tests (`tests/`).
- **Trait modules** follow the trait path as written, snake cased with `::` → `_` and generics dropped: `impl fmt::Write` → `fmt_write`, `impl From<A>` and `impl From<B>` share `from`. This keeps an inherent `fmt` apart from `Display::fmt`, and `Debug::fmt` apart from `Display::fmt`.
  - Trait methods are tested in the implementer, overridden or not: a default method the type does not override still goes in `type → trait → method`. The trait may be defined in another file.
  - A trait implementation with at most one method (`impl Drop`, `impl Error for E {}`) may hold its tests directly in the trait module, named after the trait or the method (`x::drop::drop_should_…`, `x::std_fmt_display::fmt_should_…`).
  - Blanket implementations (`impl<T: Bound> Trait for T`) and implementations for types not defined in the file (`impl MyTrait for Vec<u8>`) are tested under the trait at the top level: `trait → method`.

## Comments

Inside test bodies a comment must be either:

- a step: starts with `GIVEN`, `WHEN`, `THEN` or `AND` (`// GIVEN …`, `// GIVEN: …`). `AND` cannot come first and `GIVEN` cannot follow `WHEN` or `THEN`. A step may wrap over consecutive `//` lines.
- a `WHY` comment explaining what the code cannot show, e.g. a platform or spec constraint: `// WHY: POSIX rejects opening a file with both append and truncate`.

Any other comment is reported: delete it, or rewrite it as `WHY` if it carries meaning not present in the code.

Use steps when the setup is non-trivial (mocks, multi-step wiring, filesystem scaffolding) so it is clear where setup ends and the action begins:

```rust
#[tokio::test]
async fn connect_should_retry_after_transient_failure() {
    // GIVEN a server that fails once then succeeds
    let server = MockServer::start().await;
    server.enqueue_failure(503);
    server.enqueue_success();

    // WHEN connecting with retry enabled
    let result = Client::new(server.url(), RetryPolicy::Once).connect().await;

    // THEN the connection succeeds on the second attempt
    assert!(result.is_ok());
    assert_eq!(server.request_count(), 2);
}
```

## Left to judgement

- **Split on behaviour, not input.** A new test only when the expected behaviour or code path differs (resolving relative paths vs rejecting invalid ones). Different inputs to the same behaviour go into one test as a matrix, with the input in the assertion message:

  ```rust
  #[test]
  fn parse_bool_should_accept_truthy_representations() {
      for input in ["1", "true", "TRUE", "yes", "on"] {
          assert_eq!(parse_bool(input), Ok(true), "input: {input}");
      }
  }
  ```

  Use a local `fn check(input, expected)` when per-case setup is more than a one-liner.

- **Self-contained.** Each test builds its own state; no shared fixtures or ordering dependencies. Local helper functions are fine.
- **Do what the name claims.** The body does only what its name claims, using the simplest verification path. Losing coverage when splitting a monolithic test is expected: it exposes methods that were only covered incidentally and need their own tests.
- **Same claims, different setup.** Multiple implementations of a trait (real and in-memory) may have tests with identical names; the module path tells them apart.

## Coverage

100 % function, line, and region coverage is enforced.

```bash
# Summary
cargo llvm-cov --all-features

# HTML with per-instantiation breakdown
cargo llvm-cov --all-features --html --open --show-instantiations

# CI gate — exits 1 on any uncovered line, function, or region
cargo llvm-cov --all-features \
  --fail-uncovered-lines 0 \
  --fail-uncovered-functions 0 \
  --fail-uncovered-regions 0
```

Lines and functions use a **merged** view — covered if any instantiation covers them. Regions are **not** merged — each monomorphized copy is counted individually. `--fail-uncovered-regions 0` catches partially-covered instantiations even when source lines look green.

### Monomorphization and coverage

When tests call the same generic function with different concrete types, the compiler emits a separate copy for each. Each copy has its own regions. If different tests exercise different code paths through different instantiations, each copy is only partially covered — the summary reports missed regions, but the HTML merges instantiations when rendering, so the source looks green. You see "Missed Regions: 3" and it could be difficult to discern which region in which instantiation is uncovered in the HTML view.

```rust
// Bad — creates two instantiations, each partially covered:
Error::fs("literal");           // instantiates Error::fs::<&str>
Error::fs(format!("dynamic"));  // instantiates Error::fs::<String>

// Good — one instantiation, must be fully covered:
Error::fs(format!("literal"));  // always String
Error::fs(format!("dynamic"));  // always String
```

**Fix:** use a single concrete type per generic parameter across all tests, so only one instantiation exists and it must be fully covered. If multiple instantiations are intentional, ensure every code path is exercised for each type independently.

### Diagnosing uncovered regions with the JSON export

When the CI gate fires on regions but the HTML shows all lines green, the JSON export is the way to find which instantiation and which lines are uncovered programmatically.

The JSON `data[0].functions` array has one entry per **instantiation** (not per source function). Each has a mangled `name` (demangle with `llvm-cxxfilt`), `filenames`, `count` (times entered), and `regions` — tuples of `[line_start, col_start, line_end, col_end, execution_count, file_id, expanded_file_id, kind]`. A region with `execution_count == 0` and `kind == 0` (Code) is the uncovered region.

This script groups instantiations by base function and prints only functions that have at least one uncovered instantiation (covered instantiations are shown for context):

```bash
cargo llvm-cov --json --all-features 2>/dev/null \
  | python3 -c "
import json, sys, subprocess, os
from collections import defaultdict

data = json.load(sys.stdin)
functions = data['data'][0]['functions']
cwd = os.getcwd()

# Demangle all symbols in one batch
all_names = '\n'.join(f['name'] for f in functions)
r = subprocess.run(['llvm-cxxfilt'], input=all_names,
                   capture_output=True, text=True)
demangled_list = r.stdout.strip().split('\n')
demangled = dict(zip((f['name'] for f in functions), demangled_list))

# Group by base function (strip type parameters)
groups = defaultdict(list)
for f in functions:
    dm = demangled[f['name']]
    base = dm.split('::<')[0] if '::<' in dm else dm
    uncov = [r for r in f['regions'] if r[4] == 0 and r[7] == 0]
    raw = f['filenames'][0] if f.get('filenames') else '?'
    rel = os.path.relpath(raw, cwd) if raw != '?' else raw
    groups[base].append({
        'demangled': dm,
        'file': rel,
        'count': f['count'],
        'uncovered': uncov,
    })

# Report only problems
for base, instances in sorted(groups.items()):
    problems = [i for i in instances if i['count'] == 0 or i['uncovered']]
    if not problems:
        continue
    print(f'{base} ({len(instances)} instantiations)')
    for inst in instances:
        marker = '✗' if inst['count'] == 0 else ('⚠' if inst['uncovered'] else '✓')
        print(f'  {marker} {inst[\"demangled\"]}')
        print(f'    file={inst[\"file\"]}  count={inst[\"count\"]}  '
              f'uncovered_regions={len(inst[\"uncovered\"])}')
        for r in inst['uncovered']:
            print(f'      line {r[0]}:{r[1]}-{r[2]}:{r[3]}')
    print()
"
```

Typical output:

```
<Error>::fs (7 instantiations)
  ✓ <Error>::fs::<String>
    file=src/error.rs  count=24  uncovered_regions=0
  ✓ <Error>::fs::<&str>
    file=src/error.rs  count=1   uncovered_regions=0
  ✗ <Error>::fs::<FromUtf8Error>
    file=src/error.rs  count=0   uncovered_regions=6
      line 62:5-62:60
      line 63:9-63:18
      line 63:19-63:32
      line 63:34-63:45
      line 63:47-63:51
      line 64:5-64:6
  ✗ <Error>::fs::<String>
    file=src/error.rs  count=0   uncovered_regions=6
      line 62:5-62:60
      ...
```

If one instantiation has all regions covered but another has `count=0` — it's a monomorphization split. Consolidate types or cover all regions for all instantiated types. If **all** instantiations miss the same region — it's a real gap. Write a test.
