#!/usr/bin/env bash
#
# Open a release: bump the version on a branch of its own and open its PR.
# main takes no push but a merge, so the bump is a PR like any other, gated by
# check. Once merged, `make tag` tags it and the release workflow builds it.
#   make release VERSION=0.1.2 [DRY_RUN=1]
#
# A dry run commits, pushes and opens nothing, and reports every refusal
# instead of the first. A failure before the push undoes the branch, so a
# rerun starts clean.
#
# No errexit: each step is checked where it can fail.
set -uo pipefail
root=$(git rev-parse --show-toplevel) || exit
cd "$root" || exit

die() {
	printf 'release: %s\n' "$*" >&2
	exit 1
}

usage() {
	printf 'usage: %s [--dry-run] <x.y.z>\n' "${0##*/}" >&2
	exit 2
}

dry=false
if [[ ${1-} == --dry-run ]]; then
	dry=true
	shift
fi
(($# == 1)) || usage
version=$1
re_version='^[0-9]+\.[0-9]+\.[0-9]+$'
[[ $version =~ $re_version ]] || usage
tag=v$version
branch=release/$tag
subject="chore(repo): release $tag"

refusals=0
refuse() {
	$dry || die "$@"
	printf 'release: would refuse: %s\n' "$*" >&2
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

[[ $(git branch --show-current) == main ]] || refuse 'not on main'
[[ -z $(git status --porcelain) ]] || refuse 'tree not clean'
git fetch --quiet origin main || die 'cannot fetch origin'
# The bump sits on exactly what main holds: nothing missing, nothing extra.
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] ||
	refuse 'HEAD is not origin/main - pull, or land your work by PR'
current=$(scripts/version.sh) || die 'manifest unreadable'
[[ $current != "$version" ]] || refuse "the manifest is already $version"
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
	refuse "$tag exists here"
fi
remote_absent tags "refs/tags/$tag"
remote_absent heads "refs/heads/$branch"
gh auth status >/dev/null 2>&1 || refuse 'gh is not logged in'

if $dry; then
	printf 'release: would bump %s to %s on %s and open its PR\n' \
		"$current" "$version" "$branch"
	((refusals == 0)) || die "dry run: $refusals refusal(s)"
	printf 'release: dry run clean\n'
	exit 0
fi

git switch --quiet -c "$branch" || die "cannot make $branch"
# Nothing pushed yet: back to main as it was, the branch gone.
undo() {
	git checkout --quiet -- Cargo.toml Cargo.lock
	git switch --quiet main
	git branch --quiet -D "$branch"
	die "$1 - undone, back on main"
}
bump="s/^version = .*/version = \"$version\"/"
sed -i "/^\[workspace.package\]/,/^\[/ $bump" Cargo.toml ||
	undo 'cannot bump Cargo.toml'
# The lock records every member's version.
cargo check --workspace --quiet || undo 'check failed after the bump'
[[ $(scripts/version.sh) == "$version" ]] || undo 'bump did not take'
git commit --quiet -am "$subject" || undo 'commit failed'
git push --quiet -u origin "$branch" || {
	git reset --quiet --hard HEAD~1
	undo 'push failed'
}
git switch --quiet main ||
	die "$branch pushed, but cannot return to main"
gh pr create --base main --head "$branch" --title "$subject" \
	--body "Bumps to $version. Merged, \`make tag\` tags it." ||
	die "$branch pushed, PR failed - rerun: gh pr create --head $branch"

printf '\n%s open. Once check is green and it is merged:\n' "$branch"
printf '  git pull && make tag\n'
