#!/usr/bin/env bash
set -euo pipefail

# Reverses scripts/install-user.sh. Does not delete meeting transcripts or
# Voxtype configuration. No sudo or pkexec is required.

systemctl --user disable --now voxtype-meeting-service.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/voxtype-meeting-service.service"
rm -f "$HOME/.local/bin/voxtype-meeting-service"
systemctl --user daemon-reload 2>/dev/null || true

echo "Removed the Voxtype Meeting Transcriber user service and binary."
echo "Meeting data under ~/.local/share/voxtype-meeting-service was left in place."
