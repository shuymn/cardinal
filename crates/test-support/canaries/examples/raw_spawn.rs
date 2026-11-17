//! A raw spawn outside the spawn API: `clippy::disallowed_methods` must reject it.

fn main() {
    let _ = std::process::Command::new("true").status();
}
