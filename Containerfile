FROM rust:latest AS build
WORKDIR /build

COPY . .

RUN cargo build --release

FROM debian:12-slim
WORKDIR /app

COPY --from=build /build/target/release/gnss_exporter .

EXPOSE 9123
ENTRYPOINT "./gnss_exporter"
