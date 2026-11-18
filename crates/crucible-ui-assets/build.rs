//! With `embed-ui`, rebuilds the crate whenever anything under `ui/out` changes. The embedding derive tracks
//! only the files that existed when it last ran, so without this a file added later (such as the `.br` and
//! `.gz` variants that `cargo xtask ui compress` writes) would be missing from the next build.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if cfg!(feature = "embed-ui") {
        println!("cargo::rerun-if-changed=../../ui/out");
    }
}
