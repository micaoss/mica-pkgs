# check=skip=InvalidDefaultArgInFrom
# mica-gce-agent: not enabled by its package (a Compute Engine product enables
# it).
ARG MICA_BUILD_BASE
FROM ${MICA_BUILD_BASE} AS pack
COPY --from=tools package.json /tools/package.json
COPY --from=tools src /tools/src
COPY --from=deb shlibs.sh /usr/local/bin/shlibs.sh
COPY --from=pkgs copyright /in/copyright
COPY control/ /in/control/
COPY --from=gce-dist mica-gce-agent.service mica-gce-agent.openrc /in/dist/
COPY --from=bin mica-gce-agent /in/bin/
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
ARG MICA_DEB_SOURCE_REPO
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-gce-agent          /stage/usr/bin/mica-gce-agent; \
    install -D -m 0644 /in/dist/mica-gce-agent.service /stage/usr/lib/systemd/system/mica-gce-agent.service; \
    install -D -m 0755 /in/dist/mica-gce-agent.openrc  /stage/etc/init.d/mica-gce-agent; \
    install -D -m 0644 /in/copyright                   /stage/usr/share/doc/mica-gce-agent/copyright; \
    shlibs="$(shlibs.sh /stage)"; \
    bun /tools/src/cli.ts deb pack --root /stage --control /in/control/mica-gce-agent.control \
            --substitute "shlibs:Depends=${shlibs}" \
            --arch "${MICA_DEB_ARCH}" --out /out
FROM scratch AS out
COPY --from=pack /out/ /
