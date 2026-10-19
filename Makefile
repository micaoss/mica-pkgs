# mica-pkgs: the feature packages Mica writes itself, packed as Debian packages.
# The scripts do the work; this file only routes. `make` alone is `make help`.

MICA_ARCH ?= arm64

.PHONY: help deps deps-check rust-gate deb pool offline package-gate publish lint check

help:
	@echo "  deps                check locks/ and verify every pinned lock against its release (anonymous)"
	@echo "  deps-check          check locks/ offline"
	@echo "  lint                shell hygiene (mica-tools shell-lint)"
	@echo "  rust-gate           fmt, clippy -D warnings, nextest, doctests, cargo-deny, cargo-shear, typos, in the mica-build-env rust image"
	@echo "  check               lint, rust-gate"
	@echo "  deb                 every producer for MICA_ARCH (amd64|arm64|armhf) into _out/debs/<arch>/pool"
	@echo "  pool                deb for every architecture, indexed"
	@echo "  offline             pool from a clean checkout with only the pinned inputs; prints where the outputs are"
	@echo "  package-gate        the package gate over every pool, with a rebuild"
	@echo "  publish             TAG=<YYYYMMDD-HHMM>: attach _out/debs to that release (release workflow only)"

deps:
	bin/mica-tools locks verify
deps-check:
	bin/mica-tools locks check

lint:
	bin/mica-tools shell-lint
rust-gate:
	bash scripts/gate/rust-gate.sh

check: lint rust-gate

deb:
	set -e; rows="$$(bash scripts/deb/producers.sh)"; \
	for p in $$(printf '%s\n' "$$rows" | cut -d' ' -f1); do bash scripts/deb/build.sh --producer "$$p" --arch $(MICA_ARCH); done

pool:
	$(MAKE) deb MICA_ARCH=amd64
	$(MAKE) deb MICA_ARCH=arm64
	$(MAKE) deb MICA_ARCH=armhf
	bin/mica-tools pool index --arch amd64
	bin/mica-tools pool index --arch arm64
	bin/mica-tools pool index --arch armhf

offline:
	bash scripts/build/offline.sh

package-gate:
	bash scripts/deb/package-gate.sh

publish:
	@[ -n "$(TAG)" ] || { echo "usage: make publish TAG=<YYYYMMDD-HHMM>" >&2; exit 1; }
	bash scripts/build/release.sh "$(TAG)"
