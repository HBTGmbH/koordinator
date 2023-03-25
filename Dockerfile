FROM rustlang/rust:nightly-alpine3.19 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN apk add -q --no-cache --update musl-dev \
 && rustup component add rust-src --toolchain nightly 2>/dev/null \
 && mkdir dist/
RUN RUSTFLAGS='-C target-feature=+crt-static' \
    cargo +nightly build -q \
    -Z build-std=std,panic_abort \
    -Z build-std-features=panic_immediate_abort \
    --profile min-size-rel \
    --target x86_64-unknown-linux-musl; \
    cp target/x86_64-unknown-linux-musl/min-size-rel/copy dist/; \
    cp target/x86_64-unknown-linux-musl/min-size-rel/koordinator dist/

FROM scratch
WORKDIR /bin
COPY --from=builder /app/dist/* /bin/
USER 65534
ENTRYPOINT ["/bin/copy"]
