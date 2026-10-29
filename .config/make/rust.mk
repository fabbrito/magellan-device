# templates/rust: copy to .config/make/rust.mk, `include` it after base.mk.
# lefthook.yml's lanes, already whole tree; fmt is the one place cargo fmt
# writes.
check::
	cargo fmt --all --check
	cargo check --workspace --all-targets --all-features

fmt::
	cargo fmt --all

# Cargo.toml keeps its floors and the lock is what moves: cargo update never
# rewrites a manifest. cargo has no cooldown of its own on stable
# (-Z min-publish-age is nightly's), so cargo-cooldown applies cooldown.toml's
# day before the lock keeps the result.
.PHONY: bump-rust

bump: bump-rust

bump-rust:
	cargo cooldown update
