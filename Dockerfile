# syntax=docker/dockerfile:1
FROM clux/muslrust
# download the index
RUN cargo search lazy_static
ADD Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    echo 'fn main() {}' > src/main.rs && \
    cargo check --release
ADD . ./
RUN cargo build --release

FROM alpine:latest
RUN apk --no-cache add ca-certificates && \
    addgroup -S app && adduser -S app -G app
USER app
WORKDIR /home/app/
COPY --from=0 /volume/target/x86_64-unknown-linux-musl/release/begonia-proxy ./app
CMD ["./app"]
