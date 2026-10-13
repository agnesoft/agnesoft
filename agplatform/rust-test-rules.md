# Rust Test Rules

Unit tests should have 100 % function, region and line coverage with structured tests. Rust has no built-in test grouping — `#[test]` functions are flat by default, which leads to either too many tiny tests or too few monolithic ones. These rules use predictable nested `mod` blocks based on actual code types inside `#[cfg(test)]` as a grouping mechanism. The test functions themselves are named uniformly as `<subject>_should_<verb>_<outcome>`.

| Rule               | Detail                                                                                                                                                                                                                                                                                                              |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Module names       | Real types, methods, or standalone functions only                                                                                                                                                                                                                                                                   |
| Nesting            | `type → method → test fn`; standalone functions skip the type level                                                                                                                                                                                                                                                 |
| Cross-method tests | Direct `#[test]` on the type module, before method sub-modules                                                                                                                                                                                                                                                      |
| Test names         | `subject_should_verb`; subject typically repeats module name                                                                                                                                                                                                                                                        |
| Split criterion    | New test per distinct **behaviour**, particularly, not per input variation — see section below                                                                                                                                                                                                                      |
| State              | Each test self-contained; no/minimal shared setup unless needed due to complexity                                                                                                                                                                                                                                   |
| Comments           | **Never** comment test code. The two exceptions: explaining something not present in the code itself (e.g. _why_ a combination is invalid, a platform/spec constraint) or Given/When/Then as detailed below.                                                                                                        |
| Simplify           | After naming a test, verify the body does only what the name claims — drop steps that only existed because the old test was monolithic; use the simplest available verification path. Coverage may decrease — this is expected; it exposes methods that were only incidentally covered and now need their own tests |

## Overview

Tests live inside `#[cfg(test)] mod tests` and are grouped by **type → method** or **standalone function**. Every module name must correspond to a real type, method, or standalone function in the source — never an invented concept.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // type → its tests
    mod cache {
        use super::*;

        // cross-method test (directly on the type, before method modules)
        #[test]
        fn cache_should_evict_oldest_when_full_and_inserted() {
            let mut c = Cache::new(2);
            c.insert("a", 1);
            c.insert("b", 2);
            c.insert("c", 3);
            assert_eq!(c.get("a"), None);
            assert_eq!(c.get("c"), Some(&3));
        }

        // method → its tests
        mod get {
            use super::*;

            #[test]
            fn get_should_return_none_for_missing_key() {
                let c = Cache::new(4);
                assert_eq!(c.get("x"), None);
            }

            #[test]
            fn get_should_return_value_for_present_key() {
                let mut c = Cache::new(4);
                c.insert("k", 42);
                assert_eq!(c.get("k"), Some(&42));
            }
        }

        mod insert {
            use super::*;

            #[test]
            fn insert_should_store_new_entry() {
                let mut c = Cache::new(4);
                c.insert("k", 1);
                assert_eq!(c.get("k"), Some(&1));
            }

            #[test]
            fn insert_should_overwrite_existing_key() {
                let mut c = Cache::new(4);
                c.insert("k", 1);
                c.insert("k", 2);
                assert_eq!(c.get("k"), Some(&2));
            }
        }
    }

    // standalone function → its tests (no type wrapper)
    mod normalize_path {
        use super::*;

        #[test]
        fn normalize_path_should_resolve_parent_references() {
            assert_eq!(normalize_path("/a/b/../c"), "/a/c");
        }

        #[test]
        fn normalize_path_should_strip_trailing_slash() {
            assert_eq!(normalize_path("/a/b/"), "/a/b");
        }
    }
}
```

## Module names

Every module corresponds to a real **type**, **method**, or **standalone function** in the source. Never invent concept modules like `lifecycle`, `workflow`, or `edge_cases`. If a behaviour spans multiple methods of the same type, it belongs to that type — not to a made-up grouping.

## Nesting

Three levels: `type → method → test fn`. Standalone functions skip the type level because they are their own subject. Even a method with a single test gets its own module — consistency over compactness.

## Cross-method tests

Tests that exercise interaction between multiple methods sit directly on the type module as `#[test]` functions and come **before** the method sub-modules. See `cache_should_evict_oldest_when_full_and_inserted` above — it tests `insert` and `get` together, so it lives on `mod cache`, not in either method module. For genuine cross-type integration, use integration tests (`tests/`).

## Naming: `subject_should_verb`

Every test function is named `subject_should_verb_phrase`. The subject repeats the innermost module name. This duplication is intentional — modules give structure in source; the prefix gives context in flat `cargo test` output. Use **should**, not **is** or bare verbs. The name is a claim about behaviour.

## Split on different behaviour or code path, not on input

Split into a new test only when the **expected behaviour or code path** differs. Prefer matrix of inputs/outputs in one test where possible. Don't duplicate an entire test body just to change the input.

**What counts as the same behaviour (one, possibly matrixed, test):** e.g. unquoting different inputs, resolving different path inputs, writing different kinds of data etc.
**What counts as different behaviour (separate tests):** e.g. resolves relative paths vs reject invalid paths, accepts missing key vs reject invalid key, reading files vs writing files etc.

The combinations of assertions from these are valid (e.g. write file to assert its contents via read) but the tests themselves should be split on different behaviour - asserting various forms of read content vs various form of writing content.

A loop or `(input, expected)` tuples keeps same-behaviour tests DRY:

```rust
#[test]
fn parse_bool_should_accept_truthy_representations() {
    for input in ["1", "true", "TRUE", "yes", "Yes", "on"] {
        assert_eq!(parse_bool(input), Ok(true), "input: {input}");
    }
}

// Separate test — different outcome
#[test]
fn parse_bool_should_reject_invalid_values() {
    for (input, err) in [("", Empty), ("maybe", Unknown), ("2", Unknown)] {
        assert_eq!(parse_bool(input), Err(err), "input: {input}");
    }
}
```

Use an inner helper when per-case setup is more than a one-liner:

```rust
#[tokio::test]
async fn validate_should_accept_known_formats() {
    async fn check(input: &str, expected: bool) {
        assert_eq!(validate(input).await, expected, "input: {input}");
    }
    check("json", true).await;
    check("yaml", true).await;
    check("xml",  false).await;
}
```

## Test body style: Given / When / Then

Use **GIVEN / WHEN / THEN** comments when the test has non-trivial setup — multi-step wiring, mocks, filesystem scaffolding — so a reader can tell at a glance where setup ends and the action begins:

```rust
#[tokio::test]
async fn connect_should_retry_after_transient_failure() {
    // GIVEN a server that fails once then succeeds
    let server = MockServer::start().await;
    server.enqueue_failure(503);
    server.enqueue_success();

    // WHEN connecting with retry enabled
    let client = Client::new(server.url(), RetryPolicy::Once);
    let result = client.connect().await;

    // THEN the connection succeeds on the second attempt
    assert!(result.is_ok());
    assert_eq!(server.request_count(), 2);
}
```

Simple tests where structure is self-evident don't need G/W/T comments — the matrix or a few lines of setup + assert are clear enough on their own.

## Trait implementations

When a trait has multiple implementations (e.g. a real one and an in-memory mock), their tests often share the same claims. This is fine — the setup differs even though the claim is identical, and the module path disambiguates.

## Self-contained tests

Each test builds its own state. No shared setup across tests, no test ordering dependencies. Local helper functions within a module are fine for reducing boilerplate.

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
