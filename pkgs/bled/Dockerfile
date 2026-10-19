# check=skip=InvalidDefaultArgInFrom
# mica-bled: not enabled by its package (micad starts it); the postinst
# creates its service account.
ARG MICA_BUILD_BASE
FROM ${MICA_BUILD_BASE} AS pack
COPY --from=tools package.json /tools/package.json
COPY --from=tools src /tools/src
COPY --from=deb shlibs.sh /usr/local/bin/shlibs.sh
COPY --from=pkgs copyright /in/copyright
COPY control/ /in/control/
COPY maintainer/ /in/maintainer/
COPY --from=bled-dist mica-bled.service mica-bled.openrc mica-bled.conf /in/dist/
COPY --from=bin mica-bled /in/bin/
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
ARG MICA_DEB_SOURCE_REPO
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-bled           /stage/usr/bin/mica-bled; \
    install -D -m 0644 /in/dist/mica-bled.service  /stage/usr/lib/systemd/system/mica-bled.service; \
    install -D -m 0755 /in/dist/mica-bled.openrc   /stage/etc/init.d/mica-bled; \
    install -D -m 0644 /in/dist/mica-bled.conf     /stage/usr/share/dbus-1/system.d/mica-bled.conf; \
    install -D -m 0644 /in/copyright               /stage/usr/share/doc/mica-bled/copyright; \
    shlibs="$(shlibs.sh /stage)"; \
    bun /tools/src/cli.ts deb pack --root /stage --control /in/control/mica-bled.control \
            --substitute "shlibs:Depends=${shlibs}" \
            --maintainer-scripts /in/maintainer/mica-bled \
            --arch "${MICA_DEB_ARCH}" --out /out
FROM scratch AS out
COPY --from=pack /out/ /
