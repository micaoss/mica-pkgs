#!/usr/bin/env bash
# List and validate the package producers under pkgs/.
#
#   bash scripts/deb/producers.sh                       one line per producer:
#                                                       <name> <dir> <packages,> <enablement,>
#   bash scripts/deb/producers.sh --dir-for <name>      the producer's directory
#   bash scripts/deb/producers.sh --version-of <name>   the version its packages carry
#
# A producer is pkgs/<name>/ with mica-inputs, producer.env, Dockerfile,
# prepare.sh and one control/<package>.control per package.
#
#   mica-inputs    the packages it emits (`package` lines) and what decides
#                  their bytes (mica-build-tools design 3.3.1)
#   producer.env   plain assignments:
#                    ENABLEMENT="<package>=<n> ..."  multi-user.target.wants links each ships
#                    CONTEXTS="<name>=<path> ..."    extra build contexts (repository-relative)
#   control/       one template per package, each declaring the producer's one
#                  Version (<major>.<minor>.<patch>-<revision>) and Source-Date-Epoch
#                  literally (design 3.3.2)
#
# A dependency on a package built here names that package's exact version.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "producers.sh: error: $*" >&2; exit 1; }

DIR_FOR="" VERSION_OF=""
case "${1:-}" in
"") ;;
--dir-for) DIR_FOR="${2:?--dir-for takes a producer name}" ;;
--version-of) VERSION_OF="${2:?--version-of takes a producer name}" ;;
*) die "usage: bash scripts/deb/producers.sh [--dir-for <name> | --version-of <name>]" ;;
esac

field() { # <template> <field>: the field's value, or empty
    sed -n "s/^$2: *//p" "$1" | awk "NR == 1"
}

ROWS=()
declare -A OWNER=() VERSIONS=() PKG_VERSION=()
for inputs in "${REPO_ROOT}"/pkgs/*/mica-inputs; do
    [ -f "${inputs}" ] || continue
    dir="$(dirname "${inputs}")"
    rel="${dir#"${REPO_ROOT}"/}"
    name="$(basename "${dir}")"
    for f in Dockerfile prepare.sh producer.env; do
        [ -f "${dir}/${f}" ] || die "${rel} has no ${f}"
    done
    while IFS= read -r line; do
        case "${line}" in
        '' | '#'*) ;;
        ENABLEMENT=* | CONTEXTS=*)
            case "${line}" in *'$'* | *'`'*) die "${rel}/producer.env: no expansions allowed: ${line}" ;; esac
            ;;
        *) die "${rel}/producer.env: not ENABLEMENT or CONTEXTS: ${line}" ;;
        esac
    done <"${dir}/producer.env"
    ENABLEMENT="" CONTEXTS=""
    # shellcheck disable=SC1091
    . "${dir}/producer.env"
    PACKAGES="$(sed -n 's/^package //p' "${inputs}" | tr '\n' ' ')"
    [ -n "${PACKAGES// /}" ] || die "${rel}/mica-inputs declares no package"

    templates="$(find "${dir}/control" -maxdepth 1 -name '*.control' -printf '%f\n' 2>/dev/null | sed 's/\.control$//' | LC_ALL=C sort | tr '\n' ' ')"
    declared="$(printf '%s\n' ${PACKAGES} | LC_ALL=C sort | tr '\n' ' ')"
    [ "${templates}" = "${declared}" ] || die "${rel}: mica-inputs declares '${declared% }' but control/ holds '${templates% }'"

    version="" epoch=""
    for pkg in ${PACKAGES}; do
        template="${dir}/control/${pkg}.control"
        [ "$(field "${template}" Package)" = "${pkg}" ] || die "${rel}/control/${pkg}.control does not name Package: ${pkg}"
        v="$(field "${template}" Version)" e="$(field "${template}" Source-Date-Epoch)"
        [[ "${v}" =~ ^[0-9]+\.[0-9]+\.[0-9]+-[1-9][0-9]*$ ]] || die "${rel}/control/${pkg}.control: Version '${v}' is not <major>.<minor>.<patch>-<revision>"
        [[ "${e}" =~ ^[1-9][0-9]*$ ]] || die "${rel}/control/${pkg}.control: Source-Date-Epoch '${e}' is not a whole number of seconds"
        [ -z "${version}" ] || [ "${v}" = "${version}" ] || die "${rel}: control/${pkg}.control declares ${v}, another template ${version}; a producer has one version"
        [ -z "${epoch}" ] || [ "${e}" = "${epoch}" ] || die "${rel}: control/${pkg}.control declares Source-Date-Epoch ${e}, another template ${epoch}"
        version="${v}" epoch="${e}"
        [ -z "${OWNER[${pkg}]:-}" ] || die "${pkg} is declared by ${OWNER[${pkg}]} and ${name}"
        OWNER["${pkg}"]="${name}"
        PKG_VERSION["${pkg}"]="${v}"
        count="$(printf '%s\n' ${ENABLEMENT} | sed -n "s/^${pkg}=//p")"
        [[ "${count}" =~ ^[0-9]+$ ]] || die "${rel}/producer.env: ENABLEMENT has no <count> for ${pkg}"
    done
    VERSIONS["${name}"]="${version}"
    for e in ${ENABLEMENT}; do
        case " ${PACKAGES} " in *" ${e%%=*} "*) ;; *) die "${rel}/producer.env: ENABLEMENT names ${e%%=*}, which it does not emit" ;; esac
    done
    for c in ${CONTEXTS}; do
        [[ "${c}" =~ ^[a-z][a-z0-9-]*=.+$ ]] || die "${rel}/producer.env: CONTEXTS entry '${c}' is not <name>=<path>"
        [ -d "${REPO_ROOT}/${c#*=}" ] || die "${rel}/producer.env: context ${c} is not a directory"
    done

    [ "${DIR_FOR}" != "${name}" ] || { echo "${rel}"; exit 0; }
    ROWS+=("${name} ${rel} $(printf '%s' "${PACKAGES% }" | tr -s ' ' ',') $(printf '%s' "${ENABLEMENT}" | tr -s ' ' ',')")
done
[ -z "${DIR_FOR}" ] || die "'${DIR_FOR}' is not a producer under pkgs/"
if [ -n "${VERSION_OF}" ]; then
    [ -n "${VERSIONS[${VERSION_OF}]:-}" ] || die "'${VERSION_OF}' is not a producer under pkgs/"
    echo "${VERSIONS[${VERSION_OF}]}"
    exit 0
fi
# A package built here that depends on another package built here names its
# exact version.
while IFS= read -r template; do
    for pkg in "${!PKG_VERSION[@]}"; do
        while IFS= read -r pin; do
            [ "${pin}" = "${PKG_VERSION[${pkg}]}" ] ||
                die "${template#"${REPO_ROOT}"/} depends on ${pkg} (= ${pin}), but its template declares ${PKG_VERSION[${pkg}]}"
        done < <(sed -n "/^Depends:/s/.*[ ,]${pkg} (= \([^)]*\)).*/\1/p" "${template}")
        if grep -Ec "^Depends:.*[ ,]${pkg}( \(|,|$)" "${template}" >/dev/null &&
            ! grep -Ec "^Depends:.*[ ,]${pkg} \(= " "${template}" >/dev/null; then
            die "${template#"${REPO_ROOT}"/} depends on ${pkg} without its exact version"
        fi
    done
done < <(find "${REPO_ROOT}"/pkgs/*/control -name '*.control' 2>/dev/null | LC_ALL=C sort)
[ "${#ROWS[@]}" -gt 0 ] || die "no producer under pkgs/"
printf '%s\n' "${ROWS[@]}"
