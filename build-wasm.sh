#!/usr/bin/env bash
# Build lume-wasm with wasm-pack.
# Install wasm-pack: cargo install wasm-pack  (0.14.0+ required for --profile)
#
# Usage:
#   ./build-wasm.sh                  # optimised wasm-release profile, bundler target
#   ./build-wasm.sh --dev            # dev build (faster, unoptimised)
#   ./build-wasm.sh --target web     # plain browser ESM (no bundler)
#   ./build-wasm.sh --target nodejs

set -euo pipefail

PROFILE="--profile wasm-release"
TARGET="bundler"

for arg in "$@"; do
  case "$arg" in
    --dev)    PROFILE="--dev" ;;
    --target) ;;
    *)        TARGET="$arg" ;;
  esac
done

wasm-pack build lume-wasm $PROFILE --target "$TARGET"
echo "→ output: lume-wasm/pkg/"
