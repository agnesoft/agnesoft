# Agnesoft Central Repository

[![codecov](https://codecov.io/gh/agnesoft/agnesoft/graph/badge.svg?token=lzOPcH5K2d)](https://codecov.io/gh/agnesoft/agnesoft)

## Crates

### agplatform

<details>

<summary>Platform providing env, fs, request and process infrastructure for production and testing. Abstracted over a Platform trait it allows easy swapping production and test implementations.</summary>

```rust
fn read_file(p: impl agplatform::Platform) {
    let content = p.fs().read_to_string()?;
}
read_file(agplatform::platform()); // production implementation using std, tokio, reqwest
read_file(agplatform::test_platfrom()); // test in-memory implementation with abilities to simulate failures, manipulate time etc.
```

</details>

### agtestcheck

<details>

<summary>Cargo subcommand checking the structure of Rust tests binding the tests code in the production structure and enforcing the rules. See the [test rules](agtestcheck/rust-test-rules.md) for details.</summary>

```rust
// source.rs
fn foo() -> i32 { 1 }
struct S;
impl S { fn bar() -> Self { S } }

#[cfg(test)]
mod tests {
    use super::*;

    mod foo {
        #[test]
        fn return_1() { assert_eq(foo(), 1); }
    }

    mod s {
        mod bar {
            #[test]
            fn bar_should_crate_s() { /* testing stuff */}
        }
    }

    #[test]
    fn ranom_arbitrary_test() {}
}
```

Run

```
# cargo install agtestcheck
cargo test-check [<dir|file.rs>]
```

Possible output:

```
source.rs:12: test `return_1` should be named `foo_should_<behaviour>` or `foo_<args>_should_<behaviour>`
source.rs:18: comment "testing stuff" should be deleted, start with WHY or GIVEN, WHEN, THEN, AND
source.rs:23: test `ranom_arbitrary_test` should be in the module of its type, trait, function or method
```

</details>
