# 可选：在仓库根目录构建
#   docker build -f deploy/signal/Dockerfile -t smelt-signal .
# 运行时仍建议前面挂 Caddy/nginx 做 TLS。

FROM rust:1-bookworm AS builder
WORKDIR /src
# 仅编译信令：拷 workspace 清单 + 全 crates 清单（-p 不会编 GUI 原生依赖）
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build -p smelt-signal --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/smelt-signal /usr/local/bin/smelt-signal
ENV SMELT_SIGNAL_BIND=0.0.0.0:7878
EXPOSE 7878
USER nobody
ENTRYPOINT ["/usr/local/bin/smelt-signal"]
