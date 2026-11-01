# Contributing

## Layout

- [crates/server](crates/server): the `hylla` binary and the server.
- [crates/config](crates/config): loading and validating the config, with
  [ordna](https://github.com/cstruct/ordna).
- [crates/control](crates/control): the control plane's UI, a [Leptos](https://leptos.dev)
  app rendered on the server and hydrated in the browser.
- [docs/spec](docs/spec): the specified behaviour. [docs/guide](docs/guide): the user guides.
- [examples](examples): configs for common setups, loaded and validated by the tests.

[mise](https://mise.jdx.dev) installs the pinned toolchain and tools, from
[mise.toml](mise.toml):

```bash
mise install
```

## Building

```bash
cargo build --release -p hylla-server
```

The control plane UI's hydration bundle is embedded in the binary when it's built first,
into `target/site/pkg`, as releases and the Docker image do:

```bash
mise run ui
```

## Testing

```bash
cargo test
```

The end-to-end tests run the `hylla` binary against
[VersityGW](https://github.com/versity/versitygw) (S3) and
[Azurite](https://github.com/Azure/Azurite) (Azure Blob Storage), and three nodes sharing
over a cluster. mise installs both:

```bash
mise run e2e
```

CI also checks formatting, runs clippy on the workspace and on the UI's WASM build, and
validates the spec references:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p hylla-control --lib --target wasm32-unknown-unknown --features hydrate -- -D warnings
mise run deny
```

`mise run deny` checks the dependencies with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny),
by [deny.toml](deny.toml): security advisories, licenses and sources. An advisory that
doesn't apply is ignored there, with why. Dependabot proposes updates weekly.

## The spec

The behaviour is specified in [docs/spec](docs/spec), as requirements such as
`r[server.routes]`, and tracked against the code with
[tracey](https://github.com/bearcove/tracey): code implementing a requirement is marked
`// r[impl server.routes]`, and tests verifying it `// r[verify server.routes]`. Every
requirement needs both.

```bash
tracey query status
tracey query validate
tracey query untested
```

When behaviour changes, change the spec with it, and the [guides](docs/guide) and
[examples](examples) where users would notice.

## Developing with ordna

hylla's config is built on ordna, taken from crates.io. To build against a checkout of
ordna next to this one instead, while changing both, patch it in with
[.cargo/ordna-local.toml](.cargo/ordna-local.toml):

```bash
ln -s ordna-local.toml .cargo/config.toml
```

`.cargo/config.toml` is ignored by git. While it's enabled, Cargo.lock records the local
packages, so remove the link before committing changes to it.

## Releasing

hylla is versioned as one, by `version` in the workspace's [Cargo.toml](Cargo.toml). Bump
it, then tag the commit with the version:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The [release workflow](.github/workflows/release.yml) checks the tag against the version
and runs CI. It then builds the binaries, pushes the Docker image, and creates a GitHub
release with the binaries, their checksums and notes generated from the pull requests
merged since the last one.
