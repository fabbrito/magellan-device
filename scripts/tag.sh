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
cd "$(dirname "$0")/.." || exit 1

die() {
	printf 'tag: %s\n' "$*" >&2
	exit 1
}

dry=false
[[ ${1-} == --dry-run ]] && dry=true

refusals=0
refuse() {
	$dry || die "$@"
	printf 'tag: would refuse: %s\n' "$*" >&2
	((refusals += 1))
	return 0
}

git fetch --quiet --tags origin main || die 'cannot fetch origin'
commit=$(git rev-parse origin/main) || die 'no origin/main'
version=$(scripts/version.sh origin/main) || die 'manifest unreadable on origin/main'
tag=v$version

if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
	refuse "$tag exists - bump through make release first"
fi
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null
case $? in
	0) refuse "$tag exists on origin" ;;
	2) ;;
	*) die 'cannot list tags on origin' ;;
esac

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
