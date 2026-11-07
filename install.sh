#!/bin/sh
# Install or update hippo:
#   curl -fsSL hf.co/buckets/mcpotato/hippo-releases/resolve/install.sh | sh
set -eu
URL=https://huggingface.co/buckets/mcpotato/hippo-releases/resolve
DIR=${HIPPO_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) T=aarch64-unknown-linux-gnu ;;
  *) echo "hippo: no prebuilt binary for $(uname -sm); use: cargo install --git https://github.com/McPatate/hippo" >&2; exit 1 ;;
esac
V=$(curl -fsSL "$URL/latest.txt")
A=hippo-$T.tar.gz
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

curl -fsSL -o "$TMP/$A" "$URL/$V/$A"
curl -fsSL -o "$TMP/$A.sha256" "$URL/$V/$A.sha256"
(cd "$TMP" && shasum -a 256 -c "$A.sha256" >/dev/null) || { echo "hippo: checksum mismatch, aborting" >&2; exit 1; }
tar -xzf "$TMP/$A" -C "$TMP"
mkdir -p "$DIR"
mv "$TMP/hippo" "$DIR/hippo"
echo "🦛 $("$DIR/hippo" --version) installed to $DIR/hippo"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "   add $DIR to your PATH" ;; esac
