#!/usr/bin/env bash
#
# The workspace version, to stdout: Cargo.toml's [workspace.package] at a
# revision, or in the tree. git's copy, not cargo's - the tag check runs where
# no toolchain is installed, and on a revision not checked out.
#   scripts/version.sh [<rev>]
set -uo pipefail
root=$(git rev-parse --show-toplevel) || exit
cd "$root" || exit

die() {
	printf 'version: %s\n' "$*" >&2
	exit 1
}

if [[ -n ${1-} ]]; then
	manifest=$(git show "$1:Cargo.toml") || die "no Cargo.toml at $1"
else
	manifest=$(<Cargo.toml) || die 'no Cargo.toml'
fi

re_version='^version = "([0-9]+\.[0-9]+\.[0-9]+)"$'
section=
while IFS= read -r line; do
	if [[ $line == '['*']' ]]; then
		section=$line
	elif [[ $section == '[workspace.package]' && $line =~ $re_version ]]; then
		printf '%s\n' "${BASH_REMATCH[1]}"
		exit 0
	fi
done <<<"$manifest"
die 'no version under [workspace.package]'
