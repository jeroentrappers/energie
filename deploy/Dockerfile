# Builds energie from this source tree into a minimal image. From the repository root:
#   docker build -f deploy/Dockerfile -t energie .
# Releases publish a ready-made image instead: see deploy/docker/README.md.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p energie \
    && mkdir -p /out/usr/local/bin /out/var/lib/energie /out/etc/energie /out/usr/share/doc/energie \
    && cp target/release/energie /out/usr/local/bin/ \
    && cp LICENSE NOTICE.md /out/usr/share/doc/energie/ \
    && chown -R 65532:65532 /out/var/lib/energie

FROM scratch
COPY --from=build /out/ /
LABEL org.opencontainers.image.title="energie" \
      org.opencontainers.image.description="Self-hosted home energy controller" \
      org.opencontainers.image.source="https://github.com/jeroentrappers/energie" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
USER 65532:65532
VOLUME ["/var/lib/energie"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/energie", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/energie"]
CMD ["--config", "/etc/energie/energie.toml"]
