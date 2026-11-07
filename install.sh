#!/bin/sh
# Install or update hippo from GitHub releases.
#   private repo: gh api repos/McPatate/hippo/contents/install.sh -H 'Accept: application/vnd.github.raw' | sh
#   public repo:  curl -fsSL https://raw.githubusercontent.com/McPatate/hippo/main/install.sh | sh
set -eu
REPO=McPatate/hippo
DIR=${HIPPO_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) T=aarch64-unknown-linux-gnu ;;
  *) echo "hippo: no prebuilt binary for $(uname -sm); use: cargo install --git https://github.com/$REPO" >&2; exit 1 ;;
esac
A=hippo-$T.tar.gz
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  gh release download -R "$REPO" -p "$A" -p "$A.sha256" -D "$TMP"
else
  curl -fsSL -o "$TMP/$A" "https://github.com/$REPO/releases/latest/download/$A"
  curl -fsSL -o "$TMP/$A.sha256" "https://github.com/$REPO/releases/latest/download/$A.sha256"
fi
(cd "$TMP" && shasum -a 256 -c "$A.sha256" >/dev/null) || { echo "hippo: checksum mismatch, aborting" >&2; exit 1; }
tar -xzf "$TMP/$A" -C "$TMP"
mkdir -p "$DIR"
mv "$TMP/hippo" "$DIR/hippo"
echo "🦛 $("$DIR/hippo" --version) installed to $DIR/hippo"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "   add $DIR to your PATH" ;; esac
