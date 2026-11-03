#!/bin/sh
# Install Mixrail from source for the current user (no root needed): builds it, installs
# it to ~/.local, starts the engine and makes it start at login.
#   packaging/install.sh             install or update
#   packaging/install.sh --no-tray   without the tray icon
#   packaging/install.sh --uninstall remove it again (your settings are kept)
set -eu
cd "$(dirname "$0")/.."
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
units="$HOME/.config/systemd/user"
autostart="$HOME/.config/autostart"

if [ "${1:-}" = "--uninstall" ]; then
    systemctl --user disable --now mixrail 2>/dev/null || true
    rm -f "$bin/mixrail" "$units/mixrail.service" "$apps/mixrail.desktop" \
        "$autostart/mixrail-tray.desktop" "$autostart/mixrail-engine.desktop" \
        "$HOME/.local/share/icons/hicolor/scalable/apps/mixrail.svg"
    systemctl --user daemon-reload 2>/dev/null || true
    echo "Mixrail removed. Your settings are still in ~/.config/mixrail."
    exit 0
fi

cargo build --release --locked
install -Dm755 target/release/mixrail "$bin/mixrail"
install -Dm644 assets/icons/mixrail.svg "$HOME/.local/share/icons/hicolor/scalable/apps/mixrail.svg"
# Full path in the menu entries, in case ~/.local/bin isn't in the desktop's PATH.
mkdir -p "$apps" "$autostart"
sed "s|Exec=mixrail|Exec=$bin/mixrail|" packaging/mixrail.desktop > "$apps/mixrail.desktop"
if [ "${1:-}" != "--no-tray" ]; then
    sed "s|Exec=mixrail|Exec=$bin/mixrail|" packaging/mixrail-tray.desktop > "$autostart/mixrail-tray.desktop"
fi
if command -v systemctl >/dev/null && systemctl --user show-environment >/dev/null 2>&1; then
    mkdir -p "$units"
    sed "s|ExecStart=/usr/bin/mixrail|ExecStart=%h/.local/bin/mixrail|" packaging/mixrail.service > "$units/mixrail.service"
    systemctl --user daemon-reload
    systemctl --user enable mixrail
    systemctl --user restart mixrail
else
    # No systemd: start with the desktop session instead.
    sed "s|Exec=mixrail|Exec=$bin/mixrail|" packaging/mixrail-engine.desktop > "$autostart/mixrail-engine.desktop"
    (nohup "$bin/mixrail" run >/dev/null 2>&1 &)
fi
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin is not in your PATH; the app menu entry works, the 'mixrail' command needs the full path." ;;
esac
echo "Mixrail installed. Open it from your app menu."
