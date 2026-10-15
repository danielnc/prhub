# The OSS-Fuzz Rust builder (nightly Rust and cargo-fuzz included), pinned by digest; Dependabot keeps it current.
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f
COPY . $SRC/prhub
WORKDIR $SRC/prhub
COPY .clusterfuzzlite/build.sh $SRC/
