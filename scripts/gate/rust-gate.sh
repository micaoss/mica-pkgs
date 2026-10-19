#!/usr/bin/env bash
# The Rust gate: scripts/build/check.sh, UNMODIFIED, inside the mica-build-env rust image
# -- the version and inputs agreement, the MSRV, `cargo fmt --all --check`, clippy
# at `-D warnings`, nextest, doctests, `cargo deny check licenses bans advisories`,
# cargo-shear and typos.
# The repository is mounted read-only at the fixed path /src, so rustc's
# recorded paths do not depend on where the checkout lives and the gate
# cannot fix what it found; the image records what it is and the log reads
# that back, so the result names the toolchain that produced it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
for p in "${REPO_ROOT}/Cargo.toml" "${REPO_ROOT}/locks/mica-build-env.lock"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. scripts/gate/rust-gate.sh derives the repository as two levels above itself; locks/mica-build-env.lock is the committed mica-build-env release lock" >&2
        exit 1
    }
done
[ -x "${REPO_ROOT}/scripts/build/check.sh" ] || { echo "error: ${REPO_ROOT}/scripts/build/check.sh is missing or not executable; this gate runs that script and defines no gate of its own" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "error: docker is required and not on PATH; the gate runs in the pinned Rust image" >&2; exit 1; }

IMAGE_ARCH=amd64
IMAGE="$("${REPO_ROOT}/bin/mica-tools" from --ref "mica-build-env:rust@${IMAGE_ARCH}")"

CARGO_CACHE="${REPO_ROOT}/_out/cargo"
TARGET_DIR="${REPO_ROOT}/_out/rust-gate"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git" "${TARGET_DIR}"

echo "=== rust gate: mica-pkgs in ${IMAGE} ==="
docker run --rm \
    --label ai-agent=true \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${TARGET_DIR}:/target" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src \
    -e "CARGO_TARGET_DIR=/target" \
    --entrypoint /bin/bash \
    "${IMAGE}" -c '
        set -euo pipefail
        [ -f /etc/mica-build/rust.env ] || {
            echo "error: this image carries no /etc/mica-build/rust.env, so what ran this gate cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mica-build/rust.env
        echo "gate: mica-pkgs with rustc ${MICA_BUILD_RUSTC}, clippy ${MICA_BUILD_CLIPPY}, rustfmt ${MICA_BUILD_RUSTFMT}, nextest ${MICA_BUILD_NEXTEST}, deny ${MICA_BUILD_DENY}, shear ${MICA_BUILD_SHEAR}, typos ${MICA_BUILD_TYPOS} from ${MICA_BUILD_IMAGE}"
        bash scripts/build/check.sh
    '
echo "RUST GATE PASSED (mica-pkgs)"
