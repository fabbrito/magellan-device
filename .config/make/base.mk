# templates/base: copy to .config/make/base.mk and `include` it first in the
# Makefile. mise's pinned tools first on PATH, so a target runs what the hooks
# run. No mise is an error, not a quiet fallback.
ifeq ($(shell command -v mise),)
$(error mise not on PATH - https://mise.jdx.dev, then make deps)
endif
export PATH := $(shell mise bin-paths | paste -sd: -):$(PATH)

.PHONY: deps hooks

##@ Tools and hooks

# Once per clone, and after a bump in mise. Double-colon: a repo appends its
# own deps (collections, packages) as another `deps::`, run after this one,
# with no `##` - that would list deps twice. Plain `deps:` is a make error.
deps:: ## install the pinned tools
	mise install

# Once per clone, after make deps: hooks do not travel with the tree.
# core.hooksPath is unset first - a leftover would hide lefthook's hooks.
# mise exec, not PATH: PATH was read before make deps installed lefthook.
hooks: ## install lefthook's hooks for this clone
	@git config --unset core.hooksPath || true
	mise exec -- lefthook install
	@echo 'hooks enabled - skip one commit with LEFTHOOK=0'

##@ Gate

# The hooks grade the staged set; these, the whole tree. Plain commands, not
# `lefthook run`: a hook is for git. Double-colon like deps: each template's
# make.mk appends its lanes as another `check::` / `fmt::`, with no `##`.
.PHONY: check fmt

check:: ## every lane over the whole tree, read only
	lefthook validate

fmt:: ## the same lanes, writing - never stages
	@:

# tree_files,<pathspec>: the hooks' file set over the whole tree - tracked and
# untracked, ignored out, symlinks and deleted paths skipped. Words, not
# lines: a path with a space splits.
tree_files = $(shell git ls-files -co --exclude-standard -- $(1) | \
	while read -r f; do [ -L "$$f" ] || [ ! -e "$$f" ] || echo "$$f"; done)
