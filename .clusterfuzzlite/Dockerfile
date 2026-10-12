# The OSS-Fuzz Python builder (atheris included), pinned by digest; Dependabot keeps the digest current.
FROM gcr.io/oss-fuzz-base/base-builder-python@sha256:21f6fab17ace4dc76e1c56de91ee95d3d03da021df94b41056ba036f6c6f55e9
COPY . $SRC/prhub
WORKDIR $SRC/prhub
COPY .clusterfuzzlite/build.sh $SRC/
