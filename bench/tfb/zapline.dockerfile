# TechEmpower Framework Benchmarks image for zapline. TFB builds it from
# frameworks/Rust/zapline/ (the directory prepare-submission.sh creates) and
# runs it next to its Postgres container, reachable as `tfb-database`.
FROM docker.io/rust:1.91-slim-bookworm AS builder

WORKDIR /build
COPY . /build/
ENV RUSTFLAGS="-C target-cpu=native"
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12
ENV DATABASE_URL=postgres://benchmarkdbuser:benchmarkdbpass@tfb-database/hello_world
ENV BIND=0.0.0.0:8080
ENV TEMPLATES=/app/templates
COPY --from=builder /build/target/release/zapline-tfb /app/zapline-tfb
COPY ./templates /app/templates
WORKDIR /app
EXPOSE 8080
CMD ["/app/zapline-tfb"]
