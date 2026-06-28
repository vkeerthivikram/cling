# cling

A cross-desktop-environment clipboard manager for Linux, in the spirit of
Ditto (Windows) and CopyQ. Full-fidelity clipboard history (text, rich text,
images, files, opaque MIME), fast keyboard-driven popup UI, scripting CLI,
at-rest encryption, and a system tray. Works on **X11** and **Wayland**
(wlroots + KDE Plasma 6); GNOME-Wayland parity is provided by a companion
GNOME Shell extension until Mutter ships `ext-data-control`.

## Architecture

```
cling-daemon (always-on)  ── owns clipboard backends + SQLite (SQLCipher) + D-Bus
   │  org.cling.ClipboardManager  (session bus)
   ├─ cling-show    GTK4/libadwaita popup UI (spawned on hotkey)
   ├─ cling-cli     scripting client (list/pick/add/clear/pause/lock)
   └─ GNOME Shell extension  (GNOME-Wayland: pushes history to the daemon)
```

Crates:

| crate | role |
|---|---|
| `cling-common` | shared types (`Entry`, `MimeBlob`, `Caps`, …) |
| `cling-core` | `ClipboardProvider` trait + capture manager (policy: pause/exclude/size) |
| `cling-store` | SQLite + FTS5 history store, SQLCipher-at-rest, migrations, prune |
| `cling-backends` | X11 (`x11rb`), wlroots/KDE (`wayland`, gated), mock (tests) |
| `cling-dbus-iface` | `org.cling.ClipboardManager` interface (zbus) |
| `cling-daemon` | wires it all together + D-Bus service |
| `cling-cli` | client binary |
| `cling-show` | GTK4 popup (builds only with `libgtk-4-dev libadwaita-1-dev`) |

## Build

```sh
# Default (everything except the GTK4 UI; SQLCipher/OpenSSL are bundled):
cargo build --workspace --exclude cling-show

# X11 backend (full parity on X11):
cargo build -p cling-backends --features x11
cargo build -p cling-daemon --features x11

# GTK4 popup UI (needs libgtk-4-dev + libadwaita-1-dev):
sudo apt install libgtk-4-dev libadwaita-1-dev
cargo build -p cling-show

# Everything (with GTK4 installed):
cargo build --workspace
```

## Test

```sh
cargo test --workspace --exclude cling-show
```

## Run (development)

```sh
# 1. daemon (X11 example)
DISPLAY=:0 cargo run -p cling-daemon --features x11 -- --backend x11

# 2. query history / pick via the CLI
cargo run -p cling-cli -- list
cargo run -p cling-cli -- pick 1
echo "captured via pipe" | cargo run -p cling-cli -- add
```

Set `CLING_PASSPHRASE` (or pass `--passphrase`) to enable at-rest encryption.

## Display-server parity

| Feature | X11 | wlroots | KDE Plasma 6 | GNOME-Wayland |
|---|---|---|---|---|
| Silent history capture | ✅ | ✅ | ✅ | ✅ via GNOME extension |
| Pop UI via hotkey | ✅ grab | ⚠️ portal/custom-shortcut | ⚠️ portal/custom-shortcut | ⚠️ custom-shortcut |
| Copy-from-history | ✅ | ✅ | ✅ | ✅ |
| Auto-paste (Ctrl+V) | ✅ (XTEST) | ❌ | ❌ | ❌ |
| Exclude-by-source-app | ✅ | ❌ (source hidden) | ⚠️ partial | ✅ via extension |

Auto-paste on Wayland and exclude-by-app on wlroots are protocol-level
limitations, not bugs; see the plan for the documented fallbacks.

## Packaging

`packaging/` contains the systemd user unit and an XDG autostart `.desktop`.
Distribution: AppImage + `.deb` + AUR + the GNOME extension via
extensions.gnome.org. **Flatpak is not supported** — its sandbox blocks
clipboard + Wayland-protocol access.

## Status

Phases P0–P7 scaffolding are implemented, including the **secure GUI unlock
dialog** (P4) and **drag-drop-from-UI + multi-select bulk paste** (P5).

Implemented:
- Full-fidelity encrypted store (SQLCipher + FTS5), capture manager + policy.
- `ClipboardProvider` trait; **X11 backend** (XFIXES capture, selection
  ownership, XTEST auto-paste) and a **mock** backend + harness.
- D-Bus service `org.cling.ClipboardManager` (UI/CLI/extension contract).
- **GTK4/libadwaita popup** (`cling-show`): quick-pick, search, pin/delete,
  drag-out, multi-select bulk paste, and a `--socket` unlock dialog.
- **Secure unlock**: the passphrase is collected by `cling-show --socket` over
  a private same-UID Unix socket and re-opens the store in-process — it never
  crosses the D-Bus session bus.
- **GNOME Shell extension** bridging GNOME-Wayland clipboard to the daemon.
- **CLI**, packaging (systemd unit, autostart `.desktop`, CI).

Remaining runtime work:
- **Wayland data-control backends** (wlroots/KDE) — scaffolded; needs the
  `wayland-client` registry/data-control wiring.
- **Integration testing** of the X11 backend (needs an X server/Xephyr) and the
  GNOME extension (needs each supported shell version).

Wayland auto-paste and wlroots/KDE exclude-by-app are protocol-level
limitations, not bugs.

## License

GPL-3.0-or-later.
