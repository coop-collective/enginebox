FROM debian:bookworm-slim
RUN apt-get update && apt-get install --no-install-recommends -y coturn \
    && rm -rf /var/lib/apt/lists/*
USER 65534:65534
ENTRYPOINT ["/usr/bin/turnserver"]
CMD ["--no-config", "--listening-port=3478", "--min-port=49160", "--max-port=49220", "--realm=enginebox-test", "--user=fixture:fixture-only", "--lt-cred-mech", "--no-tls", "--no-dtls", "--no-cli", "--no-multicast-peers", "--allow-loopback-peers", "--log-file=stdout", "--pidfile=/tmp/turn.pid"]
