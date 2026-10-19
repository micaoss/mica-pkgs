#!/usr/bin/env bash
# Build one producer's packages for one architecture into
# _out/debs/<arch>/pool, each <name>_<version>_<arch>.deb.
#
#   bash scripts/deb/build.sh --producer <name> --arch <amd64|arm64|armhf>
#
# prepare.sh compiles and stages the binaries on the host; the producer's
# Dockerfile then stages the payload and packs it with `mica-tools deb pack` in
# the mica-build-env base image of the target architecture, because
# dpkg-shlibdeps resolves libraries of the architecture it runs on. armhf has
# no build-env image: it is packed in the amd64 C image, whose cross toolchain
# holds the armhf libraries (scripts/deb/shlibs.sh). BUILDX_BUILDER selects a builder; otherwise the
# default one is used when it offers the platform, else a docker-container
# builder mica-<arch>.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEB="${REPO_ROOT}/scripts/deb"
die() { echo "build.sh: error: $*" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || die "docker is required"

PRODUCER=""
ARCH=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --producer) PRODUCER="${2:?--producer takes a name}"; shift 2 ;;
    --arch) ARCH="${2:?--arch takes amd64, arm64 or armhf}"; shift 2 ;;
    *) die "usage: bash scripts/deb/build.sh --producer <name> --arch <amd64|arm64|armhf>" ;;
    esac
done
[ -n "${PRODUCER}" ] || die "--producer is required"
case "${ARCH}" in
amd64 | arm64) PLATFORM="${ARCH}" IMAGE_REF="mica-build-env:base@${ARCH}" ;;
armhf) PLATFORM=amd64 IMAGE_REF="mica-build-env:c@amd64" ;;
*) die "--arch must be amd64, arm64 or armhf" ;;
esac

DIR="${REPO_ROOT}/$(bash "${DEB}/producers.sh" --dir-for "${PRODUCER}")"
CONTEXTS=""
# shellcheck disable=SC1091
. "${DIR}/producer.env"
VERSION="$(bash "${DEB}/producers.sh" --version-of "${PRODUCER}")"
PACKAGES="$(sed -n 's/^package //p' "${DIR}/mica-inputs" | tr '\n' ' ')"
SOURCE_DATE_EPOCH="$(sed -n 's/^Source-Date-Epoch: *//p' "${DIR}/control/${PACKAGES%% *}.control")"
SOURCE_REPO="${MICA_SOURCE_REPO:-$(basename "$(git -C "${REPO_ROOT}" remote get-url origin 2>/dev/null)" .git)}"
[[ "${SOURCE_REPO}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "cannot tell the repository name from origin; set MICA_SOURCE_REPO"

STAGE="${REPO_ROOT}/tmp/deb-${PRODUCER}-${ARCH}"
rm -rf "${STAGE}"
mkdir -p "${STAGE}"
echo "build.sh: ${PRODUCER} ${ARCH}: prepare.sh"
MICA_DEB_REPO_ROOT="${REPO_ROOT}" MICA_DEB_PRODUCER="${PRODUCER}" MICA_DEB_ARCH="${ARCH}" MICA_DEB_STAGE="${STAGE}" \
    bash "${DIR}/prepare.sh"
[ -n "$(ls -A "${STAGE}")" ] || die "${PRODUCER}/prepare.sh staged nothing"

if [ -n "${BUILDX_BUILDER:-}" ]; then
    BUILDER="${BUILDX_BUILDER}"
elif docker buildx inspect default 2>/dev/null | grep -c "linux/${PLATFORM}" >/dev/null; then
    BUILDER=default
else
    # A container builder runs the buildkit image of mica-build-env's upstream rows; its digest names it.
    BUILDKIT="$("${REPO_ROOT}/bin/mica-tools" from --ref upstream:moby/buildkit:v0.33.0)"
    BUILDER="mica-${PLATFORM}-${BUILDKIT##*@sha256:}"
    BUILDER="${BUILDER:0:30}"
    docker buildx inspect "${BUILDER}" >/dev/null 2>&1 ||
        docker buildx create --name "${BUILDER}" --driver docker-container --driver-opt "image=${BUILDKIT}" >/dev/null
fi

# The packer is the pinned mica-build-tools checkout, a build context of its own.
"${REPO_ROOT}/bin/mica-tools" sync
IMAGE="$("${REPO_ROOT}/bin/mica-tools" from --ref "${IMAGE_REF}")"
SYNTAX="$("${REPO_ROOT}/bin/mica-tools" from --ref upstream:docker/dockerfile:1)"
ARGS=(
    --build-context "tools=${REPO_ROOT}/repos/mica-build-tools"
    --build-context "deb=${DEB}"
    --build-context "pkgs=${REPO_ROOT}/pkgs"
    --build-context "bin=${STAGE}"
)
for c in ${CONTEXTS}; do ARGS+=(--build-context "${c%%=*}=${REPO_ROOT}/${c#*=}"); done

POOL="${MICA_POOL_DIR:-${REPO_ROOT}/_out/debs}/${ARCH}/pool"
mkdir -p "${POOL}"
EXPECTED=()
for p in ${PACKAGES}; do
    rm -f "${POOL}/${p}"_*.deb
    EXPECTED+=("${POOL}/${p}_${VERSION}_${ARCH}.deb")
done

echo "build.sh: ${PRODUCER} ${ARCH}: packing ${PACKAGES} ${VERSION} on builder ${BUILDER}"
docker buildx build --builder "${BUILDER}" --platform "linux/${PLATFORM}" \
    --build-arg "BUILDKIT_SYNTAX=${SYNTAX}" \
    --build-arg "MICA_BUILD_BASE=${IMAGE}" \
    --build-arg "MICA_DEB_ARCH=${ARCH}" \
    --build-arg "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}" \
    --build-arg "MICA_DEB_SOURCE_REPO=${SOURCE_REPO}" \
    "${ARGS[@]}" \
    -f "${DIR}/Dockerfile" -o "type=local,dest=${POOL}" "${DIR}"

for deb in "${EXPECTED[@]}"; do
    [ -f "${deb}" ] || { rm -f "${EXPECTED[@]}"; die "the build exported no ${deb#"${REPO_ROOT}"/}"; }
done
rm -rf "${STAGE}"
printf '%s\n' "${EXPECTED[@]}"
