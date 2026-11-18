//! Serves the static UI export (contract 2.13 and 12.1).
//!
//! [`Catalog`] is the [`AssetSource`](crucible_env::AssetSource) behind every source: the export embedded at
//! build time (feature `embed-ui`), an export read from the `ui` disk root (`crucible serve --ui-dir`), or
//! nothing, which is the default build. [`serve`] is the try-files handler, identical for all three.
//!
//! # Precompressed variants
//!
//! `cargo xtask ui compress` writes `F.br` and `F.gz` beside each compressible file `F` of an export. A
//! catalog pairs them with `F` and never serves them under their own names. The step runs in `xtask` rather
//! than in a build script because it needs the filesystem, which product crates may not touch (clippy bans
//! it and the allow-attribute guard has no exemption for a build script here); because brotli at its highest
//! quality takes seconds, which belongs once per UI build rather than in every cargo build; and because the
//! same files then serve `--ui-dir`, so both sources hand the handler the same variants.

mod catalog;
mod csp;
mod serve;

pub use catalog::Catalog;
pub use serve::{Outcome, serve};
