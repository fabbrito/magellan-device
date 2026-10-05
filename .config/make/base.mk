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
