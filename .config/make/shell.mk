# templates/shell: copy to .config/make/shell.mk, `include` it after base.mk.
# lefthook.yml's lanes over every shell file. None, no run: shfmt handed no
# path reads stdin.
shell_files = $(call tree_files,'*.sh')

check::
	$(if $(shell_files),shfmt -i 0 -ci -d $(shell_files))
	$(if $(shell_files),shellcheck -x $(shell_files))

fmt::
	$(if $(shell_files),shfmt -i 0 -ci -w $(shell_files))
