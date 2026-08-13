#!/usr/bin/env bash
# Install the cua exact-window restore GNOME Shell extension for X11.
set -euo pipefail

UUID="winrestore@cua"
SRC="$(cd "$(dirname "$0")" && pwd)/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

mkdir -p "$DEST"
cp -f "$SRC/metadata.json" "$SRC/extension.js" "$DEST/"

current=$(gsettings get org.gnome.shell enabled-extensions 2>/dev/null || echo "@as []")
python3 - "$current" "$UUID" <<'PY'
import ast
import subprocess
import sys

try:
    enabled = ast.literal_eval(sys.argv[1])
except (SyntaxError, ValueError):
    enabled = []
if sys.argv[2] not in enabled:
    enabled.append(sys.argv[2])
subprocess.run(
    ["gsettings", "set", "org.gnome.shell", "enabled-extensions", str(enabled)],
    check=True,
)
print("enabled-extensions ->", enabled)
PY

echo "Installed $UUID to $DEST."
echo "Log out and back in once so GNOME Shell loads the extension."
