#!/usr/bin/env bash

BUILD_DIR="translation-build/"
if [ -d "$BUILD_DIR" ]; then
	rm -r translation-build
fi

meson translation-build
meson compile -C translation-build impression-pot
meson compile -C translation-build impression-update-po

rm -r translation-build
