# mica-pkgs

The feature packages of Mica OS that Mica writes itself: Debian packages a product chooses, built
into its root. micad, in [mica-core](https://github.com/micaoss/mica-core), only configures and
starts their services.

They run on the floor of [mica-system-base](https://github.com/micaoss/mica-system-base) under
either init, systemd or OpenRC. [mica-build](https://github.com/micaoss/mica-build) installs them
into the roots of the products that select them.
**[docs/mica-pkgs.md](docs/mica-pkgs.md)** describes the packages, their contract with micad, and
how this repository is built, checked and released.

Building and publishing run on [mica-build-tools](https://github.com/micaoss/mica-build-tools)
(`locks/mica-build-tools.pin`) inside the images of
[mica-build-env](https://github.com/micaoss/mica-build-env) (`locks/mica-build-env.lock`).

## Packages

| Package | Arch | What it is |
| --- | --- | --- |
| `mica-mqtt-broker` | amd64, arm64, armhf | A local MQTT 3.1.1 broker, started by micad |
| `mica-mqttd` | amd64, arm64, armhf | The bridge from enrolled `com.mica.*` applications to MQTT, started by micad |
| `mica-bled` | amd64, arm64, armhf | The Bluetooth LE management service for the Mica phone app, started by micad |
| `mica-sftp-server` | amd64, arm64, armhf | SFTP version 3 for dropbear's `sftp` subsystem |

Each package carries the start-up files of its own services, for systemd and OpenRC, and enables
none: micad starts the MQTT daemons and `mica-bled`, and dropbear runs the SFTP server per session.

## Releases

A release is named by its UTC minute, `YYYYMMDD-HHMM`. It is cut with
`gh release create <YYYYMMDD-HHMM> --target <commit of main>`; publishing it runs `release.yml`,
which builds the tag on every architecture, gates it, checks every package against the previous
release and publishes it. `ci.yml` runs the same build and gates on every push and publishes
nothing.

In `ghcr.io/micaoss/mica-pkgs`:

| Tag | Content |
| --- | --- |
| `pool.<arch>.<release>` | The packages for amd64, arm64 or armhf: one layer per `.deb`, each annotated with its producer's `mica.inputs`; an unchanged pool keeps its digest |

On the GitHub release, never replaced (format: `mica-build-tools:docs/spec/release-lock.md`):

| Asset | Content |
| --- | --- |
| `mica-pkgs.lock` | `mica-lock v1`: the `release` row, a `pool` row per architecture and a `package` row per package and architecture |
| `SHA256SUMS` | The sha256 of `mica-pkgs.lock` |

A package's version is its binary's crate version with a Debian revision,
`<major>.<minor>.<patch>-<revision>`. It is bumped with any change to what builds the package; a
package at a released version never changes.

## Consuming a release

1. **Pin it.** Commit the release's `mica-pkgs.lock` unchanged as `locks/mica-pkgs.lock` with its
   pin `locks/pins/mica-pkgs.pin` (the release and the sha256 of its `SHA256SUMS`), checked with
   `mica-tools locks verify`. Move both together.
2. **Take the packages** from the `package` rows: the layer of that architecture's `pool` whose
   digest is the row's sha256. Install the ones the product selects into its root.
3. **Pair it with a mica-system-base release** whose floor and init packages it runs on, and with
   a mica-core release whose micad keeps the contract of
   [docs/mica-pkgs.md](docs/mica-pkgs.md) section 3.

## Building

The host needs docker with buildx, bash, git, make, curl and jq, and no Rust toolchain.

```sh
make deps                  # every lock and pin, verified against its release
make check                 # lint and the Rust gate
make deb MICA_ARCH=amd64   # the archives, in _out/debs/<arch>/pool
make package-gate          # the package rules, including a byte-identical rebuild
```

`make help` lists every target; outputs go under `_out/`.

## Repository layout

| Path | Content |
| --- | --- |
| `crates/mica-mqttd`, `crates/mica-mqtt-broker` | The MQTT bridge and broker |
| `crates/mica-bled` | The BLE management service |
| `crates/mica-sftp-server` | The SFTP server |
| `crates/mica-busname` | The `com.mica.*` name rule |
| `crates/*/dist` | Each service's systemd unit and OpenRC script |
| `pkgs/` | One producer per directory: its packages, inputs, control templates and packing |
| `scripts/` | The producer build, the gates, the package gate and the release publisher |
| `locks/`, `bin/mica-tools` | The pinned build environment and build tools |
| `docs/mica-pkgs.md` | The description of this repository and the contract with micad |

## License

Apache License 2.0; see `LICENSE`.
