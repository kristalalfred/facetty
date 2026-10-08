FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p facetty-server \
    && cp target/release/facetty-server /usr/local/bin/

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/facetty-server /usr/local/bin/
USER 65534:65534
EXPOSE 8740/tcp 8741/udp
ENTRYPOINT ["facetty-server"]
