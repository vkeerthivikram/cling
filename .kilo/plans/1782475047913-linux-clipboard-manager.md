# cling — Cross-DE Linux Clipboard Manager (Rust + GTK4)

Status: **Implementation-ready plan.** All major and micro design decisions resolved during planning.

---

## 1. Goal

Build a polished, Ditto/CopyQ-rival clipboard manager for Linux that:
- Works across desktop environments on **X11** and **Wayland**.
- Supports **all clipboard formats** with full fidelity (text, rich text, images, files, opaque MIME).
- Has a fast, keyboard-driven popup UI plus a system tray and a scripting CLI.
- Stores history securely (at-rest encryption) in v1.

Project name: **`cling`**. D-Bus name: **`org.cling.ClipboardManager`**. License: **GPL-3.0**.

---

## 2. Hard constraints (display-server reality)

| Capability | X11 (all DEs) | Wayland wlroots (Sway/Hyprland/River) | Wayland KDE Plasma 6 | Wayland GNOME (Mutter) |
|---|---|---|---|---|
| Silent history capture | ✅ native | ✅ wlr-data-control | ✅ data-control | ✅ via **GNOME Shell extension** bridge |
| Pop UI via global hotkey | ✅ global grab | ⚠️ portal **or** custom-shortcut | ⚠️ portal **or** custom-shortcut | ⚠️ custom-shortcut |
| Copy-from-history (write selection) | ✅ | ✅ | ✅ | ✅ |
| Auto-paste (synthesize Ctrl+V) | ✅ (XTEST) | ❌ | ❌ | ❌ |
| Exclude-by-source-app | ✅ | ❌ (source hidden) | ⚠️ partial | ✅ (extension knows focus app) |

Non-negotiable platform facts the design accepts:
- **GNOME-Wayland history** is achieved via a **companion GNOME Shell extension** (Mutter does not expose `wlr-data-control`/`ext-data-control`). The extension is fragile/version-coupled; it is retired once Mutter ships `ext-data-control`.
- **Wayland auto-paste is impossible** without `ydotool`+`uinput`; we degrade to "copy to clipboard, user presses paste" on Wayland backends.
- **Exclude-by-app cannot work on wlroots/KDE** (source is hidden). Fallback: content/MIME-hint heuristics (`application/x-kde-passwordManagerHint`, secret markers, optional content regex denylist).
- **Flatpak is excluded** — its sandbox blocks clipboard + Wayland-protocol access. Distribution is native packages + AppImage.

---

## 3. Architecture

### 3.1 Process topology

```
autostart (systemd user unit + .desktop fallback)
   │
   ▼
cling-daemon (always-on)
   • ClipboardProvider backends (X11 / Wlroots / KdeWayland / GnomeExt)
   • SQLite + FTS5 history store (SQLCipher-encrypted)
   • D-Bus service org.cling.ClipboardManager
   • Hotkey listener (X11 grab / Wayland portal)
   │  D-Bus session bus
   ├─► cling-show   (short-lived GTK4 popup, spawned on hotkey)
   ├─► cling-cli    (scripting: list/pick/copy/clear/pause/lock)
   └─► GNOME Shell extension (GNOME-Wayland only: pushes history via AddEntry)
```

### 3.2 Backend abstraction (compile-time, auto-detected)

All backends compiled into one binary; daemon selects at startup (`WAYLAND_DISPLAY` + data-control probe, else X11; GNOME-ext pushes via D-Bus). No plugin loader.

```rust
trait ClipboardProvider {
    fn capabilities(&self) -> Caps;                  // {silent_history, auto_paste, source_id}
    fn subscribe(&self) -> BoxStream<ClipboardEvent>;// selection-changed events
    fn read_targets(&self) -> Result<Vec<MimeBlob>>; // full-fidelity read of offered targets
    fn offer(&self, entry: &Entry) -> Result<()>;    // write an entry back to the selection
    fn source_hint(&self) -> Option<AppId>;          // best-effort origin (exclude-apps)
}
```

- `X11Backend`: x11rb / xcb; source via WM_CLASS + `_NET_WM_PID`; auto-paste via XTEST.
- `WlrootsBackend`: `wayland-client` + `wlr-data-control-unstable-v1`.
- `KdeWaylandBackend`: data-control (+ KDE hints where available).
- `GnomeExtBackend`: **passive** — owns no Wayland connection; surfaces `AddEntry` D-Bus calls from the extension as `ClipboardEvent`s. The rest of the daemon is identical.

### 3.3 D-Bus contract

Service `org.cling.ClipboardManager`, object path `/org/cling/ClipboardManager`, session bus. Same-UID only; `AddEntry` rate-limited.

- **Methods:** `Query(offset,limit,filter)` · `Search(query,limit)` · `GetEntry(id)` · `Pick(id, auto_paste)` (auto_paste honored only on X11 backend; no-op elsewhere) · `AddEntry(targets[])` (GNOME ext + external producers) · `Delete(ids[])` · `SetPinned(id,bool)` · `SetGroup(id,group)` · `Clear()` · `Pause(bool)` · `ExcludeApp{Add,Remove}` · `ExcludeContentRegex{Add,Remove}` · `Lock()` · `RequestUnlock()` · `State()`.
- **Signals:** `EntryAdded` · `EntryRemoved` · `StateChanged` (paused/locked) · `Unlocked`.
- **Properties:** `Locked`, `Paused`, `BackendName`, `EntryCount`, `Caps`.
- **Unlock security:** `RequestUnlock()` makes the **daemon itself** show a GTK unlock dialog; the passphrase **never crosses the bus**. Outcome reported via `StateChanged`/`Unlocked`. CLI/UI/extension all use the same path.

### 3.4 GNOME Shell extension

- Runs inside the shell process; hooks selection-change; calls `org.cling.ClipboardManager.AddEntry` over `Gio.DBus`.
- **Capture fidelity:** full-fidelity via `Meta.Selection`/private `ClipboardManager` for all MIME targets, with **automatic text-only fallback** per GNOME version (driven by a compat matrix).
- **Lock buffering:** if DB is locked, buffer the last N entries in-shell; flush on `Unlocked` signal (no history loss while locked).
- **Per-version compat matrix + CI smoke test** that loads the extension against each supported shell version (catches breakage on GNOME bumps).

### 3.5 Hotkeys

- **X11:** native global grab (best UX, zero setup).
- **Wayland:** prefer `org.freedesktop.portal.GlobalShortcuts` where supported (KDE/GNOME); **always** auto-guide the user to bind a DE/compositor custom shortcut to `cling-show` as the universal fallback (works everywhere, incl. Sway/Hyprland).

---

## 4. Data model (SQLite, whole-DB SQLCipher)

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);            -- schema_version, settings
CREATE TABLE groups (id INTEGER PRIMARY KEY, name TEXT UNIQUE, icon TEXT, pos INTEGER);

CREATE TABLE entries (
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
  pinned INTEGER NOT NULL DEFAULT 0, group_id INTEGER REFERENCES groups(id),
  origin TEXT, use_count INTEGER NOT NULL DEFAULT 0,
  preview_kind TEXT,                -- 'text'|'image'|'files'|'rich'|'other'
  deleted INTEGER NOT NULL DEFAULT 0, size_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_entries_ts ON entries(ts DESC);

CREATE TABLE targets (              -- full-fidelity (mime,blob) pairs
  entry_id INTEGER REFERENCES entries(id) ON DELETE CASCADE,
  mime TEXT NOT NULL, blob BLOB NOT NULL,
  PRIMARY KEY (entry_id, mime)
);

CREATE VIRTUAL TABLE entries_fts USING fts5(
  entry_id UNINDEXED, content, tokenize='unicode61 remove_diacritics 2'
);  -- text/plain + stripped text/html; kept in sync via triggers on targets

CREATE TABLE entry_tags (entry_id INTEGER REFERENCES entries(id) ON DELETE CASCADE,
                         tag TEXT, PRIMARY KEY(entry_id, tag));
```

- **Encryption:** whole-DB SQLCipher; argon2id-derived key from passphrase, in-memory only, wiped on `Lock()`/idle autolock.
- **Thumbnails:** generated on-demand by the UI, cached in `~/.cache/cling/thumbs/` (not in the DB).
- **Pruning:** background job deletes non-pinned oldest beyond `max_entries`/past `retention`; respects `group_id`.
- **Migrations:** forward-only, versioned via `meta.schema_version`.
- **Defaults (configurable):** ~1000 entries; skip single blobs > ~50 MB; optional N-day retention.

---

## 5. v1 feature scope (broad — full daily-driver parity)

Capture (full-fidelity, all MIME) · instant hotkey popup · FTS5 search · fully keyboard-driven quick-pick · pin · delete/clear/prune · multi-select delete · images (preview+restore) · files (`text/uri-list` + `x-special/gnome-copied-files` + `application/x-kde-cutselection`) · rich text (HTML/RTF/MD) · opaque MIME · exclude-apps (X11/GNOME) + content/MIME-hint fallback (wlroots/KDE) + global pause · inline edit · system tray (StatusNotifierItem) · **groups/tabs** · **drag-drop-from-UI** · **multi-select bulk paste** · theming (light/dark, libadwaita on GNOME) · `cling-cli` · **at-rest encryption** (unlock-on-launch, idle autolock).

---

## 6. Distribution

AppImage + `.deb` + AUR + GNOME extension via **extensions.gnome.org** (+ bundled zip in releases). COPR/`.rpm` as fast-follow. **No Flatpak.** Cargo workspace build; SQLCipher linked as system lib with bundled fallback. GitHub Actions matrix CI producing all artifacts.

---

## 7. Risks (carried into execution)

1. **GNOME extension version-coupling** — may break per GNOME major release. Mitigation: compat matrix + CI smoke test; text-only fallback; plan to retire when Mutter ships `ext-data-control`.
2. **Wayland auto-paste impossible** — degrade to copy + manual paste; set expectation in UI copy.
3. **Large cumulative v1 scope** (full parity + crypto + extension + DnD + multi-paste). Mitigation: phase order ships a usable core loop early; tail features are independently deferrable.
4. **SQLCipher cross-distro packaging** — bundle fallback; verify on Debian/Fedora/Arch.
5. **Same-UID trust boundary** — session-bus methods callable by same-user processes; acceptable, but no secrets cross the bus (unlock dialog).

---

## 8. Implementation phases (TDD; headless clipboard-simulator harness for backend tests)

> Each phase: write failing tests first, implement, verify green before moving on.

**P0 — Workspace & skeleton**
- Cargo workspace: crates `core`, `backends` (x11/wlroots/kde/gnome-ext), `dbus-iface`, `store`, `daemon`, `show` (GTK4), `cli`, `common`.
- Define `ClipboardProvider` trait, `Entry`/`MimeBlob`/`Caps`/`ClipboardEvent` types.
- D-Bus name reservation + skeleton service; systemd user unit + `.desktop` autostart.
- *Validate:* `cargo test` green; daemon starts and claims the D-Bus name.

**P1 — X11 backend + store + CLI (first usable product)**
- `X11Backend` (subscribe/read_targets/offer/source_hint/auto-paste via XTEST).
- SQLite store + schema + migrations + FTS5 triggers + pruning.
- D-Bus methods: Query/Search/GetEntry/Pick/Delete/SetPinned/Clear/State.
- `cling-cli` (list/pick/copy/clear).
- *Validate:* capture→store→search→pick round-trip on X11; CLI E2E against a headless Xephyr.

**P2 — wlroots + KDE Wayland backends**
- `WlrootsBackend`, `KdeWaylandBackend` via data-control; capability-gated (no auto_paste, no source_hint).
- Backend auto-detection at startup.
- *Validate:* round-trip in Sway + KDE Plasma 6 (real or nested); confirm exclude-by-app degrades to content hints.

**P3 — UI (`cling-show`) + tray + search**
- GTK4 popup: keyboard-driven quick-pick, search field, image/file/rich previews, pin/edit/delete, multi-select.
- libadwaita on GNOME; light/dark theming.
- System tray (StatusNotifierItem): show/pause/preferences/quit.
- *Validate:* spawn via hotkey/custom-shortcut on each DE; a11y/keyboard nav checks.

**P4 — Crypto + exclude/pause + edit/groups**
- SQLCipher whole-DB; `RequestUnlock` daemon-side dialog; argon2id key; idle autolock; lock-buffering contract.
- Exclude-apps (X11/GNOME) + content/MIME-hint denylist (wlroots/KDE) + global pause.
- Inline edit; groups/tabs CRUD + filtering.
- *Validate:* DB unreadable without passphrase; lock/unlock round-trip; exclude/pause tests.

**P5 — Drag-drop-from-UI + multi-select bulk paste**
- DnD from popup into other apps (per-backend DnD handling; Wayland quirks).
- Multi-select → bulk paste (concatenate / sequence with configurable separator).
- *Validate:* DnD into Nautilus/Editor on X11 + wlroots; multi-paste ordering tests.

**P6 — GNOME Shell extension**
- Extension: hook selection, full-fidelity via `Meta.Selection` + text fallback, `AddEntry` over D-Bus, lock buffering.
- Per-version compat matrix; metadata for extensions.gnome.org.
- *Validate:* CI smoke test across supported GNOME shell versions; parity vs X11 for text+image+files.

**P7 — Packaging & CI**
- AppImage + `.deb` + AUR PKGBUILD; GNOME extension zip + e.g.o submission.
- GitHub Actions matrix (build + test + artifact upload); COPR/`.rpm` fast-follow.
- SQLCipher system-vs-bundled verification.
- *Validate:* fresh-VM install on Debian/Fedora/Arch; autostart works; extension installs from e.g.o.

---

## 9. Out of scope (explicitly deferred)

Cross-machine sync · plugin/scripting system · QR codes & action chains · scheduled paste · per-blob encryption · thumbnails in-DB · Flatpak packaging · native `.rpm`/COPR at v1 launch (fast-follow) · auto-paste on Wayland (impossible without ydotool) · exclude-by-app on wlroots/KDE (protocol-limited; content-hint fallback only).

---

## 10. Open follow-ups for implementer

- Confirm exact `Meta.Selection` private API surface per supported GNOME version while building P6 (matrix will capture findings).
- Decide idle-autolock default timeout and whether to offer "keep unlocked while on AC" (settings UX).
- Tray icon asset + app icon (`cling`) design.
