//! `MAGELLAN_VERSION`: the package version, plus `+<MAGELLAN_BUILD>` when set.
//!
//! Here and not `option_env!`: clap wants a `&'static str`, and a const cannot join an optional
//! one.

fn main() {
    println!("cargo::rerun-if-env-changed=MAGELLAN_BUILD");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // Empty counts as unset: make exports the variable even when the tree sits on its release tag.
    let version = match std::env::var("MAGELLAN_BUILD") {
        Ok(build) if !build.is_empty() => format!("{version}+{build}"),
        _ => version,
    };
    println!("cargo::rustc-env=MAGELLAN_VERSION={version}");
}
