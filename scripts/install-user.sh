#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${CARGO:-$HOME/.cargo/bin/cargo}"

"$cargo_bin" build --release --manifest-path "$project_dir/service/Cargo.toml"
install -Dm755 "$project_dir/service/target/release/voxtype-meeting-service" "$HOME/.local/bin/voxtype-meeting-service"
install -Dm644 "$project_dir/service/systemd/voxtype-meeting-service.service" "$HOME/.config/systemd/user/voxtype-meeting-service.service"
systemctl --user daemon-reload
systemctl --user enable --now voxtype-meeting-service.service
