#!/usr/bin/env bash
# The package gate over _out/debs/<arch>/pool.
#
#   bash scripts/deb/package-gate.sh [--arch <amd64|arm64|armhf>] [--no-reproduce]
#
# `mica-tools pool gate` answers what the archives answer by themselves. Then,
# per architecture, what needs this repository's producers, read out of the
# archives with dpkg-deb in the mica-build-env base image: the pool holds
# exactly the packages the producers declare, each at its producer's declared
# version; a dependency on a package built here names its exact pool version;
# as many multi-user.target.wants links as ENABLEMENT declares; and, unless
# --no-reproduce, one producer rebuilt on an empty-cache builder gives
# byte-identical archives. --arch limits the gate to one pool.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST="${REPO_ROOT}/_out/debs"
die() { echo "package-gate.sh: error: $*" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || die "docker is required"

ARCHES=(amd64 arm64 armhf)
REPRODUCE=1
while [ "$#" -gt 0 ]; do
    case "$1" in
    --arch)
        case "${2-}" in amd64 | arm64 | armhf) ARCHES=("$2") ;; *) die "--arch takes amd64, arm64 or armhf" ;; esac
        shift 2
        ;;
    --no-reproduce) REPRODUCE=0; shift ;;
    *) die "usage: bash scripts/deb/package-gate.sh [--arch <amd64|arm64|armhf>] [--no-reproduce]" ;;
    esac
done
for arch in "${ARCHES[@]}"; do
    [ -d "${DIST}/${arch}/pool" ] || die "${DIST}/${arch}/pool does not exist; build it with make deb MICA_ARCH=${arch}"
done

mapfile -t ROWS < <(bash "${REPO_ROOT}/scripts/deb/producers.sh")
"${REPO_ROOT}/bin/mica-tools" pool gate $(printf -- '--arch %s ' "${ARCHES[@]}") || die "mica-tools pool gate refused the pool"
case "$(uname -m)" in
x86_64) HOST_ARCH=amd64 ;;
aarch64 | arm64) HOST_ARCH=arm64 ;;
*) die "$(uname -m) is not amd64 or arm64" ;;
esac
IMAGE="$("${REPO_ROOT}/bin/mica-tools" from --ref "mica-build-env:base@${HOST_ARCH}")"

mkdir -p "${REPO_ROOT}/tmp"
WORK="$(mktemp -d "${REPO_ROOT}/tmp/package-gate.XXXXXX")"
BUILDERS=()
cleanup() {
    for b in ${BUILDERS[@]+"${BUILDERS[@]}"}; do docker buildx rm "${b}" >/dev/null 2>&1 || true; done
    rm -rf "${WORK}"
}
trap cleanup EXIT
for row in "${ROWS[@]}"; do
    read -r producer _dir packages _enablement <<<"${row}"
    version="$(bash "${REPO_ROOT}/scripts/deb/producers.sh" --version-of "${producer}")"
    for p in ${packages//,/ }; do printf '%s %s\n' "${p}" "${version}"; done
done >"${WORK}/versions.tsv"
printf '%s\n' "${ROWS[@]}" >"${WORK}/producers.tsv"

LOG="${WORK}/static.log"
status=0
docker run --rm -i --label ai-agent=true \
    -v "${DIST}:/dist:ro" -v "${WORK}/producers.tsv:/producers.tsv:ro" -v "${WORK}/versions.tsv:/versions.tsv:ro" \
    --entrypoint /bin/bash "${IMAGE}" -s "${ARCHES[@]}" <<'INNER' 2>&1 | tee "${LOG}" || status=1
set -euo pipefail
PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

declare -A WANTS=() DECLARED=()
LOCAL=" "
while read -r _producer _dir packages enablement; do
    for p in ${packages//,/ }; do LOCAL="${LOCAL}${p} "; done
    for e in ${enablement//,/ }; do WANTS["${e%%=*}"]="${e#*=}"; done
done </producers.tsv
EXPECTED="$(printf '%s\n' ${LOCAL} | LC_ALL=C sort | tr '\n' ' ')"
while read -r p v; do DECLARED["${p}"]="${v}"; done </versions.tsv

ARCHIVES=0
for arch in "$@"; do
    pool="/dist/${arch}/pool"
    mapfile -t debs < <(find "${pool}" -maxdepth 1 -type f -name '*.deb' | LC_ALL=C sort)
    ARCHIVES=$((ARCHIVES + ${#debs[@]}))
    declare -A VER=()
    for deb in "${debs[@]}"; do VER["$(dpkg-deb --field "${deb}" Package)"]="$(dpkg-deb --field "${deb}" Version)"; done
    got="$(printf '%s\n' "${!VER[@]}" | LC_ALL=C sort | tr '\n' ' ')"
    if [ "${got}" = "${EXPECTED}" ]; then pass "${arch}: the pool holds exactly ${got% }"; else fail "${arch}: the pool holds [${got% }], the producers declare [${EXPECTED% }]"; fi

    for deb in "${debs[@]}"; do
        name="$(dpkg-deb --field "${deb}" Package)"
        version="${VER[${name}]}"

        if [ "${version}" = "${DECLARED[${name}]:-}" ]; then pass "${name} ${arch}: version ${version} is its producer's declared version"; else fail "${name} ${arch}: version ${version}, its producer declares ${DECLARED[${name}]:-none}"; fi

        IFS=',' read -ra entries <<<"$(dpkg-deb --field "${deb}" Depends)"
        for entry in ${entries[@]+"${entries[@]}"}; do
            IFS='|' read -ra alts <<<"${entry}"
            for alt in "${alts[@]}"; do
                read -r dep _rest <<<"${alt}"
                dep="${dep%%(*}"
                case "${LOCAL}" in *" ${dep} "*) ;; *) continue ;; esac
                case "${alt}" in
                *"(= ${VER[${dep}]:-<absent>})"*) pass "${name} ${arch}: depends on ${dep} (= ${VER[${dep}]})" ;;
                *) fail "${name} ${arch}: depends on ${dep} as '${alt# }', not its exact pool version ${VER[${dep}]:-<absent>}" ;;
                esac
            done
        done

        listing="$(dpkg-deb --contents "${deb}")"
        links="$(awk '$1 ~ /^l/ && $6 ~ /^\.\/etc\/systemd\/system\/multi-user\.target\.wants\// { n++ } END { print n + 0 }' <<<"${listing}")"
        if [ "${links}" = "${WANTS[${name}]:-}" ]; then
            pass "${name} ${arch}: ${links} multi-user.target.wants link(s)"
        else
            fail "${name} ${arch}: ${links} multi-user.target.wants link(s), ENABLEMENT declares ${WANTS[${name}]:-none}"
        fi
    done
    unset VER
done
echo "GATE ${PASS} ${FAIL} ${ARCHIVES}"
INNER

read -r PASS FAIL ARCHIVES < <(sed -n 's/^GATE //p' "${LOG}") || true
[ -n "${PASS:-}" ] || die "the gate container reported no result"

if [ "${status}" != 0 ] || [ "${FAIL}" != 0 ]; then
    echo "RESULT: FAIL (${PASS}/$((PASS + FAIL)) checks, ${ARCHIVES} archives)"
    exit 1
fi
if [ "${REPRODUCE}" = 0 ]; then
    echo "RESULT: PASS (${PASS}/${PASS} checks, ${ARCHIVES} archives, no rebuild)"
    exit 0
fi

# Rebuild one producer per architecture on an empty-cache builder and compare.
COMPARED=0
for i in "${!ARCHES[@]}"; do
    arch="${ARCHES[${i}]}"
    row="${ROWS[$((i % ${#ROWS[@]}))]}"
    read -r producer _dir packages _enablement <<<"${row}"
    pool="${DIST}/${arch}/pool"
    mkdir -p "${WORK}/before/${arch}"
    names=()
    for p in ${packages//,/ }; do
        files="$(find "${pool}" -maxdepth 1 -name "${p}_*_${arch}.deb" -printf '%f\n')"
        [ -n "${files}" ] || die "${pool} has no ${p} to compare"
        for f in ${files}; do
            cp "${pool}/${f}" "${WORK}/before/${arch}/"
            names+=("${f}")
        done
    done
    builder="mica-gate-${arch}-$$"
    docker buildx create --name "${builder}" --driver docker-container \
        --driver-opt "image=$("${REPO_ROOT}/bin/mica-tools" from --ref upstream:moby/buildkit:v0.33.0)" >/dev/null
    BUILDERS+=("${builder}")
    log="${WORK}/rebuild-${arch}.log"
    echo "package-gate.sh: rebuilding ${producer} for ${arch} on ${builder}"
    if ! BUILDX_BUILDER="${builder}" BUILDKIT_PROGRESS=plain bash "${REPO_ROOT}/scripts/deb/build.sh" --producer "${producer}" --arch "${arch}" >"${log}" 2>&1; then
        tail -n 30 "${log}"
        FAIL=$((FAIL + 1))
        echo "FAIL: ${arch}: the rebuild of ${producer} failed"
        continue
    fi
    for n in "${names[@]}"; do
        COMPARED=$((COMPARED + 1))
        if ! grep -c -- "/out/${n}\$" "${log}" >/dev/null; then
            FAIL=$((FAIL + 1))
            echo "FAIL: ${arch}: the rebuild did not pack ${n} (a cached layer was replayed)"
        elif cmp -s "${WORK}/before/${arch}/${n}" "${pool}/${n}"; then
            PASS=$((PASS + 1))
            echo "PASS: ${arch}: ${n} is byte-identical when rebuilt"
        else
            FAIL=$((FAIL + 1))
            echo "FAIL: ${arch}: ${n} differs when rebuilt"
        fi
    done
done
echo "RESULT: $([ "${FAIL}" = 0 ] && echo PASS || echo FAIL) (${PASS}/$((PASS + FAIL)) checks, ${ARCHIVES} archives, ${COMPARED} rebuilt archives compared)"
[ "${FAIL}" = 0 ]
