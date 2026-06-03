FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM alpine:3
RUN apk add --no-cache libcap-utils \
    && adduser -D -H dabping
COPY --from=build /src/target/release/dabping /usr/local/bin/dabping
# raw-socket fallback for environments without ping_group_range
RUN setcap cap_net_raw+ep /usr/local/bin/dabping

USER dabping
WORKDIR /data
VOLUME /data
EXPOSE 8420
ENTRYPOINT ["dabping", "-c", "/etc/dabping/dabping.toml"]
CMD ["run"]
