# templates/dprint: copy to .config/make/dprint.mk, `include` it after
# base.mk. lefthook.yml's lane over the whole tree: dprint.json says which
# files.
check::
	dprint check --allow-no-files

fmt::
	dprint fmt --allow-no-files

# The plugins move together, so they are one bump. --reload: the registry, not
# the cache. --minimum-dependency-age: the day mise.toml's minimum_release_age
# holds to. The first run may move a URL plugin to npm:@dprint/<name>, the same
# plugin at a newer version.
.PHONY: bump-dprint

bump: bump-dprint

bump-dprint:
	dprint config update --yes --reload=plugins --minimum-dependency-age P1D
