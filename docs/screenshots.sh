#!/bin/sh
# Regenerates the README screenshots from docs/demo-config.toml. Needs a desktop session
# and ImageMagick (`magick`). No mixer engine is needed or touched.
set -e
cd "$(dirname "$0")/.."
bin=${BIN:-./target/release/mixrail}
out=docs/screenshots
tmp=$(mktemp -d)
shot() { # name [LAM_OPEN target] [panel height] [zoom]   (THEME=light for the light theme)
    LAM_THEME="${THEME:-dark}" LAM_ZOOM="${4:-1}" LAM_SHOT_H="$3" LAM_DEMO=docs/demo-config.toml LAM_SCREENSHOT="$tmp/$1.ppm" LAM_OPEN="$2" "$bin" gui
    magick "$tmp/$1.ppm" "$out/$1.png"
}
shot mixer
THEME=light shot mixer-light
shot effects fx:mic 1420 0.82
shot eq eq:mic
shot output-effects fx:b2 880
shot output-settings bus:a1
shot input-settings strip:chat
shot apps apps
shot midi midi
rm -r "$tmp"
