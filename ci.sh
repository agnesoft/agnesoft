#!/usr/bin/env bash

export CARGO_TERM_COLOR=always

GREEN='\033[0;32m'
RED='\033[0;31m'
NONE='\033[0m'

function validate() {
    if [ $? -ne 0 ]; then
        echo -e "${RED}FAILED${NONE}"
        echo "$OUT"
        exit 1
    else
        echo -e "${GREEN}OK${NONE}"
    fi
}

echo -ne "${GREEN}Linting${NONE} ... "
OUT=$(cargo clippy --all-targets --all-features --fix --allow-dirty 2>&1)
validate

echo -ne "${GREEN}Formatting${NONE} ... "
OUT=$(cargo fmt 2>&1)
validate

echo -ne "${GREEN}Warnings as errors${NONE} ... "
OUT=$(cargo clippy --all-targets --all-features -- -D warnings 2>&1)
validate

echo -ne "${GREEN}Test rules${NONE} ... "
OUT=$(cargo run -r -p agtestcheck 2>&1)
validate

echo -ne "${GREEN}Doc tests${NONE} ... "
OUT=$(cargo test --all-features --doc 2>&1)
validate

echo -ne "${GREEN}Benchmarks${NONE} ... "
OUT=$(cargo bench 2>&1)
validate

CRATES=$(for MANIFEST in */Cargo.toml; do dirname "$MANIFEST"; done)

for CRATE in $CRATES; do
    OTHERS=$(echo "$CRATES" | grep -vx "$CRATE" | paste -sd '|')
    IGNORE=()

    if [ -n "$OTHERS" ]; then
        IGNORE=(--ignore-filename-regex "/($OTHERS)/")
    fi

    cargo llvm-cov clean --workspace > /dev/null 2>&1

    echo -ne "${GREEN}Unit+integration tests${NONE} ($CRATE) ... "
    OUT=$(cargo llvm-cov -p "$CRATE" --all-features --show-missing-lines --no-report 2>&1)
    validate

    echo -ne "${GREEN}Coverage${NONE} ($CRATE) ... "
    OUT=$(cargo llvm-cov report -p "$CRATE" "${IGNORE[@]}" --fail-uncovered-lines 0 --fail-uncovered-regions 0 --fail-uncovered-functions 0 2>&1)
    if [ $? -ne 0 ]; then
        echo -e "${RED}FAILED${NONE}"
        echo "$OUT"
        cargo llvm-cov report -p "$CRATE" "${IGNORE[@]}" --html --open --show-instantiations
        exit 1
    else
        echo -e "${GREEN}OK${NONE}"
    fi
done
