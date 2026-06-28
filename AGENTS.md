# cling — build & test commands

## Build
- `cargo build --workspace` — all members (needs `libgtk-4-dev libadwaita-1-dev` for `cling-show`; SQLCipher/OpenSSL bundled).
- `cargo build --workspace --exclude cling-show` — everything except the GTK4 UI (no GTK4 needed).
- `cargo build -p cling-backends --features x11` — X11 backend.
- `cargo build -p cling-daemon --features x11` — daemon with X11 backend.
- `cargo build -p cling-show` — GTK4 popup (needs `libgtk-4-dev libadwaita-1-dev`).

## Test
- `cargo test --workspace` — full test suite.

## Lint / typecheck
- `cargo clippy --workspace -- -D warnings`
- `cargo fmt --all -- --check`

## Run
- Daemon: `cargo run -p cling-daemon --features x11 -- --backend x11`
- CLI: `cargo run -p cling-cli -- list` / `pick N` / `add` / `clear` / `pause` / `state`

## Notes
- No Flatpak (sandbox blocks clipboard). Distribution = native packages + AppImage.
- GNOME-Wayland parity needs the GNOME Shell extension in `extension/`.
- Wayland backends (wlroots/KDE) are feature-gated (`wayland`) and still runtime work.
