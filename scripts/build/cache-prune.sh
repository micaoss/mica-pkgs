#!/usr/bin/env bash
# Reduce cargo target directories to third-party dependency artifacts before CI
# saves them as a cache. Everything this workspace's crates produced -- their
# libraries, binaries, tests, build scripts, fingerprints and incremental state
# -- is removed, so a build restored from the cache still compiles every
# workspace crate from its source.
#
#   bash scripts/build/cache-prune.sh <target-dir>...
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "cache-prune.sh: error: $*" >&2; exit 1; }
[ "$#" -gt 0 ] || die "usage: bash scripts/build/cache-prune.sh <target-dir>..."

mapfile -t PACKAGES < <(awk '/^\[/ { section = $0 } section == "[package]" && $1 == "name" { gsub(/"/, "", $3); print $3 }' "${REPO_ROOT}"/crates/*/Cargo.toml | LC_ALL=C sort -u)
[ "${#PACKAGES[@]}" -gt 0 ] || die "no workspace package found under crates/"

for target in "$@"; do
    [ -d "${target}" ] || continue
    mapfile -t profiles < <(find "${target}" -type d -name .fingerprint -printf '%h\n' | LC_ALL=C sort)
    for profile in "${profiles[@]}"; do
        rm -rf "${profile}/incremental" "${profile}/examples"
        find "${profile}" -maxdepth 1 -type f -delete
        # Executables in deps/ are binaries and tests; third-party crates leave
        # only lib*.rlib, lib*.rmeta, lib*.so and .d files there.
        find "${profile}/deps" -maxdepth 1 -type f ! -name 'lib*' ! -name '*.d' -delete 2>/dev/null || true
        for package in "${PACKAGES[@]}"; do
            crate="${package//-/_}"
            rm -rf "${profile}/.fingerprint/${package}-"* "${profile}/build/${package}-"*
            rm -f "${profile}/deps/lib${crate}-"* "${profile}/deps/${crate}-"*
        done
    done
done
