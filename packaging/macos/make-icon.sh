#!/usr/bin/env bash
# Builds the macOS icon from the master artwork.
#
# macOS is the one platform that wants the shape baked into the file. Finder
# draws the pixels it is given, so a square image sits as a square tile next
# to every other app's rounded one — iOS masks for itself and must not be fed
# a pre-rounded icon, and the Linux themes use the sized PNGs beside this.
#
# Apple's grid: a 1024px canvas with the artwork on an 824px shape centred in
# it, which is the 100px margin every other icon leaves. The shape is a
# superellipse rather than a rounded rectangle — the corners ease in rather
# than meeting an arc, which is the difference you can see when it sits
# beside a real one in the Dock.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
icons="$here/../../crates/mailbox-ui/icons"
src="${1:-$icons/icon.png}"
out="${2:-$icons/icon-macos.png}"

canvas=1024
shape=824
margin=$(((canvas - shape) / 2))
# `n` of the superellipse |x|^n + |y|^n = 1. Apple's is near 5.
power=5

points=$(python3 - "$shape" "$power" <<'PY'
import math, sys
size, n = int(sys.argv[1]), float(sys.argv[2])
r = size / 2
out = []
for i in range(720):
    t = 2 * math.pi * i / 720
    c, s = math.cos(t), math.sin(t)
    x = r * math.copysign(abs(c) ** (2 / n), c)
    y = r * math.copysign(abs(s) ** (2 / n), s)
    out.append(f"{r + x:.3f},{r + y:.3f}")
print(" ".join(out))
PY
)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

magick -size "${shape}x${shape}" xc:black -fill white \
    -draw "polygon $points" -alpha off "$work/mask.png"

magick "$src" -resize "${shape}x${shape}!" "$work/art.png"
magick "$work/art.png" "$work/mask.png" -alpha off -compose CopyOpacity -composite "$work/shaped.png"
magick "$work/shaped.png" -background none -gravity center \
    -extent "${canvas}x${canvas}" "$out"

echo "wrote $out ($(identify -format '%wx%h' "$out"), ${shape}px shape, ${margin}px margin)"
