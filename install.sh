#!/bin/sh
# Installs dumBot from its GitHub release:
#
#   curl -fsSL https://raw.githubusercontent.com/vangheem/dumbot/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/vangheem/dumbot/main/install.sh | sh -s -- --server
#
# Options:
#   --server          install the command line and run it as a service (`dumbot install-server`)
#   --cli             install the command line only (no app; what servers need)
#   --version vX.Y.Z  a release other than the latest
#   --dir DIR         where to put `dumbot` (default ~/.local/bin, or $DUMBOT_INSTALL_DIR)
set -eu

REPO=vangheem/dumbot
dir=${DUMBOT_INSTALL_DIR:-$HOME/.local/bin}
version=latest
server=0
cli=0

while [ $# -gt 0 ]; do
    case "$1" in
        --server) server=1; cli=1 ;;
        --cli) cli=1 ;;
        --version) version=${2:?--version needs a tag, like v0.3.0}; shift ;;
        --dir) dir=${2:?--dir needs a folder}; shift ;;
        -h|--help) sed -n '2,13p' "$0" 2>/dev/null || true; exit 0 ;;
        *) echo "dumbot install: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '%s\n' "$*"; }
die() { printf 'dumbot install: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

has curl || die "needs curl"
has tar || die "needs tar"

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) die "no releases for $arch" ;;
esac
case "$os" in
    Darwin)
        # One universal binary: the app and the command line.
        asset=dumbot-macos-universal.tar.gz ;;
    Linux)
        if [ "$cli" = 1 ] || [ "$arch" != x86_64 ]; then
            [ "$cli" = 1 ] || say "No app build for Linux $arch yet; installing the command line (dumbot server, …)."
            asset=dumbot-cli-linux-$arch.tar.gz
        else
            asset=dumbot-linux-x86_64.tar.gz
        fi ;;
    *) die "no releases for $os" ;;
esac

if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$version"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ($version)…"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "couldn't download $base/$asset"
curl -fsSL "$base/$asset.sha256" -o "$tmp/sum" || die "couldn't download its checksum"
want=$(cut -d' ' -f1 "$tmp/sum")
if has sha256sum; then
    got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
    got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
[ "$want" = "$got" ] || die "$asset doesn't match its SHA-256; not installing it"

mkdir -p "$tmp/x" "$dir"
tar -xzf "$tmp/$asset" -C "$tmp/x"
[ -f "$tmp/x/dumbot" ] || die "$asset has no dumbot binary"
chmod 755 "$tmp/x/dumbot"
# Moved into place in one step, so a running dumbot keeps its copy.
mv -f "$tmp/x/dumbot" "$dir/dumbot.new"
mv -f "$dir/dumbot.new" "$dir/dumbot"
"$dir/dumbot" help >/dev/null 2>&1 || die "the installed dumbot doesn't run on this machine"
say "Installed dumbot to $dir/dumbot"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "Note: $dir isn't on your PATH; add it (e.g. in ~/.profile): export PATH=\"$dir:\$PATH\"" ;;
esac
has docker || say "Note: bots run in Docker, which isn't installed: https://docs.docker.com/engine/install/"
if [ "$server" = 0 ] && ! has opencode; then
    say "Note: the app uses opencode (your models and logins): https://opencode.ai"
fi

if [ "$server" = 1 ]; then
    "$dir/dumbot" install-server
elif [ "$os" = Darwin ]; then
    say "Open it with \`dumbot\`, or add it to Applications with \`dumbot install-app\`."
else
    say "Open it with \`dumbot\`. To run bots on this machine for an app elsewhere: \`dumbot install-server\`."
fi
