#!/usr/bin/env bash
#
# e2e: each release binary runs on its target and behaves as a process - the
# version it was built as, a config read, a config refused, a run that holds
# and stops on SIGTERM. No fake answers anything: what the program does is the
# unit and integration tests' to prove, this only that the build made one.
#   e2e/run.sh [--release] dist/magellan-<version>-<target>...
#
# --release: the version must be bare, as a build on its tag reads. A binary
# this host cannot run fails, never skips - armv7 needs qemu-user-binfmt.
#
# No errexit: each step is checked where it can fail.
set -uo pipefail
root=$(git rev-parse --show-toplevel) || exit
fixtures=$root/e2e

die() {
	printf 'e2e: %s\n' "$*" >&2
	exit 1
}

release=false
if [[ ${1-} == --release ]]; then
	release=true
	shift
fi
if (($# == 0)); then
	printf 'usage: %s [--release] <binary>...\n' "${0##*/}" >&2
	exit 2
fi

# A real token never comes near this: the run sends to nothing.
export MAGELLAN_TOKEN=e2e

# The release name carries the version: magellan-<x.y.z>-<target>.
re_name='^magellan-([0-9]+\.[0-9]+\.[0-9]+)-.+$'

# One scratch directory at a time, gone however the script ends.
scratch=
trap '[[ -n $scratch ]] && rm -rf "$scratch"' EXIT

prove() {
	local bin=$1
	local name=${bin##*/}
	local version out status pid tries
	[[ $name =~ $re_name ]] ||
		die "$bin: not named magellan-<version>-<target>"
	version=${BASH_REMATCH[1]}
	[[ -x $bin ]] || die "$bin: not an executable"
	# Absolute: the run starts from the scratch directory.
	[[ $bin == /* ]] || bin=$PWD/$bin

	out=$("$bin" --version) ||
		die "$name: cannot run here (exit $?) - armv7 needs qemu-user-binfmt"
	if $release; then
		[[ $out == "magellan $version" ]] ||
			die "$name: --version reads '$out', not bare $version"
	else
		[[ $out == "magellan $version" || $out == "magellan $version+"* ]] ||
			die "$name: --version reads '$out', not $version"
	fi

	"$bin" check --config "$fixtures/config.toml" >/dev/null ||
		die "$name: check refused the e2e config (exit $?)"

	"$bin" check --config "$fixtures/malformed.toml" 2>/dev/null
	status=$?
	((status == 78)) || die "$name: a malformed config exited $status, not 78"

	# Its own directory: the run writes its buffer under it, and reads
	# .env.local from it - the tree's own must never reach the binary.
	scratch=$(mktemp -d) || die 'mktemp failed'
	mkdir "$scratch/buffer" || die 'cannot make the buffer directory'
	(
		cd "$scratch" || exit
		exec "$bin" run --config "$fixtures/config.toml"
	) 2>/dev/null &
	pid=$!
	sleep 2
	if ! kill -0 "$pid" 2>/dev/null; then
		wait "$pid"
		die "$name: run ended on its own (exit $?) before SIGTERM"
	fi
	kill -TERM "$pid"
	# Five seconds to stop: a unit's stop that hangs is a SIGKILL later.
	tries=50
	while kill -0 "$pid" 2>/dev/null && ((tries-- > 0)); do
		sleep 0.1
	done
	if kill -0 "$pid" 2>/dev/null; then
		kill -KILL "$pid"
		die "$name: still running 5s after SIGTERM"
	fi
	wait "$pid"
	status=$?
	((status == 0)) || die "$name: SIGTERM exited $status, not 0"
	rm -rf "$scratch"
	scratch=

	printf 'e2e: %s ok\n' "$name"
}

for bin in "$@"; do
	prove "$bin"
done
