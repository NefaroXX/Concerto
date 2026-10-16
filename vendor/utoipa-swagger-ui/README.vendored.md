# Vendored: utoipa-swagger-ui 9.0.2

This directory is a local copy of `utoipa-swagger-ui` **9.0.2** (upstream:
<https://github.com/juhaku/utoipa>, license MIT OR Apache-2.0, unchanged), used
via `[patch.crates-io]` from the workspace root `Cargo.toml`.

## Why it is vendored

Upstream 9.0.2 declares a build dependency on `zip ^3.0`. Every `zip` release
in that range is **yanked** on crates.io, as is `zip 8.6.0` (the only release in
the `^8.6` range pinned by upstream 10.x). The only unyanked `zip` is **9.0.3**.

We do not weaken the `yanked = "deny"` gate in `deny.toml`, so this crate is
patched locally to build against the unyanked `zip 9.0.3` instead.

## Local changes from upstream 9.0.2

Exactly two edits, both minimal:

1. `Cargo.toml` — the `[build-dependencies.zip]` requirement goes from
   `version = "3.0"` / `features = ["deflate"]` to `version = "9.0.3"` /
   `features = ["deflate-flate2-zlib-rs"]`.
   - `zip` declares its `flate2` dependency with `default-features = false`,
     so a backend must be named explicitly. `deflate-flate2-zlib-rs` selects
     `flate2/zlib-rs`, and `zlib-rs` is already in the workspace lock (no new
     crate is introduced). The default `deflate` feature in `zip 9` would
     instead pull `zopfli`, and the build script never compresses anything —
     it only reads deflate entries out of the vendored Swagger UI archive.
2. `build.rs` — `file.name().ends_with('/')` becomes `file.name()?.ends_with('/')`.
   `ZipFile::name` changed from `&str` (3.x) to `ZipResult<Cow<'_, str>>` (9.x);
   this is the only 3.x→9.x API difference the build script touches.

Everything else, including the embedded offline Swagger UI assets from
`utoipa-swagger-ui-vendored` (no runtime CDN), is upstream and unmodified.

To refresh on a future utoipa-swagger-ui bump: take the released crate source,
re-apply these two edits, and update this note.
