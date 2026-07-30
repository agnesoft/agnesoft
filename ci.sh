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

echo -ne "${GREEN}Doc tests${NONE} ... "
OUT=$(cargo test --all-features --doc 2>&1)
validate

echo -ne "${GREEN}Unit+integration tests${NONE} ... "
OUT=$(cargo llvm-cov --all-features --show-missing-lines --no-report 2>&1)
validate

echo -ne "${GREEN}Coverage${NONE} ... "
OUT=$(cargo llvm-cov report --fail-uncovered-lines 0 --fail-uncovered-regions 0 --fail-uncovered-functions 0 2>&1)
if [ $? -ne 0 ]; then
    echo -e "${RED}FAILED${NONE}"
    echo "$OUT"
    cargo llvm-cov report --html --open
    exit 1
else
    echo -e "${GREEN}OK${NONE}"
fi
