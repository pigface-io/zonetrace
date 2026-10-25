# zonetrace

set shell := ["zsh", "-cu"]

# list recipes
default:
    @just --list

# install the git hooks (pre-commit and pre-push)
hooks:
    prek install

# every hook of both stages on every file: what CI runs
check:
    prek run --all-files
    prek run --all-files --stage pre-push

# rewrite files in place
fmt:
    cargo fmt --all

# fail on a formatting diff, without rewriting
fmt-check:
    cargo fmt --all -- --check

# clippy with warnings as errors, and the docs with broken links as errors
lint:
    cargo clippy --all-targets --locked -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked --quiet

# tests with line coverage, held to 100%
cover:
    cargo llvm-cov --locked --fail-under-lines 100

# what a zone costs, against the last saved baseline when there is one
bench:
    cargo bench --locked --bench overhead
