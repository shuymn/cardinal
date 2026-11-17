//! A spawnable command from `assert_cmd`'s `CommandCargoExt` trait on std's `Command`:
//! `clippy::disallowed_methods` must reject it.

use assert_cmd::cargo::CommandCargoExt;

fn main() {
    let _ = std::process::Command::cargo_bin("canary");
}
