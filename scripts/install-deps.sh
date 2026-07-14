#!/usr/bin/env bash
#
# install-deps.sh — installs rtsp-generator's build dependencies and runtime dependencies
# (ffmpeg, MediaMTX) on a Debian-family host. Does NOT install Rust or build rtsp-gen itself —
# see the README's "Build (Debian)" section for that.
#
# Usage: sudo ./scripts/install-deps.sh
#
# Env vars:
#   FORCE=1   reinstall/upgrade MediaMTX even if a binary is already on $PATH

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: this script installs system packages and writes to /usr/local/bin; re-run with sudo." >&2
    exit 1
fi

echo "==> Installing build dependencies and ffmpeg (apt)"
apt update
apt install -y build-essential pkg-config libssl-dev libudev-dev curl git ffmpeg

echo "==> Installing MediaMTX"
if command -v mediamtx >/dev/null 2>&1 && [[ "${FORCE:-0}" != "1" ]]; then
    echo "mediamtx already on \$PATH ($(command -v mediamtx)); skipping. Set FORCE=1 to reinstall/upgrade."
else
    case "$(uname -m)" in
        x86_64)  arch=amd64 ;;
        aarch64) arch=arm64 ;;
        armv7l)  arch=armv7 ;;
        armv6l)  arch=armv6 ;;
        *)
            echo "error: unsupported architecture $(uname -m) — pick a release manually from" >&2
            echo "https://github.com/bluenviron/mediamtx/releases" >&2
            exit 1
            ;;
    esac

    tag="$(curl -fsSL https://api.github.com/repos/bluenviron/mediamtx/releases/latest \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    if [[ -z "$tag" ]]; then
        echo "error: could not determine the latest MediaMTX release from the GitHub API" >&2
        exit 1
    fi

    asset="mediamtx_${tag}_linux_${arch}.tar.gz"
    url="https://github.com/bluenviron/mediamtx/releases/download/${tag}/${asset}"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT
    echo "Downloading MediaMTX ${tag} (${arch})..."
    curl -fsSL "$url" -o "$tmpdir/$asset"
    tar -xzf "$tmpdir/$asset" -C "$tmpdir" mediamtx
    install -m 755 "$tmpdir/mediamtx" /usr/local/bin/mediamtx
    echo "Installed mediamtx ${tag} to /usr/local/bin/mediamtx"
fi

echo "==> Done"
echo "ffmpeg:   $(command -v ffmpeg)"
echo "mediamtx: $(command -v mediamtx)"
