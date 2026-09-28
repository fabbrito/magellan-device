# shellcheck shell=sh
# Sourced by every git hook before lefthook runs: mise.toml's pinned tools
# first on PATH. Without it a hook falls back to whatever apt left installed and
# passes on the wrong version - so no mise is a failed hook, not a fallback.
if ! command -v mise >/dev/null 2>&1; then
	echo 'lefthook-rc: mise not on PATH - https://mise.jdx.dev, then make hooks' >&2
	exit 1
fi
eval "$(mise env -s bash)" || exit 1
