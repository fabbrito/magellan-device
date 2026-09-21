#!/usr/bin/env bash
#
# Cut a release on this machine: bump, commit, tag, gate. Nothing leaves it -
# `make publish` does that, once the board has run the binary.
#   make release VERSION=0.1.2 [DRY_RUN=1]
#
# Tag before gate: `make dist` names a build by `git describe`, and only a
# clean tree sitting on the tag builds a binary that reads plain `0.1.2`. A
# failed gate drops the tag and keeps the commit, so a rerun tags it again.
#
# A dry run commits and tags nothing, reports every refusal instead of the
# first, and gates the tree as it stands with `cross`, not `dist`: dist/ may
# hold a tagged build still awaiting publish.
#
# No errexit: each step is checked where it can fail.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

die() {
	printf 'release: %s\n' "$*" >&2
	exit 1
}

dry=false
if [[ ${1-} == --dry-run ]]; then
	dry=true
	shift
fi
version=${1-}
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	printf 'usage: %s [--dry-run] <x.y.z>\n' "$0" >&2
	exit 2
fi
tag=v$version
subject="chore(repo): release $tag"

gate() {
	make lint test dist || return 1
	local binaries=(dist/magellan-"$version"-*)
	[[ -f ${binaries[0]} ]] || return 1
	# A build id means the tree was not clean on the tag after all.
	! rg -q -a -F "$version+" "${binaries[@]}"
}

refusals=0
refuse() {
	$dry || die "$@"
	printf 'release: would refuse: %s\n' "$*" >&2
	((refusals += 1))
	return 0
}

[[ $(git branch --show-current) == master ]] || refuse 'not on master'
[[ -z $(git status --porcelain) ]] || refuse 'tree not clean'
git fetch --quiet origin master || die 'cannot fetch origin'
# Ahead is fine - a rerun after a failed gate is - but nothing of origin's may
# be missing from the release.
git merge-base --is-ancestor origin/master HEAD ||
	refuse 'origin/master has commits this branch lacks'
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
	refuse "$tag exists here"
fi
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null
case $? in
	0) refuse "$tag exists on origin" ;;
	2) ;;
	*) die 'cannot list tags on origin' ;;
esac

pkgid=$(cargo pkgid --locked -p magellan) || die 'manifest or lock unreadable'
if [[ ${pkgid##*[#@]} == "$version" ]]; then
	printf 'release: manifest already %s, no bump\n' "$version"
elif $dry; then
	printf 'release: would bump %s to %s and commit\n' \
		"${pkgid##*[#@]}" "$version"
else
	bump="s/^version = .*/version = \"$version\"/"
	sed -i "/^\[workspace.package\]/,/^\[/ $bump" Cargo.toml ||
		die 'cannot bump Cargo.toml'
	# The lock records every member's version.
	cargo check --workspace --quiet || die 'check failed after the bump'
	pkgid=$(cargo pkgid --locked -p magellan) || die 'lock unreadable'
	[[ ${pkgid##*[#@]} == "$version" ]] || die 'bump did not take'
	git commit --quiet -am "$subject" || die 'commit failed'
fi

if $dry; then
	printf 'release: would tag %s\n' "$tag"
	make lint test cross || die 'gate failed'
	((refusals == 0)) || die "dry run: $refusals refusal(s)"
	printf 'release: dry run clean\n'
	exit 0
fi

git tag -a "$tag" -m "magellan $tag" || die 'tag failed'
# An interrupted gate is a failed one: a tag left behind blocks the rerun.
trap 'git tag -d "$tag" >/dev/null; exit 130' INT TERM HUP
if ! gate; then
	git tag -d "$tag" >/dev/null
	die "gate failed: $tag dropped, fix and rerun"
fi
trap - INT TERM HUP

undo="git tag -d $tag"
# Decided from HEAD, not this run: a rerun finds the bump already made.
if [[ $(git log -1 --format=%s) == "$subject" ]] &&
	! git merge-base --is-ancestor HEAD origin/master; then
	undo+=' && git reset --hard HEAD~1'
fi
printf '\n%s tagged, nothing pushed. Next:\n' "$tag"
printf '  test %s on the board\n' dist/magellan-"$version"-*
printf '  make publish\n'
printf 'To undo: %s\n' "$undo"
