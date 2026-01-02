# hash-crate

`hash-crate` is a fast, deterministic hashing tool for Rust crates and their transitive dependencies.

It is designed for CI environments and large workspaces where you want to:

- Cache prebuilt binaries for local crates

- Detect when a crate actually needs to be rebuilt

- Avoid over-invalidating caches due to unrelated changes in a workspace

- Scale to hundreds or thousands of dependencies

- The output hash is intended to be stable across machines and independent of filesystem layout.

## Why?

Cargo does not provide a built-in way to compute a content-based hash for a single crate and only the dependencies it actually uses.

`hash-crate` solves this by:

- Resolving the per-crate transitive dependency graph

- Hashing the contents of all path dependencies

- Including version/source information for non-path dependencies

- Producing a single deterministic hash suitable for cache keys

This makes it ideal for:

- Large monorepos

- xtask-style tooling (without needing cargo xtask)

Features

- Very fast (uses Rayon + BLAKE3)

- Deterministic (order-independent, thread-safe)

- Per-crate granularity

Handles:

- Path dependencies

- Workspace members

- [patch] overrides

- Deduplicated dependencies

## Installation

```sh
cargo binstall hash-crate
```

## Usage

```sh
hash-crate <package-name>
```

Example:

```sh
hash-crate xtask-build
```

Output:

```text
9e8b7a4b1d9c4b6b3f8c6c1a1a4e0d9e7c9a5f0c2c0c3a7e8c6b9e1f4d2
```

This hash represents:

- The target crate

- All of its transitive dependencies

- The contents of any path dependencies

## What affects the hash?

The hash will change if any of the following change:

- Files inside a path dependency

- Dependency versions

- Git revisions

- Dependency graph structure

- Cargo patch overrides

The hash should not depend on:

- Absolute filesystem paths

- Dependency resolution order

- Thread scheduling

- Host machine

## Intended use in CI

Typical pattern:

1. Compute the hash:

   ```sh
   HASH=$(hash-crate xtask-build)
   ```

1. Use it as an artifact key:

   ```sh
   # for some local crate called `xtask-build`
   xtask-build-${HASH}-${RUNNER_TYPE}.tar.gz
   ```

1. If the artifact exists:

   - Download and run it

1. Otherwise:

   - Build the crate

   - Upload the artifact

Invalidation is automatic.

## Performance

`hash-crate` is optimized for large workspaces:

- Parallel directory hashing using Rayon and BLAKE3

- Streaming dependency resolution

- Deterministic folding of results

On large applications (1000+ dependencies), runtime is typically dominated by cargo metadata generation.

## Non-goals

- Replacing Cargo’s dependency resolver

- Being human-readable

- Supporting partial or heuristic hashing

- This tool is intentionally strict and conservative.

## Note

This README was originally written using AI. The crate was not.
