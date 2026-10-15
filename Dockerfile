# syntax=docker/dockerfile:1

# Keep the Rust version in sync with mise.toml.
FROM rust:1.99.0-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

# The dependencies, from the manifests alone, so they're only rebuilt when those change.
FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin hylla
# Distroless has no shell to create the disk cache's directory with.
RUN mkdir /cache

# glibc and CA certificates, as the build's Debian has them, and no shell.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/target/release/hylla /usr/local/bin/hylla
# For a disk cache at /var/cache/hylla: a volume mounted there starts out owned by the
# nonroot user (65532) that hylla runs as.
COPY --from=build --chown=65532:65532 /cache /var/cache/hylla
# Mount the config here, and the secrets file next to it if any.
WORKDIR /etc/hylla
ENTRYPOINT ["/usr/local/bin/hylla"]
CMD ["--config", "/etc/hylla/config.styx"]
