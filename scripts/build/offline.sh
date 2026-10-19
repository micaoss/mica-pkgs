#!/usr/bin/env bash
# Build this repository's release outputs locally from a clean checkout, with
# only the inputs it already pins (locks/, Cargo.lock):
# every pool, indexed, exactly as `make pool` writes them. No GitHub release is
# read; the locks are checked offline.
#
#   bash scripts/build/offline.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "offline.sh: error: $*" >&2; exit 1; }
cd "${REPO_ROOT}"

[ -z "$(git status --porcelain)" ] || die "the checkout has uncommitted changes; an offline build is made from a clean commit only"
bin/mica-tools locks check

rm -rf _out/debs/amd64 _out/debs/arm64 _out/debs/armhf
make --no-print-directory pool

echo "offline.sh: built $(git rev-parse HEAD)"
echo "offline.sh: warning: the package versions were not checked against a release (mica-tools pool guard needs the published release); CI and the release check them" >&2
for arch in amd64 arm64 armhf; do
    echo "offline.sh: ${REPO_ROOT}/_out/debs/${arch}/pool ($(find "_out/debs/${arch}/pool" -maxdepth 1 -name '*.deb' | wc -l) archives)"
    echo "offline.sh: ${REPO_ROOT}/_out/debs/${arch}/Packages ${REPO_ROOT}/_out/debs/${arch}/SHA256SUMS ${REPO_ROOT}/_out/debs/${arch}/manifest.txt"
done
