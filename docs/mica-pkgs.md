# mica-pkgs

The feature packages Mica writes itself: Debian packages a product chooses, built into its root,
whose daemons micad only configures and starts. This document describes the packages, their
contract with micad, and how the repository is built, checked and released.

## 1. Neighbours

| Repository | What it is | The relation |
| --- | --- | --- |
| `micaoss/mica-build-tools` | The build and release tools, pinned by `locks/mica-build-tools.pin` | `bin/mica-tools` runs the pinned commit |
| `micaoss/mica-build-env` | The build images, pinned by `locks/mica-build-env.lock` | The Rust image compiles and gates; the base image packs, so `${shlibs:Depends}` resolves against the pinned Debian; armhf has no image of its own and is packed in the amd64 C image, against its cross toolchain |
| `micaoss/mica-system-base` | The floor root and both inits | The packages run on it: `passwd` for the service accounts, D-Bus, systemd or OpenRC with `supervise-daemon`, and dropbear (`mica-ssh`) for SFTP |
| `micaoss/mica-core` | micad, the API and the console | micad writes the MQTT configuration and starts both MQTT services by name (section 3) |
| `micaoss/mica-build` | Boards, products and the signed images | Pins a release of this repository and installs its packages, by name, into the roots of the products that select them |

## 2. Packages

Every package is built for amd64, arm64 and armhf.

| Package | Producer | What it installs | systemd | OpenRC | Started |
| --- | --- | --- | --- | --- | --- |
| `mica-mqtt-broker` | `pkgs/mqtt` | `/usr/bin/mica-mqtt-broker`, the local MQTT 3.1.1 broker | `mica-mqtt-broker.service` | `/etc/init.d/mica-mqtt-broker` | By micad, when MQTT is on |
| `mica-mqttd` | `pkgs/mqtt` | `/usr/bin/mica-mqttd`, the application-data bridge to MQTT, and the empty directory `/usr/lib/mica/mqtt-applications.d` | `mica-mqttd.service` | `/etc/init.d/mica-mqttd` | By micad, when MQTT is on |
| `mica-sftp-server` | `pkgs/sftp` | `/usr/lib/sftp-server`, SFTP version 3 | -- | -- | By dropbear, per `sftp` subsystem request |

Each package also installs `/usr/share/doc/<package>/copyright`.

- **mica-mqtt-broker** is rumqttd used as a library: one TCP listener, MQTT 3.1.1, built in code
  from the three keys of its configuration (3.1). No TLS, websockets, console, metrics, clustering
  or bridging exist to turn on. Its postinst creates the system account `mica-mqtt-broker`
  (uid and gid 969), which the service runs as.
- **mica-mqttd** bridges only enrolled applications (3.4) and has no access to micad. It reads
  and watches each application's `com.mica.Item1` tree on the system bus and publishes it as
  `N/<deviceId>/<class>/<instance>/<path>`; `R/...` requests a read and `W/...` a write, which
  reaches the application's `SetValue` only in `full` mode (`read-only` is the default). The
  protocol is a pure state machine (`crates/mica-mqttd/src/bridge.rs`) between a transport and
  the application buses. Its postinst creates the system account `mica-mqttd` (uid and gid 970),
  which the service runs as.
- **mica-sftp-server** is exec'd by dropbear for the `sftp` subsystem and runs as the logged-in
  user. It speaks SFTP version 3 on its standard input and output, uses no other Mica code, and
  has no configuration and no service.

## 3. The contract with micad

micad owns the `mqtt` settings and the lifecycle of the MQTT services. The packages own the
executables, the service files and the formats below. A change to any of this needs both sides.

### 3.1 `/run/mica/mqtt-broker.toml`

The broker's configuration, read once at start (`--config`). micad renders it before it starts
the broker; the file is world-readable, since the broker reads it as `mica-mqtt-broker`. It is
TOML with exactly three keys, all required:

```toml
listen_address = "127.0.0.1"
listen_port = 1883
auth_enabled = false
```

| Key | Type | Meaning |
| --- | --- | --- |
| `listen_address` | string | The address to bind: an IPv4 or IPv6 literal. A host name is a start-up error |
| `listen_port` | integer, 0 to 65535 | The TCP port to bind |
| `auth_enabled` | boolean | Whether a client must present a user name and password |

A missing key or one of the wrong type is a start-up error that names the file. The broker does
not watch the file: a change takes effect when the service is restarted.

With `auth_enabled = true` the broker reads the accounts from
`/var/lib/mica/mqtt-broker-users.toml`, also once at start. The file must be readable by
`mica-mqtt-broker` and holds one table of user name to password:

```toml
[users]
venus = "hunter2"
```

An absent file, or one without the table, is no accounts: every connection is refused until the
file lists one. With `auth_enabled = false` the file is not read. Listening on an address that is
not loopback without authentication is allowed and logged as a warning.

### 3.2 `/run/mica/mqttd-device.env`

The bridge's identity, written by micad before it starts the bridge. It holds one assignment:

```sh
MICA_MQTT_DEVICE_ID=<deviceId>
```

`<deviceId>` becomes the second level of every topic. It is not empty and holds only ASCII
letters, digits, `.`, `_`, `-` and `:`. The file is read as a systemd `EnvironmentFile=` and
sourced by the OpenRC script, so it holds nothing else and no quoting.

Where the bridge connects is not micad's to write. Both service files default to
`MICA_MQTT_BROKER_HOST=localhost`, `MICA_MQTT_BROKER_PORT=1883`, `MICA_MQTT_CLIENT_ID=mica-mqttd`
and `MICA_MQTT_MODE=read-only`, and read `/var/lib/mica/mqttd.env`, when it exists, for other
values of those four. For a broker that authenticates,
`/var/lib/mica/mqttd-credentials.json` holds exactly
`{"username":"<user>","password":"<password>"}`, owned by `mica-mqttd` with mode 0600; absent, the
bridge connects anonymously, and a malformed file or one readable by others refuses the start.

### 3.3 Services

| | Broker | Bridge |
| --- | --- | --- |
| systemd unit | `mica-mqtt-broker.service` | `mica-mqttd.service` |
| OpenRC service | `mica-mqtt-broker` | `mica-mqttd` |
| Runs as | `mica-mqtt-broker` | `mica-mqttd` |
| Needs before start | `/run/mica/mqtt-broker.toml` | `/run/mica/mqttd-device.env` |

- **The packages enable neither.** No package ships a `multi-user.target.wants` link or a
  runlevel link (`ENABLEMENT` in `pkgs/mqtt/producer.env` is 0 for both, and the package gate
  holds the archives to it). micad starts and stops both services from the `mqtt` settings.
- Without its file a service does not run: on systemd the unit's `ConditionPathExists=` skips the
  start, on OpenRC `start_pre` fails it.
- Both are ordered after micad and the network (`After=network.target micad.service` and
  `Wants=micad.service`; `need micad` and `after net`), and both restart after a failure with a
  delay of 5 seconds: `Restart=on-failure`, and `supervise-daemon` with `respawn_delay=5`.
- Under OpenRC both log through `logger`, tagged with the service name.

### 3.4 `/usr/lib/mica/mqtt-applications.d/`

How an application enrols in the bridge. `mica-mqttd` owns the directory and ships it empty; an
application's package installs one regular file in it whose **name** is the application's exact
D-Bus name, `com.mica.<class>[.<suffix>]` (3.5). The content is ignored. The bridge reads the
directory at start and bridges exactly the names it finds.

- The `<class>` becomes a topic level and holds only ASCII letters, digits, `.`, `_`, `-` and `:`.
- `com.mica.micad` cannot be enrolled.
- A name outside the rule, a name that is not a valid D-Bus well-known name, or an entry that is
  not a regular file refuses the bridge's start.
- The same package ships the D-Bus policy that lets the user `mica-mqttd` call
  `com.mica.Item1` on that one name (`GetItems`, `ItemsChanged`, `SetValue`). The bridge has no
  other grant.

### 3.5 The bus-name rule

`crates/mica-busname` is the rule that reads a Mica service name:

```text
com.mica.<class>[.<suffix>]
```

- The name starts with `com.mica.`.
- `<class>` is the third dotted component; `<suffix>` is everything after it, without the
  separating dot, and may itself hold dots.
- No component is empty: `com.mica`, `com.mica.`, `com.mica.sensor.` and `com.mica..sensor` are
  not names.
- No class has a meaning of its own to the rule: `com.mica.ext.sensor` is the class `ext` with
  the suffix `sensor`.

The crate has no dependencies and is not published. micad carries its own copy of the rule with
the same tests; the two must give the same answer for every name.

## 4. The repository

| Path | Content |
| --- | --- |
| `crates/mica-mqtt-broker`, `crates/mica-mqttd` | The broker and the bridge; `dist/` holds each service's systemd unit and OpenRC script |
| `crates/mica-sftp-server` | The SFTP server |
| `crates/mica-busname` | The `com.mica.*` name rule |
| `pkgs/<producer>/` | One producer per directory: `mica-inputs` (its packages and what decides their bytes), `producer.env`, `control/<package>.control`, `prepare.sh`, `Dockerfile`, and `maintainer/<package>/` for maintainer scripts |
| `pkgs/copyright` | The copyright file every package installs |
| `scripts/build`, `scripts/deb`, `scripts/gate` | The compile step, the Rust gate, the producer build, the package gate and the release publisher |
| `locks/`, `bin/mica-tools` | The pinned build environment and build tools; `locks/upstream.lock` is empty |

A producer declares one version for its packages, `<major>.<minor>.<patch>-<revision>`, and a
`Source-Date-Epoch`, literally in each control template. The part before the revision is the crate
version of every binary the producer ships. Either part is bumped with any change to what builds
the package; a package at a released version never changes
(`mica-build-tools:docs/spec/package-versions.md`).

## 5. Building and the gates

The host needs docker with buildx, bash, git, make, curl and jq, and no Rust toolchain.

| Target | What it does |
| --- | --- |
| `make deps` | Checks `locks/` and verifies every pinned lock against its release |
| `make lint` | Shell hygiene over every script (`mica-tools shell-lint`) |
| `make rust-gate` | In the mica-build-env Rust image, with the repository mounted read-only: each producer's version is its binaries' crate version and its `mica-inputs` names every workspace crate they are built from; the declared `rust-version` is the image's rustc; then `cargo fmt --check`, clippy with warnings as errors, nextest, doctests, cargo-deny (licences, bans, advisories), cargo-shear and typos |
| `make check` | `lint` and `rust-gate` |
| `make deb MICA_ARCH=<arch>` | Every producer for one architecture, into `_out/debs/<arch>/pool`: the binaries are compiled in the Rust image of the host's architecture (natively, or cross-compiled; never emulated; armhf from an amd64 host) and packed with `mica-tools deb pack` in the base image of the target architecture, or for armhf in the amd64 C image, where `${shlibs:Depends}` comes from the armhf cross toolchain's shlibs files with their package names written as the target's (`libc6`, `libgcc-s1`) |
| `make pool` | `deb` for every architecture, indexed |
| `make package-gate` | `mica-tools pool gate`; then the pool holds exactly the declared packages at their declared versions, with exact versions for dependencies between them and as many `multi-user.target.wants` links as `ENABLEMENT` declares; then one producer per architecture is rebuilt on a builder with an empty cache and must come out byte-identical |
| `make offline` | `pool` from a clean checkout with only the pinned inputs |

`ci.yml` runs all of it on every push to `main` and every pull request, on a native runner per
architecture (armhf cross-built on the amd64 runner), and then holds every archive against the latest release with `mica-tools pool guard`:
a package at a released version keeps its inputs and its bytes. It publishes nothing.

## 6. Releases

A release is named by its UTC minute, `YYYYMMDD-HHMM`, and cut with
`gh release create <YYYYMMDD-HHMM> --target <commit of main>`. Publishing it runs `release.yml`:
the tag is built and gated as in CI, and `scripts/build/release.sh` publishes it. Nothing
published is ever replaced.

| Where | What |
| --- | --- |
| `ghcr.io/micaoss/mica-pkgs:pool.<arch>.<release>` | The packages for amd64, arm64 or armhf, one layer per `.deb`, each annotated with its producer's `mica.inputs`; an unchanged pool keeps its digest |
| Release asset `mica-pkgs.lock` | `mica-lock v1`: the `release` row, a `pool` row per architecture and a `package` row per package and architecture (`mica-build-tools:docs/spec/release-lock.md`) |
| Release asset `SHA256SUMS` | The sha256 of `mica-pkgs.lock` |

A consumer commits the release's `mica-pkgs.lock` unchanged as `locks/mica-pkgs.lock` with its
pin `locks/pins/mica-pkgs.pin` (the release and the sha256 of its `SHA256SUMS`), checks both with
`mica-tools locks verify`, and takes each package from its `package` row: the layer of that
architecture's `pool` whose digest is the row's sha256.
