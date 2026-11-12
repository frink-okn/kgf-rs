# syntax=docker/dockerfile:1

# One image, both binaries.
#
# `kgf build` shells out to `hdtc`, and `kgf-store`
# links hdtc as a path dependency, so the two must be the same hdtc. Shipping
# them separately would let a deployment pair a builder with a format layer it
# was never tested against — and `.hdt.perm`, the sketch and key-set files, and
# the text index are pinned by convention rather than by commit, so that
# mismatch produces plausible artifacts rather than an error.
#
#   docker build --build-arg HDTC_REF=v1.3.2 -t kgf .
#   docker run --rm -v /bundles:/bundles kgf serve --bundle-root /bundles --bind 0.0.0.0:8080
#   docker run --rm -v /bundles:/bundles kgf build --config - \
#     --out /bundles/dreamkg/2026-06-01 --hdt /in/graph.hdt

# Must match rust-toolchain.toml. The build honours that file regardless, so a
# mismatch here only costs a second toolchain download inside the image.
ARG RUST_VERSION=1.94.1
# The hdtc release this image is built from and tested against.
ARG HDTC_REF=v1.3.2
# The tool that records each binary's dependencies in the binary; see below.
ARG CARGO_AUDITABLE_VERSION=0.7.7

# The same Debian release as the runtime image, so the binaries are linked
# against the glibc and gcc runtime they run with.
FROM rust:${RUST_VERSION}-trixie AS build
ARG HDTC_REF
ARG CARGO_AUDITABLE_VERSION

RUN apt-get update \
    && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*

# `cargo auditable build` compiles exactly what `cargo build` does and also
# embeds the resolved dependency list in each binary. Without it the binaries
# are opaque to an image scanner (Trivy, Grype, `cargo audit bin`), which then
# reports only the runtime image's OS packages and never a vulnerable crate.
RUN cargo install cargo-auditable --locked --version "${CARGO_AUDITABLE_VERSION}"

# Link liblzma and libbz2 from the sources their -sys crates bundle. Both crates
# otherwise prefer a system copy found through pkg-config, and this builder
# carries both, so `hdtc` would need them at run time from an image that has
# neither. lzma-sys reads the first variable; the second makes the pkg-config
# probe for bzip2 fail, which is bzip2-sys's only route to its bundled source.
ENV LZMA_API_STATIC=1 \
    BZIP2_NO_PKG_CONFIG=1

# The sibling layout `Cargo.toml`'s `path = "../hdtc"` expects, and the same one
# CI's two checkouts produce.
WORKDIR /src
RUN git clone --depth 1 --branch "${HDTC_REF}" https://github.com/frink-okn/hdtc.git hdtc

COPY . /src/kgf-rs

# `--locked` is the guard that makes the pairing real: kgf-rs's Cargo.lock
# records the hdtc version it was resolved against, so a HDTC_REF whose
# Cargo.toml version disagrees fails the build here rather than shipping.
WORKDIR /src/kgf-rs
RUN cargo auditable build --release --locked --bin kgf

WORKDIR /src/hdtc
RUN cargo auditable build --release --locked --bin hdtc

# Both binaries may load glibc and libgcc_s, which every Rust binary on this
# target needs, and no other shared library. Any other C library is linked
# statically, so neither binary depends on what else the runtime image happens
# to carry. A binary that breaks this would still build, then fail to start in
# the runtime image, so the check is made here, where it names the library.
RUN set -eu; \
    for bin in /src/kgf-rs/target/release/kgf /src/hdtc/target/release/hdtc; do \
      for lib in $(readelf -d "$bin" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do \
        case "$lib" in \
          libc.so.6|libm.so.6|libgcc_s.so.1) ;; \
          *) echo "$bin links $lib dynamically; only glibc and libgcc_s may be" >&2; exit 1 ;; \
        esac; \
      done; \
    done

# The runtime user's home, made here because the runtime image has no shell to
# make it with.
RUN mkdir -p /runtime/home/kgf

# glibc, the gcc runtime libraries, CA certificates and time zone data: what a
# dynamically linked Rust binary needs, with no shell, package manager or
# coreutils. Every OS package in the image is one a scanner reports against,
# so the ones neither binary ever runs are left out rather than patched. The
# tag follows Debian's security updates, so each build picks them up.
FROM gcr.io/distroless/cc-debian13 AS runtime

COPY --from=build /src/kgf-rs/target/release/kgf /usr/local/bin/kgf
COPY --from=build /src/hdtc/target/release/hdtc /usr/local/bin/hdtc
COPY --from=build --chown=10001:999 /runtime/home/kgf /home/kgf

# uid 10001 and gid 999 are what deployments see: deploy/gke runs the pod as
# them, and a host directory `kgf build` writes into must be writable by them.
# Neither has an /etc/passwd entry here, which neither binary reads, so HOME is
# set rather than looked up.
USER 10001:999
ENV HOME=/home/kgf
WORKDIR /home/kgf

# `kgf build` resolves `hdtc` from PATH by default, so the two find each
# other with no flag.

# `/healthz` is the health probe: it opens no bundle and is not part of the API,
# so a failed check means this process cannot answer rather than that some
# bundle is slow, and the server does not write an access record for an
# uneventful one.
EXPOSE 8080
ENTRYPOINT ["kgf"]
CMD ["--help"]
