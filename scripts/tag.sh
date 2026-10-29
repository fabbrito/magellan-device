#!/usr/bin/env bash
#
# Tag what main holds as the release its manifest names, and push the tag
# alone: the release workflow builds it, attests it and drafts the release.
# Public and never moved after - a bad release gets the next patch, not a
# retag.
#   make tag [DRY_RUN=1]
#
# origin/main, not HEAD: the tag names what was merged, whatever is checked
# out here. A dry run tags and pushes nothing, and reports every refusal.
#
# No errexit: each step is checked where it can fail.
set -uo pipefail
root=$(git rev-parse --show-toplevel) || exit
cd "$root" || exit

die() {
	printf 'tag: %s\n' "$*" >&2
	exit 1
}

dry=false
if [[ ${1-} == --dry-run ]]; then
	dry=true
	shift
fi
if (($# > 0)); then
	printf 'usage: %s [--dry-run]\n' "${0##*/}" >&2
	exit 2
fi

refusals=0
refuse() {
	$dry || die "$@"
	printf 'tag: would refuse: %s\n' "$*" >&2
	((refusals += 1))
	return 0
}

# Refuses when origin holds the ref. ls-remote exits 2 for absent, anything
# else past 0 is a failure to ask - never taken for absent.
remote_absent() {
	local kind=$1 ref=$2
	git ls-remote --exit-code "--$kind" origin "$ref" >/dev/null
	case $? in
		0) refuse "${ref#refs/*/} exists on origin" ;;
		2) ;;
		*) die "cannot list $kind on origin" ;;
	esac
}

git fetch --quiet --tags origin main || die 'cannot fetch origin'
commit=$(git rev-parse origin/main) || die 'no origin/main'
version=$(scripts/version.sh origin/main) ||
	die 'manifest unreadable on origin/main'
tag=v$version

if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
	refuse "$tag exists - bump through make release first"
fi
remote_absent tags "refs/tags/$tag"

if $dry; then
	printf 'tag: would tag %s at %s and push it\n' "$tag" "${commit:0:12}"
	((refusals == 0)) || die "dry run: $refusals refusal(s)"
	printf 'tag: dry run clean\n'
	exit 0
fi

git tag -a "$tag" "$commit" -m "magellan $tag" || die 'tag failed'
if ! git push --quiet origin "refs/tags/$tag"; then
	git tag -d "$tag" >/dev/null
	die "push failed: $tag dropped here, rerun"
fi

printf '\n%s pushed. The release workflow drafts it. Next:\n' "$tag"
printf '  gh run watch\n'
printf '  gh release download %s, run it on the board\n' "$tag"
printf '  gh release edit %s --draft=false\n' "$tag"
