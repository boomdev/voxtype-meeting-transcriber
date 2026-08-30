#!/usr/bin/env bash
set -euo pipefail

# Builds this repository's capture service into ~/.local and enables a user
# systemd unit. No sudo or pkexec is required. This does not install the
# Omarchy plugin; use `omarchy plugin add` for that.

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${CARGO:-$HOME/.cargo/bin/cargo}"

if [[ ! -x "$cargo_bin" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    cargo_bin="$(command -v cargo)"
  else
    echo "Rust's cargo is required to build voxtype-meeting-service." >&2
    echo "Install a Rust toolchain (rustup), then run this script again." >&2
    exit 1
  fi
fi

"$cargo_bin" build --release --manifest-path "$project_dir/service/Cargo.toml"
install -Dm755 "$project_dir/service/target/release/voxtype-meeting-service" "$HOME/.local/bin/voxtype-meeting-service"
install -Dm644 "$project_dir/service/systemd/voxtype-meeting-service.service" "$HOME/.config/systemd/user/voxtype-meeting-service.service"
systemctl --user daemon-reload
systemctl --user enable --now voxtype-meeting-service.service

echo "Installed voxtype-meeting-service for the current user."
echo "Remove it later with $project_dir/scripts/uninstall-user.sh"
