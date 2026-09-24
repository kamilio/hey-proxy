#!/bin/sh
set -eu
stage=$1
base_url=$2
codex_home=$3
model=$4
umask 077
export TMPDIR="$stage"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
command -v python3 >/dev/null || { echo 'Remote host needs Python 3 for service setup' >&2; exit 1; }
python3 "$stage/src/remote_service.py" preflight
mkdir -p "$HOME/.hey-proxy"
lock="$HOME/.hey-proxy/rollout.lock"
mkdir "$lock" || { echo 'Another rollout is active (or left a stale ~/.hey-proxy/rollout.lock)' >&2; exit 1; }
trap 'rmdir "$lock"' EXIT
if ! command -v cargo >/dev/null; then
    command -v curl >/dev/null || { echo 'Install Rust/Cargo or curl on this host first' >&2; exit 1; }
    echo 'Installing Rust toolchain for this user'
    curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o "$stage/rustup-init.sh"
    sh "$stage/rustup-init.sh" -y --profile minimal --no-modify-path
fi
# Compile natively: macOS/Linux and ARM/x86 do not need a matching controller binary.
# Reuse build artifacts between rollouts.
export CARGO_TARGET_DIR="$HOME/.hey-proxy/build"
export CARGO_INCREMENTAL=0
cargo build --release --locked --manifest-path "$stage/Cargo.toml"
built="$CARGO_TARGET_DIR/release/hey-proxy"
"$built" --config "$stage/remote-config.json" --init
python3 "$stage/src/remote_service.py" install "$built" "$stage/remote-config.json" "$base_url" "$codex_home" "$model"
