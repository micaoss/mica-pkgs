#!/usr/bin/env bash
# Publish _out/debs/{amd64,arm64,armhf}/pool of a clean HEAD for the published GitHub
# Release <tag> of this repository -- its Debian packages:
#   - every archive passes `mica-tools pool guard` against the previous
#     release: a package at a released version keeps its inputs hash and its
#     bytes;
#   - `mica-tools release pool` pushes the pools ghcr.io/micaoss/<repository>:pool.<arch>.<tag>,
#     one layer per archive, and reads them back anonymously;
#   - the lock (release, pool and package rows) passes `mica-tools lock check`, and
#     `mica-tools release attach` attaches it and SHA256SUMS listing only it, read back
#     anonymously.
# Nothing published is ever replaced.
#
#   GH_TOKEN=<token with contents:write and packages:write> bash scripts/build/release.sh <YYYYMMDD-HHMM>
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOLS="${REPO_ROOT}/bin/mica-tools"
ARCHES=(amd64 arm64 armhf)
die() { echo "release.sh: error: $*" >&2; exit 1; }

[ "$#" -eq 1 ] || die "usage: bash scripts/build/release.sh <YYYYMMDD-HHMM>"
TAG="$1"
cd "${REPO_ROOT}"
[ -n "${GH_TOKEN:-}" ] || die "GH_TOKEN must be set; releasing is CI's, with its own token"

# The tag is a UTC time naming this clean HEAD, on origin/main.
COMMIT="$("${TOOLS}" release check "${TAG}")" || die "${TAG} is not a release this checkout can publish; nothing was published"
"${TOOLS}" locks check --ci >/dev/null

# Every package of every producer is in each pool.
ROWS_TEXT="$(bash scripts/deb/producers.sh)" || die "scripts/deb/producers.sh failed"
for arch in "${ARCHES[@]}"; do
    while read -r producer _dir packages _enablement; do
        version="$(bash scripts/deb/producers.sh --version-of "${producer}")"
        for p in $(printf '%s' "${packages}" | tr ',' ' '); do
            f="${p}_${version}_${arch}.deb"
            [ -f "_out/debs/${arch}/pool/${f}" ] ||
                die "_out/debs/${arch}/pool/${f} does not exist; ${p} is released at its declared version"
        done
    done <<<"${ROWS_TEXT}"
    for file in "_out/debs/${arch}/pool/"*; do
        [ -f "${file}" ] || continue
        "${TOOLS}" pool guard --before "${TAG}" "${arch}" "${file}" | sed 's/^/release.sh: /'
    done
done

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
mkdir -p "${WORK}/assets"
LOCK="$(basename "$(git remote get-url origin)" .git).lock"

"${TOOLS}" release pool "${TAG}" $(printf -- '--arch %s ' "${ARCHES[@]}") >"${WORK}/rows"
{
    echo "# mica-lock v1"
    printf 'release\t%s\t%s\t%s\n' "${LOCK%.lock}" "${TAG}" "${COMMIT}"
    cat "${WORK}/rows"
} >"${WORK}/assets/${LOCK}"
result="$("${TOOLS}" lock check "${WORK}/assets/${LOCK}")" || die "the lock this release writes is ${result}"

"${TOOLS}" release attach "${TAG}" "${WORK}/assets/${LOCK}" \
    --notes "${LOCK%.lock} ${COMMIT}: ${LOCK} (mica-lock v1) names the package pools of this release. Verify with SHA256SUMS." ||
    die "the assets of ${TAG} were not attached"
sed 's/^/release.sh:   /' "${WORK}/assets/${LOCK}"
