#!/bin/sh
# Render the PNGs from the SVG sources (needs rsvg-convert, from librsvg).
set -eu
cd "$(dirname "$0")"
for size in 512 256 128 64; do
    rsvg-convert -w $size -h $size logo.svg -o logo-$size.png
done
rsvg-convert -w 512 -h 512 logo-rounded.svg -o logo-rounded-512.png
rsvg-convert -w 512 -h 512 mascot.svg -o mascot-512.png
rsvg-convert -w 128 -h 128 mascot.svg -o mascot-128.png   # Discord emoji size
