# templates/rust: copy to .config/make/rust.mk, `include` it after base.mk.
# lefthook.yml's lanes, already whole tree; fmt is the one place cargo fmt
# writes.
check::
	cargo fmt --all --check
	cargo check --workspace --all-targets --all-features

fmt::
	cargo fmt --all
