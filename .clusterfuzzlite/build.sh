#!/bin/bash -eu
# Build every fuzz/fuzz_*.py target into a standalone fuzzer (compile_python_fuzzer comes with the base image).
pip3 install .
for fuzzer in "$SRC"/prhub/fuzz/fuzz_*.py; do
  compile_python_fuzzer "$fuzzer"
done
