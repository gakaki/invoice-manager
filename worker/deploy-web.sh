#!/usr/bin/env bash
# Build the NSH web app and deploy it to Cloudflare as an assets-only Worker.
# Run from anywhere:  ./worker/deploy-web.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKER="$ROOT/worker"
OUT="$ROOT/app/.nsh/out/发票管理"
PUBLIC="$WORKER/web-public"

# The app pins nsh-* = 0.4.0; the CLI on PATH may be an older 0.3.0.
# Prefer the pinned global CLI (see AGENTS.md global tool install rules),
# allow NSH override, and fall back to `nsh` (e.g. CI installs v0.4.0).
NSH="${NSH:-$HOME/.local/share/codex-tools/bin/nsh}"
[ -x "$NSH" ] || NSH=nsh

# `nsh package --target web` compiles C deps to wasm; cc-rs needs llvm-ar,
# which rustup installs with the llvm-tools component but does not put on PATH.
SYS="$(rustc --print sysroot)"
export PATH="$SYS/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin:$PATH"
command -v llvm-ar >/dev/null || { echo "llvm-ar missing (rustup component add llvm-tools)"; exit 1; }

echo "==> nsh package --target web ($("$NSH" --version))"
(cd "$ROOT/app" && "$NSH" package --target web)

# NSH emits plain files plus .gz/.br pre-compressed variants. Cloudflare
# compresses responses on the fly, so ship only the plain ones.
echo "==> stage plain assets into worker/web-public"
rm -rf "$PUBLIC"
mkdir -p "$PUBLIC"
rsync -a --exclude='*.gz' --exclude='*.br' "$OUT/" "$PUBLIC/"
echo "    $(find "$PUBLIC" -type f | wc -l | tr -d ' ') files, $(du -sh "$PUBLIC" | awk '{print $1}')"

echo "==> wrangler deploy"
cd "$WORKER"
npx wrangler deploy -c wrangler.web.jsonc
