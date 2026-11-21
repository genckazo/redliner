#!/bin/sh
# Wire the redline overlay into a Rune checkout so it can be built.
# Redline currently builds as an example inside Rune's workspace (it needs Rune's
# vendored dependencies), so this clones Rune at the commit we tested and registers it.
#
#   ./setup.sh && cd rune-src && cargo build --release -p redline
#
# Needs: Rust 1.99.1 (rustup), and for dictation `sox`, `whisper-cli` (whisper.cpp) and a
# model at ~/.redline/models/ggml-base.bin.
set -e
cd "$(dirname "$0")"
ROOT="$(pwd)"
RUNE_REV=8d81d77022927557f0a9f12520162cfb8c5c4fe9

if [ ! -d rune-src ]; then
	git clone https://github.com/anomalyco/rune rune-src
	git -C rune-src checkout "$RUNE_REV"
fi

mkdir -p rune-src/examples/redline
cp redline/Cargo.toml rune-src/examples/redline/Cargo.toml
ln -sfn "$ROOT/redline/src" rune-src/examples/redline/src

if ! grep -q '"examples/redline"' rune-src/Cargo.toml; then
	# Add redline to the workspace members, right after the overlay example.
	awk '{ print } /^    "examples\/overlay",$/ { print "    \"examples/redline\"," }' rune-src/Cargo.toml > rune-src/Cargo.toml.new
	mv rune-src/Cargo.toml.new rune-src/Cargo.toml
fi
echo "Ready: cd rune-src && cargo build --release -p redline"
