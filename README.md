<div align="center">
<h1>Impression</h1>

A straight-forward modern application to flash ISOs into drives.

<img src="data/resources/icons/hicolor/scalable/apps/io.gitlab.adhami3310.Impression.svg" width="128" height="128" alt="Impression icon">

[![Latest Tag](https://img.shields.io/gitlab/v/tag/adhami3310/Impression?sort=date&style=for-the-badge)](https://gitlab.com/adhami3310/Impression/-/tags)
[![License](https://img.shields.io/gitlab/license/adhami3310/Impression?style=for-the-badge)](https://gitlab.com/adhami3310/Impression/-/raw/main/COPYING)

</div>

## Interface

<img src="data/resources/screenshots/0.png" alt="Main screen with a chosen ISO and two USB memories">

## Contributing
Issues and merge requests are more than welcome. However, please take the following into consideration:

- This project follows the [GNOME Code of Conduct](https://wiki.gnome.org/Foundation/CodeOfConduct)
- Only Flatpak is supported

## Development

### GNOME Builder
The recommended method is to use GNOME Builder:

1. Install [GNOME Builder](https://apps.gnome.org/app/org.gnome.Builder/) from Flathub
1. Open Builder and select "Clone Repository..."
1. Clone `https://gitlab.com/adhami3310/Impression.git` (or your fork)
1. Press "Run Project" (▶) at the top, or `Ctrl`+`Shift`+`[Spacebar]`.

### Flatpak
You can install Impression from the latest commit:

1. Install [`org.flatpak.Builder`](https://github.com/flathub/org.flatpak.Builder) from Flathub
1. Clone `https://gitlab.com/adhami3310/Impression.git` (or your fork)
1. Run `flatpak run org.flatpak.Builder --install --user --force-clean build-dir io.gitlab.adhami3310.Impression.json` in the terminal from the root of the repository.

### Meson
You can build and install on your host system by directly using the Meson buildsystem:

1. Install `blueprint-compiler`
1. Run the following commands (with `/usr` prefix):
```
meson --prefix=/usr build
ninja -C build
sudo ninja -C build install
```
