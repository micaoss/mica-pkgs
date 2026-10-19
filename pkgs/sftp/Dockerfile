# check=skip=InvalidDefaultArgInFrom
# mica-sftp-server: the SFTP server dropbear runs as /usr/lib/sftp-server.
ARG MICA_BUILD_BASE
FROM ${MICA_BUILD_BASE} AS pack
COPY --from=tools package.json /tools/package.json
COPY --from=tools src /tools/src
COPY --from=deb shlibs.sh /usr/local/bin/shlibs.sh
COPY --from=pkgs copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mica-sftp-server /in/bin/
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
ARG MICA_DEB_SOURCE_REPO
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-sftp-server /stage/usr/lib/sftp-server; \
    install -D -m 0644 /in/copyright            /stage/usr/share/doc/mica-sftp-server/copyright; \
    shlibs_mica_sftp_server="$(shlibs.sh /stage)"; \
    bun /tools/src/cli.ts deb pack --root /stage --control /in/control/mica-sftp-server.control \
            --substitute "shlibs:Depends=${shlibs_mica_sftp_server}" \
            --arch "${MICA_DEB_ARCH}" --out /out
FROM scratch AS out
COPY --from=pack /out/ /
