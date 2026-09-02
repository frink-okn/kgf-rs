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
#   docker build --build-arg HDTC_REF=v1.2.0-beta.3 -t kgf .
#   docker run --rm -v /bundles:/bundles kgf serve --bundle-root /bundles --bind 0.0.0.0:8080
#   docker run --rm -v /bundles:/bundles kgf build --config - \
#     --out /bundles/dreamkg/2026-06-01 --hdt /in/graph.hdt

# Must match rust-toolchain.toml. The build honours that file regardless, so a
# mismatch here only costs a second toolchain download inside the image.
ARG RUST_VERSION=1.94.1
# The hdtc release this image is built from and tested against.
ARG HDTC_REF=v1.2.0-beta.3

FROM rust:${RUST_VERSION}-bookworm AS build
ARG HDTC_REF

RUN apt-get update \
    && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*

# The sibling layout `Cargo.toml`'s `path = "../hdtc"` expects, and the same one
# CI's two checkouts produce.
WORKDIR /src
RUN git clone --depth 1 --branch "${HDTC_REF}" https://github.com/frink-okn/hdtc.git hdtc

COPY . /src/kgf-rs

# `--locked` is the guard that makes the pairing real: kgf-rs's Cargo.lock
# records the hdtc version it was resolved against, so a HDTC_REF whose
# Cargo.toml version disagrees fails the build here rather than shipping.
WORKDIR /src/kgf-rs
RUN cargo build --release --locked --bin kgf

WORKDIR /src/hdtc
RUN cargo build --release --locked --bin hdtc

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 kgf

COPY --from=build /src/kgf-rs/target/release/kgf /usr/local/bin/kgf
COPY --from=build /src/hdtc/target/release/hdtc /usr/local/bin/hdtc

# `kgf build` resolves `hdtc` from PATH by default, so the two find each
# other with no flag.
USER kgf
WORKDIR /home/kgf

# The service descriptor at `/` is the health probe: it opens no bundle, so no
# `/healthz` is needed.
EXPOSE 8080
ENTRYPOINT ["kgf"]
CMD ["--help"]
