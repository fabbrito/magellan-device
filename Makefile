# Magellan device — targets. The commit gate lives in `lefthook.yml`.

# mise.toml's pinned tools first on PATH, so a target runs what the hooks run -
# not whatever apt or cargo install left behind. No mise is an error, not a
# quiet fallback to those.
ifeq ($(shell command -v mise),)
$(error mise not on PATH - https://mise.jdx.dev, then make hooks)
endif
export PATH := $(shell mise bin-paths | paste -sd: -):$(PATH)

# Where the tree stood, for `magellan`'s build.rs. Empty on a clean tree at
# `v<version>`, so a release binary reads the bare version.
PKG_VERSION := $(shell cargo pkgid -p magellan | sed 's/.*[#@]//')
export MAGELLAN_BUILD := $(filter-out v$(PKG_VERSION),$(shell git describe --tags --match 'v[0-9]*' --always --dirty))

# Not adopted (yet): nursery is unstable, and the other three each cost more
# ceremony than they buy here. Advisory - see the `advisory` target.
ADVISORY = -W clippy::nursery \
           -W clippy::arithmetic_side_effects \
           -W clippy::as_conversions \
           -W clippy::cast_precision_loss \
           -W clippy::cast_possible_truncation \
           -W clippy::cast_sign_loss

.PHONY: help hooks build run test check lint advisory fmt clean cross \
        dist release publish

define HELP_AWK
BEGIN {
	FS = ":.*##"
	printf "\nUsage: make \033[1m<target>\033[0m\n"
	printf "       make -n \033[1m<target>\033[0m prints its recipe, runs nothing\n"
}
/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }
/^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2 }
endef
export HELP_AWK

##@ Setup
help: ## show this help
	@awk "$$HELP_AWK" $(firstword $(MAKEFILE_LIST))

# Once per clone, and after a bump in mise.toml: hooks do not travel with the
# tree. core.hooksPath is unset first - a leftover from the vendored engine
# would hide lefthook's. mise exec, not PATH: PATH was read before the install.
hooks: ## install the pinned tools and lefthook's hooks for this clone
	mise install
	@git config --unset core.hooksPath || true
	mise exec -- lefthook install
	@echo 'hooks enabled - skip one commit with LEFTHOOK=0'

##@ Build
build: ## cargo build - debug
	cargo build --workspace

# ARGS carries the subcommand and flags, e.g.
#   make run ARGS="schema"
run: ## run the cli - args in ARGS="..."
	cargo run --bin magellan -- $(ARGS)

# Board -> Rust target. musl links static, so no libc match on the board.
# ESP32 does not go through cross: it needs the esp-idf toolchain, added with
# the platform crate.
PI             ?= 2b
PI_TARGET_2b   = armv7-unknown-linux-musleabihf
PI_TARGET_4    = aarch64-unknown-linux-musl
CROSS_TARGET   = $(PI_TARGET_$(PI))
CROSS_DIR      = target/cross

cross: ## release binary for a Pi, in Docker - PI=2b|4, default 2b
	$(if $(CROSS_TARGET),,$(error unknown PI=$(PI)))
	cross build --release --bin magellan --target $(CROSS_TARGET) --target-dir $(CROSS_DIR)
	@echo $(CROSS_DIR)/$(CROSS_TARGET)/release/magellan

# Emptied first: publish uploads what is here, and a stale binary must not
# ride along with a fresh one.
dist: cross ## release binary + SHA256SUMS in dist/ - PI= as for cross
	rm -rf dist
	mkdir dist
	cp $(CROSS_DIR)/$(CROSS_TARGET)/release/magellan \
		dist/magellan-$(PKG_VERSION)-$(CROSS_TARGET)
	cd dist && sha256sum magellan-$(PKG_VERSION)-$(CROSS_TARGET) >SHA256SUMS

##@ Quality
test: ## cargo nextest - one process per test, slow ones flagged
	cargo nextest run --workspace --all-features

check: ## cargo check - types only, no lints
	cargo check --workspace --all-targets --all-features

# The lanes live in lefthook.yml; `check` runs them over the working changes -
# the same ones pre-commit grades when staged. So the formatter flags and the
# globs have one home, and this target only calls.
# Clippy levels live in [workspace.lints.clippy]; -D warnings is what catches
# the rest - rustc's own dead_code, unused_variables and friends. Clippy is not
# a lane: it is not fast enough to sit between you and a commit.
lint: ## the commit gate lanes + clippy - read only
	lefthook run check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

# Advisory only - mine it for candidates; promote a rule by moving it into
# Cargo.toml.
advisory: ## the lints make lint does not deny - advisory
	cargo clippy --workspace --all-targets --all-features -- $(ADVISORY)

fmt: ## the lanes' fixers: cargo fmt, shfmt -w, dprint fmt - writes, never stages
	lefthook run fix

clean: ## cargo clean
	cargo clean

##@ Release
# Both reach outside this machine, so both are the maintainer's to run
# (AGENTS.md). DRY_RUN=1 writes nothing and reports every refusal.
release: ## tag a release here, gated - VERSION=x.y.z [DRY_RUN=1]
	scripts/release.sh $(if $(DRY_RUN),--dry-run) $(VERSION)

publish: ## push the tag, publish dist/ to GitHub - [DRY_RUN=1]
	scripts/publish.sh $(if $(DRY_RUN),--dry-run) $(CROSS_TARGET)
