# ph-reactor container image.
#
# Deliberately NOT a from-source build: the release workflow already produces
# a fully static x86_64-musl binary, and reusing that exact artifact means the
# snap, the GitHub release asset and this image are bit-identical -- one
# binary, one sha256. Build it first; scripts/build-image.sh does both.
#
# Base is distroless/static, not scratch. TLS roots are not the reason:
# Cargo.lock pins webpki-roots rather than rustls-native-certs, so reqwest's
# CA bundle is compiled into the binary. The base is here for the `nonroot`
# uid in /etc/passwd, a writable /tmp, and a layer that receives CVE patches.

FROM alpine:3.20 AS prep
# distroless has no shell, so the state dir and its ownership are staged here.
RUN mkdir -p /var/lib/ph-reactor \
 && chown 65532:65532 /var/lib/ph-reactor \
 && chmod 700 /var/lib/ph-reactor

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=prep --chown=65532:65532 /var/lib/ph-reactor /var/lib/ph-reactor
COPY --chown=65532:65532 dist/ph-reactor /usr/local/bin/ph-reactor

ENV PH_REACTOR_STATE_DIR=/var/lib/ph-reactor

USER 65532:65532
WORKDIR /var/lib/ph-reactor

# libp2p (25422: IANA-unassigned, RFC 6335 User range, below the Linux
# ephemeral floor) and the console. The console is ClusterIP-only in k8s --
# its API is unauthenticated by design.
EXPOSE 25422/tcp
EXPOSE 4002/tcp

# Foreground, never --daemonize: Kubernetes is the supervisor, so the fork
# and pidfile path is bypassed entirely. With no D-Bus session bus the tray
# self-disables, which is the documented headless path.
ENTRYPOINT ["/usr/local/bin/ph-reactor"]
CMD ["run"]
