//! The Crucible server binary. Subcommands are wired as their crates land.
#![allow(clippy::print_stdout)]

fn main() {
    println!("crucible {}", env!("CARGO_PKG_VERSION"));
}
