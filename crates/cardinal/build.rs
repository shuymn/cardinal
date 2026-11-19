//! Names the source commit in `cardinal --version` (`docs/architecture.md` section 11.2):
//! sets `CARDINAL_VERSION` to the package version followed by HEAD's commit when the crate
//! is built from a checkout of cardinal, and to the package version alone otherwise. The
//! binary reads it at compile time and starts no process to answer `--version`.
//!
//! Cargo runs this script again when a file of [`commit::Source::watch`] changes (a commit,
//! a checkout, refs packed), when one of the binary's own inputs ([`INPUTS`]) does, which
//! also refreshes the `-dirty` mark, and when `PATH` does, which decides whether git is
//! found; a change to other tracked files alone (docs, tests) shows at the next such run.

#[path = "build/commit.rs"]
mod commit;

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

/// The binary's own inputs, relative to the manifest directory. Only those that exist are
/// watched: a crate package has no workspace manifest, and cargo reruns a build script on
/// every build while a path it watches is missing.
const INPUTS: &[&str] = &[
    "build.rs",
    "build",
    "src",
    "Cargo.toml",
    "../../Cargo.toml",
    "../../Cargo.lock",
];

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);
    let env = commit::git_env(std::env::vars_os(), cfg!(windows));
    let source = commit::read(manifest, |args| git(manifest, &env, args));
    for input in INPUTS.iter().filter(|input| manifest.join(input).exists()) {
        println!("cargo::rerun-if-changed={input}");
    }
    println!("cargo::rerun-if-env-changed=PATH");
    for path in &source.watch {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    println!(
        "cargo::rustc-env=CARDINAL_VERSION={}",
        commit::version(env!("CARGO_PKG_VERSION"), source.commit.as_ref())
    );
}

/// `git <GIT_OPTIONS> <args>` in `dir` with `env` alone: its stdout when it exits 0, `None`
/// when it cannot start (no git on `PATH`), fails, or prints something other than UTF-8.
fn git(dir: &Path, env: &[(OsString, OsString)], args: &[&str]) -> Option<String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the build script asks git for the source commit; it cannot use the crate's \
                  spawn sites, and starts git with a constructed environment as they do"
    )]
    let output = std::process::Command::new("git")
        .args(commit::GIT_OPTIONS)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
