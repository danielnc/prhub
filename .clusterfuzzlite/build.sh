#!/bin/bash -eu
# Build every fuzz/fuzz_targets/*.rs target with cargo-fuzz (it comes with the base image) and hand them to the runner.
cd "$SRC/prhub"
cargo fuzz build -O
for target in fuzz/fuzz_targets/*.rs; do
  name=$(basename "$target" .rs)
  cp "fuzz/target/x86_64-unknown-linux-gnu/release/$name" "$OUT/"
done
