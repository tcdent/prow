# prow-server: the static binary and the built web app, nothing else. Built by bin/build-image,
# which compiles both first; this file only copies them in.
FROM scratch
COPY target/x86_64-unknown-linux-musl/release/prow-server /prow-server
COPY web/dist /web
USER 1000:1000
ENV PROW_DATA=/data PROW_WEB=/web
ENTRYPOINT ["/prow-server"]
