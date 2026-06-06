# stage 1: compile the Tailwind/daisyUI stylesheet (setup ported from DMS).
# src/web is copied in so the content scanner sees the markup's class names.
FROM node:20-alpine AS assets
WORKDIR /build
COPY assets/package.json assets/package-lock.json ./assets/
RUN cd assets && npm ci
COPY assets ./assets
COPY src/web ./src/web
RUN cd assets \
 && ./node_modules/.bin/tailwindcss -c tailwind.config.js -i css/style.css -o ../src/web/assets/app.css \
 && mkdir -p ../src/web/assets/files \
 && cp node_modules/@fontsource/b612-mono/files/b612-mono-latin-400-normal.* ../src/web/assets/files/ \
 && cp node_modules/@fontsource/b612-mono/files/b612-mono-latin-700-normal.* ../src/web/assets/files/

FROM rust:1-alpine AS build
# build-base: ring (rustls) needs a C compiler on musl
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# freshly built stylesheet + fonts replace the committed ones before
# rust-embed bakes src/web/assets/ into the binary
COPY --from=assets /build/src/web/assets/app.css ./src/web/assets/app.css
COPY --from=assets /build/src/web/assets/files ./src/web/assets/files
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
