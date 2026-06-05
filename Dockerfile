FROM rust:1-alpine AS build
# build-base: ring (rustls) needs a C compiler on musl
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM alpine:3
RUN adduser -D -H dabping
COPY --from=build /src/target/release/dabping /usr/local/bin/dabping
# ICMP uses unprivileged ping sockets (no setcap: file capabilities break
# exec in rootless containers). podman allows them by default; for docker:
#   docker run --sysctl net.ipv4.ping_group_range="0 2147483647" …

USER dabping
WORKDIR /data
VOLUME /data
EXPOSE 8420
ENTRYPOINT ["dabping", "-c", "/etc/dabping/dabping.toml"]
CMD ["run"]
