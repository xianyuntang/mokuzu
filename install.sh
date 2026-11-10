#!/bin/sh
# Installs the mokuzu binary from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/xianyuntang/mokuzu/main/install.sh | sh
#
# Environment:
#   MOKUZU_VERSION       release tag to install, e.g. v0.1.0 (default: latest)
#   MOKUZU_INSTALL_DIR   where the binary goes (default: ~/.local/bin)
#   MOKUZU_DEPS          1 installs the optional system packages with apt without asking, 0 skips
#   MOKUZU_DOWNLOAD_BASE base URL holding the release files (default: GitHub releases; for mirrors)
# The MITTEN_* names from before the project was renamed still work.
set -eu

repo="xianyuntang/mokuzu"
version="${MOKUZU_VERSION:-${MITTEN_VERSION:-latest}}"
dir="${MOKUZU_INSTALL_DIR:-${MITTEN_INSTALL_DIR:-$HOME/.local/bin}}"
updating="${MOKUZU_UPDATING:-${MITTEN_UPDATING:-}}"

fail() {
    echo "mokuzu install: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) fail "unsupported OS $(uname -s); build from source: cargo install --git https://github.com/$repo mokuzu" ;;
esac
case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "unsupported CPU $(uname -m)" ;;
esac
# A shell under Rosetta reports x86_64 on Apple silicon; take the native build.
if [ "$os" = apple-darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
    arch=aarch64
fi
target="$arch-$os"
[ "$os" = unknown-linux-gnu ] ||
    fail "no prebuilt binary for $target; build from source: cargo install --git https://github.com/$repo mokuzu"

download_base="${MOKUZU_DOWNLOAD_BASE:-${MITTEN_DOWNLOAD_BASE:-}}"
if [ -n "$download_base" ]; then
    base="$download_base"
elif [ "$version" = latest ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    # A progress bar for the archive, when there's a terminal to show it on.
    if [ -t 2 ]; then
        fetch_big() { curl -fL --progress-bar "$1" -o "$2"; }
    else
        fetch_big() { fetch "$1" "$2"; }
    fi
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
    fetch_big() { fetch "$1" "$2"; }
else
    fail "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail "needs sha256sum or shasum to check the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
archive="mokuzu-$target.tar.gz"

echo "downloading $archive ($version)"
fetch_big "$base/$archive" "$tmp/$archive" || fail "download failed: $base/$archive"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "checksum download failed: $base/SHA256SUMS"
expected="$(awk -v f="$archive" '$2 == f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || fail "no checksum for $archive in SHA256SUMS"
[ "$(sha256 "$tmp/$archive")" = "$expected" ] || fail "checksum mismatch for $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$dir"
cp "$tmp/mokuzu-$target/mokuzu" "$dir/mokuzu.tmp"
chmod 755 "$dir/mokuzu.tmp"
# Rename into place, so a running `mokuzu serve` keeps its old binary until restarted.
mv -f "$dir/mokuzu.tmp" "$dir/mokuzu"
echo "installed $("$dir/mokuzu" --version) to $dir/mokuzu"
# Installed before as mitten: the old name now runs mokuzu, which moves mitten's config, data,
# and background service over on its first start.
if [ -e "$dir/mitten" ] && [ ! -L "$dir/mitten" ]; then
    ln -sf mokuzu "$dir/mitten"
    echo "mitten is now mokuzu; \`mitten\` still works and runs mokuzu"
fi

# System packages behind the optional tools: the code sandbox and its Python libraries, CJK
# fonts for charts, the virtual desktop, its live view and taking control of it, Node for `npx` MCP servers, and the
# libraries the browser's Firefox links (Ubuntu 24.04 renamed some with a t64 suffix; the name a
# release lacks is skipped).
# Asked on first installs with apt and a terminal; MOKUZU_DEPS=1 installs without asking, 0 skips.
deps="bubblewrap xvfb ffmpeg xdotool python3 python3-pandas python3-numpy python3-matplotlib
python3-openpyxl python3-pil python3-docx python3-pptx python3-requests fonts-noto-cjk nodejs npm
libgtk-3-0 libgtk-3-0t64 libdbus-glib-1-2 libasound2 libasound2t64 libxtst6 libx11-xcb1"
want_deps="${MOKUZU_DEPS:-}"
if [ -z "$updating" ] && [ "$want_deps" != 0 ] && [ "$os" = unknown-linux-gnu ] &&
    command -v apt-get >/dev/null 2>&1; then
    if [ -z "$want_deps" ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
        printf 'install the system packages for sandboxed code, charts, the browser desktop, and\nMCP servers (needs sudo)?\n  %s\n[Y/n] ' "$(echo $deps)" >/dev/tty
        read -r answer </dev/tty || answer=n
        case "$answer" in [nN]*) want_deps=0 ;; *) want_deps=1 ;; esac
    fi
    if [ "$want_deps" = 1 ]; then
        sudo=""
        [ "$(id -u)" = 0 ] || sudo=sudo
        $sudo apt-get update -q || echo "note: apt-get update failed; trying the install anyway"
        # Skip names this release doesn't carry, so one missing package doesn't stop the rest.
        available=""
        for pkg in $deps; do
            # A real candidate, not a virtual name another package provides.
            if apt-cache policy "$pkg" 2>/dev/null | grep -q 'Candidate: [^(]'; then
                available="$available $pkg"
            else
                echo "note: $pkg is not available here; skipped"
            fi
        done
        # shellcheck disable=SC2086
        $sudo apt-get install -y $available || echo "note: some system packages failed to install"
        if ! command -v uvx >/dev/null 2>&1 && command -v curl >/dev/null 2>&1; then
            echo "installing uv, for MCP servers started with uvx"
            curl -LsSf https://astral.sh/uv/install.sh | sh || echo "note: uv failed to install"
        fi
        # The browser mokuzu comes with runs a patched Firefox that is downloaded once.
        uvx="$(command -v uvx || echo "$HOME/.local/bin/uvx")"
        if [ -x "$uvx" ]; then
            echo "downloading the browser's Firefox"
            "$uvx" --from invisible-playwright invisible-playwright fetch ||
                echo "note: the browser's Firefox failed to download"
        fi
    else
        echo "skipped system packages; for every tool, install them later:
  sudo apt install $(echo $deps)
  curl -LsSf https://astral.sh/uv/install.sh | sh
  uvx --from invisible-playwright invisible-playwright fetch"
    fi
fi

case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "note: $dir is not on your PATH; add it in your shell profile, e.g.
    export PATH=\"$dir:\$PATH\"" ;;
esac
# `mokuzu update` sets this; the setup hints are for first installs.
[ -n "$updating" ] || echo "next:
  mokuzu configure   # set up the model, Discord, tools
  mokuzu install     # run it in the background (reinstall after upgrading)"
