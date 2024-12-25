FROM clux/muslrust:stable AS chef
USER root
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
WORKDIR /compose-scaler
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
WORKDIR /compose-scaler
COPY --from=planner /compose-scaler/recipe.json recipe.json
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM docker AS runtime
WORKDIR /compose-scaler
COPY --from=builder /compose-scaler/target/x86_64-unknown-linux-musl/release/app /compose-scaler
COPY --from=planner /compose-scaler/themes /compose-scaler/themes/
ENTRYPOINT ["/compose-scaler/app"]