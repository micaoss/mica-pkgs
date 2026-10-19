#!/usr/bin/env bash
# A producer's prepare step: compile its binaries and stage them.
#
#   bash scripts/build/build-deb.sh --producer <name> --bins "<binary> ..." \
#        --arch <amd64|arm64|armhf> --stage <dir>
#
# Compiles only the named binaries, in the mica-build-env rust image of the
# host's architecture (native, or a cargo cross-compile; never emulation; armhf
# only from an amd64 host, whose image carries its std and linker), into
# the producer's own target directory _out/target-deb/<producer>. It then fails
# if that directory holds a binary the producer does not own, checks each
# binary's ELF architecture, and copies the binaries into --stage. The
# repository is mounted read-only at /src so rustc's recorded paths do not
# depend on where the checkout lives.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "build-deb.sh: error: $*" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || die "docker is required"

ALL_BINARIES=(mica-mqttd mica-mqtt-broker mica-sftp-server mica-bled)
PRODUCER="" BINS="" ARCH="" STAGE=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --producer) PRODUCER="${2:?}"; shift 2 ;;
    --bins) BINS="${2:?}"; shift 2 ;;
    --arch) ARCH="${2:?}"; shift 2 ;;
    --stage) STAGE="${2:?}"; shift 2 ;;
    *) die "usage: bash scripts/build/build-deb.sh --producer <name> --bins \"<binary> ...\" --arch <amd64|arm64|armhf> --stage <dir>" ;;
    esac
done
[ -n "${PRODUCER}" ] && [ -n "${BINS}" ] && [ -d "${STAGE}" ] || die "--producer, --bins and an existing --stage are required"
case "${ARCH}" in
amd64) TRIPLE=x86_64-unknown-linux-gnu ELF="ELF 64-bit*x86-64" ;;
arm64) TRIPLE=aarch64-unknown-linux-gnu ELF="ELF 64-bit*aarch64" ;;
armhf) TRIPLE=armv7-unknown-linux-gnueabihf ELF="ELF 32-bit*ARM, EABI5" ;;
*) die "--arch must be amd64, arm64 or armhf" ;;
esac
EXCLUDED=()
for b in ${BINS}; do
    case " ${ALL_BINARIES[*]} " in *" ${b} "*) ;; *) die "${b} is not one of ${ALL_BINARIES[*]}" ;; esac
done
for b in "${ALL_BINARIES[@]}"; do
    case " ${BINS} " in *" ${b} "*) ;; *) EXCLUDED+=("${b}") ;; esac
done
case "$(uname -m)" in x86_64) HOST_ARCH=amd64 ;; aarch64 | arm64) HOST_ARCH=arm64 ;; *) die "unsupported host $(uname -m)" ;; esac
[ "${ARCH}" != armhf ] || [ "${HOST_ARCH}" = amd64 ] || die "armhf is cross-compiled on an amd64 host"

TARGET_DIR="${REPO_ROOT}/_out/target-deb/${PRODUCER}"
CARGO_CACHE="${REPO_ROOT}/_out/cargo"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git" "${TARGET_DIR}"
RUST_IMAGE="$("${REPO_ROOT}/bin/mica-tools" from --ref "mica-build-env:rust@${HOST_ARCH}")"

docker run --rm --label ai-agent=true --platform "linux/${HOST_ARCH}" \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${TARGET_DIR}:/target" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src \
    -e "TARGET=${TRIPLE}" -e "ELF=${ELF}" -e "BINS=${BINS}" \
    -e "CARGO_TARGET_DIR=/target" \
    --entrypoint /bin/bash "${RUST_IMAGE}" -c '
        set -euo pipefail
        . /etc/mica-build/rust.env
        echo "build-deb.sh: compiling ${BINS} for ${TARGET} with rustc ${MICA_BUILD_RUSTC}"
        bins=""
        for b in ${BINS}; do bins="${bins} --bin ${b}"; done
        # shellcheck disable=SC2086
        cargo build --release --locked --target "${TARGET}" ${bins}
        for name in ${BINS}; do
            bin="${CARGO_TARGET_DIR}/${TARGET}/release/${name}"
            [ -f "${bin}" ] || { echo "build-deb.sh: error: ${name} was not produced" >&2; exit 1; }
            case "$(file -b "${bin}")" in
            ${ELF}*) ;;
            *) echo "build-deb.sh: error: ${name} is not an ELF for ${TARGET}" >&2; exit 1 ;;
            esac
        done
    '

stray=""
for name in ${EXCLUDED[@]+"${EXCLUDED[@]}"}; do
    stray="${stray}$(find "${TARGET_DIR}" -type f -name "${name}" -printf ' %p')"
done
[ -z "${stray}" ] || die "${TARGET_DIR} holds binaries the ${PRODUCER} producer does not own:${stray}"
for name in ${BINS}; do
    cp "${TARGET_DIR}/${TRIPLE}/release/${name}" "${STAGE}/${name}"
done
echo "build-deb.sh: staged ${BINS} into ${STAGE}"
