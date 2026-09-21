#!/usr/bin/env bash
#
# Publish what `make release` tagged: master and the tag to origin, then a
# GitHub release carrying the binary and its checksum. Public, and never moved
# after - a bad release gets the next patch, not a retag.
#   make publish [DRY_RUN=1]
#
# A dry run writes to neither origin nor gh: it reports every refusal instead
# of the first, and prints what it would run and the notes.
#
# No errexit: each step is checked where it can fail.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

die() {
	printf 'publish: %s\n' "$*" >&2
	exit 1
}

dry=false
if [[ ${1-} == --dry-run ]]; then
	dry=true
	shift
fi
target=${1-}
if [[ -z $target ]]; then
	printf 'usage: %s [--dry-run] <rust target>\n' "$0" >&2
	exit 2
fi

refusals=0
refuse() {
	$dry || die "$@"
	printf 'publish: would refuse: %s\n' "$*" >&2
	((refusals += 1))
	return 0
}

tag=$(git describe --tags --exact-match HEAD 2>/dev/null) ||
	die 'HEAD carries no tag - make release first'
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$tag is not a release tag"
version=${tag#v}
[[ $(git branch --show-current) == master ]] || refuse 'not on master'
[[ -z $(git status --porcelain) ]] || refuse 'tree not clean'
pkgid=$(cargo pkgid --locked -p magellan) || die 'manifest or lock unreadable'
[[ ${pkgid##*[#@]} == "$version" ]] || refuse "manifest is not $version"

# Exactly these two go up: a stray file in dist/ never does.
name=magellan-$version-$target
binary=dist/$name
sums=dist/SHA256SUMS
[[ -f $binary && -f $sums ]] || die "no $binary - make release first"
rg -q -F "  $name" "$sums" || refuse "$sums does not list $name"
(cd dist && sha256sum --quiet -c SHA256SUMS) || refuse 'checksum mismatch'
if rg -q -a -F "$version+" "$binary"; then
	refuse "$binary carries a build id - rebuild on the tag"
fi
gh auth status >/dev/null 2>&1 || refuse 'gh is not logged in'
# Only "not found" is absent: on any other error the push would go ahead blind.
if err=$(gh release view "$tag" 2>&1 >/dev/null); then
	refuse "release $tag exists. If nobody fetched it: gh release delete $tag"
elif [[ $err != 'release not found' ]]; then
	die "cannot tell whether release $tag exists: $err"
fi

notes=$(mktemp) || die 'mktemp failed'
trap 'rm -f "$notes"' EXIT
scripts/notes.sh "$tag" >"$notes" || die 'notes failed'

push=(git push --atomic origin master "refs/tags/$tag")
create=(gh release create "$tag" "$binary" "$sums" --verify-tag
	--title "magellan $tag" --notes-file "$notes")

if $dry; then
	printf 'publish: would run:\n '
	printf ' %q' "${push[@]}"
	printf '\n '
	printf ' %q' "${create[@]}"
	printf '\npublish: notes:\n'
	cat "$notes"
	((refusals == 0)) || die "dry run: $refusals refusal(s)"
	printf 'publish: dry run clean\n'
	exit 0
fi

# Atomic: a tag on origin without its master would point at nothing there.
"${push[@]}" || die 'push failed'
"${create[@]}" || die 'release failed after the push - rerun make publish'
