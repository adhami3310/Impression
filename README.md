# Impression

## Contributing
Issues and merge requests are more than welcome. However, please take the following into consideration:

- This project follows the [GNOME Code of Conduct](https://wiki.gnome.org/Foundation/CodeOfConduct)
- Only Flatpak is supported

## Development

### GNOME Builder
The recommended method is to use GNOME Builder:

1. Install [GNOME Builder](https://apps.gnome.org/app/org.gnome.Builder/) from Flathub
1. Open Builder and select "Clone Repository..."
1. Clone `https://gitlab.com/adhami3310/impression.git` (or your fork)
1. Press "Run Project" (▶) at the top, or `Ctrl`+`Shift`+`[Spacebar]`.

### Flatpak
You can install Impression from the latest commit:

1. Install [`org.flatpak.Builder`](https://github.com/flathub/org.flatpak.Builder) from Flathub
1. Clone `https://gitlab.com/adhami3310/impression.git` (or your fork)
1. Run `flatpak run org.flatpak.Builder --install --user --force-clean build-dir io.gitlab.adhami3310.impression.json` in the terminal from the root of the repository.

### Meson
You can build and install on your host system by directly using the Meson buildsystem:

1. Install `blueprint-compiler`
1. Run the following commands (with `/usr` prefix):
```
meson --prefix=/usr build
ninja -C build
sudo ninja -C build install
```
