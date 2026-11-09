# A minimal image holding the static release binary. Build from the repository root:
#   docker build -f deploy/Dockerfile --build-arg ENERGIE_VERSION=v0.1.0 -t energie .
FROM alpine:3.20 AS fetch
ARG ENERGIE_VERSION
ARG TARGETARCH
RUN set -eu; \
    test -n "$ENERGIE_VERSION"; \
    arch="${TARGETARCH:-$(apk --print-arch)}"; \
    case "$arch" in \
      amd64|x86_64) target=x86_64-unknown-linux-musl ;; \
      arm64|aarch64) target=aarch64-unknown-linux-musl ;; \
      arm|armv7) target=armv7-unknown-linux-musleabihf ;; \
      riscv64) target=riscv64gc-unknown-linux-musl ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    archive="energie-$ENERGIE_VERSION-$target.tar.gz"; \
    base="https://github.com/jeroentrappers/energie/releases/download/$ENERGIE_VERSION"; \
    wget -q "$base/$archive" "$base/SHA256SUMS"; \
    grep "  $archive\$" SHA256SUMS | sha256sum -c -; \
    tar -xzf "$archive"; \
    mkdir -p /out/usr/local/bin /out/var/lib/energie /out/etc/energie /out/usr/share/doc/energie; \
    cp "energie-$ENERGIE_VERSION-$target/energie" /out/usr/local/bin/; \
    cp "energie-$ENERGIE_VERSION-$target/LICENSE" "energie-$ENERGIE_VERSION-$target/NOTICE.md" /out/usr/share/doc/energie/; \
    chown -R 65532:65532 /out/var/lib/energie

FROM scratch
COPY --from=fetch /out/ /
USER 65532:65532
VOLUME ["/var/lib/energie"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/energie"]
CMD ["--config", "/etc/energie/energie.toml"]
