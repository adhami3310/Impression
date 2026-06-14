fn main() {
    // We link libwim/libudf by name; in the Flatpak build they live in /app/lib,
    // which isn't on the linker's default search path. No-op on the host.
    if std::path::Path::new("/app/lib").is_dir() {
        println!("cargo:rustc-link-search=native=/app/lib");
    }
}
