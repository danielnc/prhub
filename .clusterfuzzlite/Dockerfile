# The OSS-Fuzz Python builder (atheris included), pinned by digest; Dependabot keeps the digest current.
FROM gcr.io/oss-fuzz-base/base-builder-python@sha256:756c7196c97d62a66c448e70bdeecce400e68ae2ee96d4393cb38d58ffca18d2
COPY . $SRC/prhub
WORKDIR $SRC/prhub
COPY .clusterfuzzlite/build.sh $SRC/
