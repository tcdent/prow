# prow-server: the binary (DuckDB embedded, so glibc and libstdc++) and the built web app. Built by
# bin/build-image, which compiles both first; this file only copies them in. BASE: the Debian image,
# for a registry of your own.
ARG BASE=debian:bookworm-slim
FROM ${BASE}
COPY target/release/prow-server /usr/local/bin/prow-server
COPY web/dist /web
COPY server/extensions/icu.duckdb_extension /usr/local/lib/prow/extensions/icu.duckdb_extension
USER 1000:1000
ENV PROW_DATA=/data PROW_WEB=/web
ENTRYPOINT ["/usr/local/bin/prow-server"]
