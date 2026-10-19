#!/usr/bin/env bash
# The Rust gate, run inside the mica-build-env rust image by scripts/gate/rust-gate.sh.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "${HERE}/../.." && pwd)"
cd "${WORKSPACE}"
export PATH="$HOME/.cargo/bin:$PATH"

# EVERY PRODUCER'S UPSTREAM VERSION IS ITS BINARIES' CRATE VERSION, AND ITS INPUTS
# NAME EVERY CRATE THEY ARE BUILT FROM. pkgs/<producer>/control/*.control declare
# Version: <major>.<minor>.<patch>-<revision>; the part before the revision must
# be the version of the crate of every binary the producer ships (its
# prepare.sh --bins). Every
# workspace crate in the cargo closure of those binaries (normal and build
# dependencies) must be a `path` line of pkgs/<producer>/mica-inputs, so a
# change to any of them changes the producer's inputs hash.
metadata="$(cargo metadata --locked --no-deps --format-version 1)"
for inputs in pkgs/*/mica-inputs; do
    dir="$(dirname "${inputs}")"
    producer="$(basename "${dir}")"
    declared="$(bash scripts/deb/producers.sh --version-of "${producer}")"
    bins="$(sed -n 's/.*--bins "\([^"]*\)".*/\1/p' "${dir}/prepare.sh")"
    [ -n "${bins}" ] || {
        echo "error: ${dir}/prepare.sh names no --bins" >&2
        exit 1
    }
    for bin in ${bins}; do
        crate="$(jq -r --arg b "${bin}" '.packages[] | select(any(.targets[]; .name == $b and (.kind | index("bin")))) | "\(.name) \(.version)"' <<<"${metadata}")"
        [ "${crate#* }" = "${declared%-*}" ] || {
            echo "error: ${dir} declares ${declared}, but ${bin} is built from ${crate:-no crate}; a producer's upstream version is its binaries' crate version" >&2
            exit 1
        }
    done
    closure="$(jq -r --arg bins "${bins}" --arg root "${WORKSPACE}/" '
        ($bins | split(" ")) as $wanted
        | .packages as $packages
        | [ $packages[] | select(any(.targets[]; (.kind | index("bin")) and (.name as $n | $wanted | index($n)))) | .name ] as $roots
        | def close($set):
            ([ $packages[] | select(.name as $n | $set | index($n)) | .dependencies[]
               | select(.path != null and (.kind == null or .kind == "build")) | .name ] + $set | unique) as $next
            | if ($next | length) == ($set | length) then $set else close($next) end;
          close($roots | unique)
        | .[] as $name | $packages[] | select(.name == $name) | .manifest_path | ltrimstr($root) | sub("/Cargo.toml$"; "")' <<<"${metadata}")"
    for crate_dir in ${closure}; do
        grep -qx "path ${crate_dir}" "${inputs}" || {
            echo "error: ${inputs} does not name path ${crate_dir}, which ${bins} are built from" >&2
            exit 1
        }
    done
done

# THE DECLARED MSRV IS THE LOCKED TOOLCHAIN. Nothing outside this repository builds
# these crates, and every build runs in the mica-build-env rust image, so the one
# version they are verified with is that image's rustc: every gate run is the MSRV
# check, and an image that moves the toolchain fails here until rust-version follows.
. /etc/mica-build/rust.env
declared_msrv="$(jq -r '.packages[0].rust_version' <<<"${metadata}")"
[ "${declared_msrv}" = "${MICA_BUILD_RUSTC%.*}" ] || {
    echo "error: Cargo.toml declares rust-version ${declared_msrv}, but the locked toolchain is rustc ${MICA_BUILD_RUSTC}; set rust-version = \"${MICA_BUILD_RUSTC%.*}\"" >&2
    exit 1
}

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked
cargo nextest run --workspace --locked

# `cargo nextest` does not execute doctests, so this is not a duplicate of the
# line above: without it, a broken doctest passes the gate silently.
cargo test --doc --workspace --locked
cargo deny check licenses bans advisories
# Unused dependencies, and misspellings (`_typos.toml` names what only looks like one).
cargo shear
typos

echo "ALL CHECKS PASSED"
