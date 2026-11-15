# mokuzu with everything its optional tools need: the code sandbox, the browser and its virtual
# desktop, and Node and uv for MCP servers.
#
#   docker build -t mokuzu .                       # builds the binary from this checkout
#   docker build --build-arg BINARY=prebuilt ...   # uses dist/<arch>/mokuzu instead (release CI)

ARG BINARY=source

FROM rust:1-bookworm AS source
RUN apt-get update && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mokuzu && mkdir /out && cp target/release/mokuzu /out/

# Binaries the release workflow already built, one per architecture.
FROM scratch AS prebuilt
ARG TARGETARCH
COPY dist/${TARGETARCH}/mokuzu /out/mokuzu

FROM ${BINARY} AS binary

FROM debian:bookworm-slim
# The packages install.sh adds, under Debian 12's names.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl bubblewrap xvfb matchbox-window-manager ffmpeg xdotool \
        python3 python3-pandas python3-numpy python3-matplotlib python3-openpyxl python3-pil \
        python3-docx python3-requests fonts-noto-cjk nodejs npm \
        libgtk-3-0 libdbus-glib-1-2 libasound2 libxtst6 libx11-xcb1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.9 /uv /uvx /usr/local/bin/
# Debian does not package python-pptx.
RUN uv pip install --system --break-system-packages --no-cache python-pptx
# Download the browser's Firefox into the image. Its last Linux arm64 build is firefox-55, so
# arm64 stays on that core, as mokuzu's browser does.
ARG TARGETARCH
RUN pin=; [ "$TARGETARCH" = arm64 ] && pin="--with invisible-core<56"; \
    uvx --from invisible-playwright $pin invisible-playwright fetch
COPY --from=binary /out/mokuzu /usr/local/bin/mokuzu
# The config (with its OAuth sign-ins) and the database, workspace, and browser profile.
VOLUME ["/root/.config/mokuzu", "/root/.local/share/mokuzu"]
ENTRYPOINT ["mokuzu"]
CMD ["serve"]
