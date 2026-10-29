#!/usr/bin/env bash
#
# Open a release: bump the version on a branch of its own and open its PR.
# main takes no push but a merge, so the bump is a PR like any other, gated by
# check. Once merged, `make tag` tags it and the release workflow builds it.
#   make release VERSION=0.1.2 [DRY_RUN=1]
#
# A dry run commits, pushes and opens nothing, and reports every refusal
# instead of the first.
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
re_version='^[0-9]+\.[0-9]+\.[0-9]+$'
if [[ ! $version =~ $re_version ]]; then
	printf 'usage: %s [--dry-run] <x.y.z>\n' "$0" >&2
	exit 2
fi
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

[[ $(git branch --show-current) == main ]] || refuse 'not on main'
[[ -z $(git status --porcelain) ]] || refuse 'tree not clean'
git fetch --quiet origin main || die 'cannot fetch origin'
# The bump sits on exactly what main holds: nothing missing, nothing extra.
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] ||
	refuse 'HEAD is not origin/main - pull, or push your work through a PR first'
current=$(scripts/version.sh) || die 'manifest unreadable'
[[ $current != "$version" ]] || refuse "the manifest is already $version"
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
	refuse "$tag exists here"
fi
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null
case $? in
	0) refuse "$tag exists on origin" ;;
	2) ;;
	*) die 'cannot list tags on origin' ;;
esac
git ls-remote --exit-code --heads origin "refs/heads/$branch" >/dev/null
case $? in
	0) refuse "$branch exists on origin" ;;
	2) ;;
	*) die 'cannot list branches on origin' ;;
esac
gh auth status >/dev/null 2>&1 || refuse 'gh is not logged in'

if $dry; then
	printf 'release: would bump %s to %s on %s and open its PR\n' \
		"$current" "$version" "$branch"
	((refusals == 0)) || die "dry run: $refusals refusal(s)"
	printf 'release: dry run clean\n'
	exit 0
fi

git switch --quiet -c "$branch" || die "cannot make $branch"
bump="s/^version = .*/version = \"$version\"/"
sed -i "/^\[workspace.package\]/,/^\[/ $bump" Cargo.toml ||
	die 'cannot bump Cargo.toml'
# The lock records every member's version.
cargo check --workspace --quiet || die 'check failed after the bump'
[[ $(scripts/version.sh) == "$version" ]] || die 'bump did not take'
git commit --quiet -am "$subject" || die 'commit failed'
git push --quiet -u origin "$branch" || die 'push failed'
gh pr create --base main --head "$branch" --title "$subject" \
	--body "Bumps to $version. Merged, \`make tag\` tags it." ||
	die "PR failed - rerun: gh pr create --head $branch"
git switch --quiet main || die 'cannot return to main'

printf '\n%s open. Once check is green and it is merged:\n' "$branch"
printf '  git pull && make tag\n'
