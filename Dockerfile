# syntax=docker/dockerfile:1
# Builds fully static (musl) binaries, then copies each into a distroless
# "static" image: no shell, no libc, no package manager, just CA certificates,
# a nonroot user and the binary.
#
#   docker build --target agent-runtime -t agent-runtime .
#   docker build --target agent-runtime-python -t agent-runtime-python .
#   docker build --target mcp-server -t mcp-server .
#   docker build --target agent-console -t agent-console .
#   docker build --target agent-operator -t agent-operator .
#
# agent-runtime holds no agent. Mount an agent folder at /agent, or bake one
# in with a two-line Dockerfile (see agents/Dockerfile.example).

FROM rust:1.99-alpine AS build
# aws-lc-sys (rustls' crypto) compiles C code: it needs a C toolchain and cmake.
RUN apk add --no-cache musl-dev gcc g++ make cmake perl
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p agent-runtime -p agent-console -p agent-operator -p mcp-server \
 && mkdir /out && cp target/release/agent target/release/agent-console target/release/agent-operator target/release/mcp-server /out/

FROM gcr.io/distroless/static-debian12:nonroot AS mcp-server
COPY --from=build /out/mcp-server /mcp-server
ENV MCP_ADDR=0.0.0.0:8081
EXPOSE 8081
ENTRYPOINT ["/mcp-server"]

FROM gcr.io/distroless/static-debian12:nonroot AS agent-runtime
COPY --from=build /out/agent /agent-runtime
ENV AGENT_ADDR=0.0.0.0:3000 AGENT_DIR=/agent
EXPOSE 3000
ENTRYPOINT ["/agent-runtime"]

# Same runtime, plus a shell (busybox sh) and Python 3, for agents that need
# to run code. Alpine is the smallest base with both; the static binary runs
# on it unchanged. uid 65532 matches distroless' nonroot user.
FROM alpine:3.22 AS agent-runtime-python
# Bytecode caches, pip's bootstrap and the IDE/demo parts of the standard
# library are dropped: ~16 MB nobody runs in an agent container.
RUN apk add --no-cache python3 \
 && cd /usr/lib/python3.* \
 && rm -rf ensurepip idlelib lib2to3 pydoc_data turtledemo turtle.py \
 && find . -name __pycache__ -prune -exec rm -rf {} + \
 && adduser -D -H -u 65532 -s /bin/sh nonroot
COPY --from=build /out/agent /agent-runtime
ENV AGENT_ADDR=0.0.0.0:3000 AGENT_DIR=/agent PYTHONDONTWRITEBYTECODE=1
USER 65532:65532
EXPOSE 3000
ENTRYPOINT ["/agent-runtime"]

FROM gcr.io/distroless/static-debian12:nonroot AS agent-console
COPY --from=build /out/agent-console /agent-console
ENV CONSOLE_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/agent-console"]

FROM gcr.io/distroless/static-debian12:nonroot AS agent-operator
COPY --from=build /out/agent-operator /agent-operator
ENTRYPOINT ["/agent-operator"]
