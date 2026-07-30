echo "Linting"
cargo clippy --all-targets --all-features --fix --allow-dirty

echo "Formatting"
cargo fmt

echo "Unit tests"
cargo test --all-features --lib

echo "Doc tests"
cargo test --all-features --doc

echo "Integration tests"
cargo test --all-features --test '*'
