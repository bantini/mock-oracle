FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p mock-oracle-server

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/mock-oracle-server /mock-oracle-server
EXPOSE 1521
ENTRYPOINT ["/mock-oracle-server"]
