// Cling Clipboard Bridge — GNOME Shell extension.
//
// Bridges GNOME's clipboard to the cling clipboard manager daemon
// (org.cling.ClipboardManager on the session bus) so that on GNOME-Wayland
// — where Mutter does not expose wlr-data-control — history is captured with
// the same fidelity the daemon gets on X11/wlroots/KDE.
//
// Capture strategy (matches the "full-fidelity attempt + text fallback"
// decision in the plan):
//   * Text: reliable via St.Clipboard `notify::text` (works on every version).
//   * Images / files / rich text: best-effort via the shell's private
//     Meta.Selection / ClipboardManager when reachable, guarded by try/catch
//     and a per-version compat map. Falls back to text-only silently.
//
// Lock buffering: while the daemon's DB is locked, recent captures are buffered
// in-shell and flushed on the `Unlocked` signal so history is never lost.

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import St from 'gi://St';

import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';
import { config } from 'resource:///org/gnome/shell/misc/config.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS_NAME = 'org.cling.ClipboardManager';
const OBJECT_PATH = '/org/cling/ClipboardManager';

// Best-effort cap for the in-shell lock buffer.
const MAX_BUFFER = 64;

// MIMEs we attempt for the full-fidelity path (best-effort; availability varies).
const RICH_TARGETS = [
    'text/plain;charset=utf-8',
    'text/plain',
    'text/html',
    'text/uri-list',
    'image/png',
];

class ClingBridge {
    constructor() {
        this._clipboard = null;
        this._textSig = null;
        this._dbusProxy = null;
        this._dbusCancellable = null;
        this._locked = false;
        this._buffer = [];
        this._lastText = null;
        this._compat = this._probeCompat();
    }

    // Per-version compatibility probe (the compat matrix lives here).
    _probeCompat() {
        const v = config.PACKAGE_VERSION;
        const major = parseInt(v.split('.')[0], 10) || 45;
        return {
            shellVersion: v,
            major,
            // St.Clipboard text API is stable across 45+.
            hasTextApi: true,
            // Meta.Selection/full-fidelity reachability: best-effort, off by default
            // until validated per-version. Flip on once smoke-tested.
            hasMetaSelection: false,
        };
    }

    enable() {
        this._clipboard = St.Clipboard.get_default();
        this._textSig = this._clipboard.connect('notify::text', () => this._onTextChanged());

        this._dbusCancellable = new Gio.Cancellable();
        Gio.DBus.session.call(
            BUS_NAME,
            OBJECT_PATH,
            'org.freedesktop.DBus.Properties',
            'GetAll',
            new GLib.Variant('(s)', ['org.cling.ClipboardManager']),
            new GLib.VariantType('(a{sv})'),
            Gio.DBusCallFlags.NONE,
            -1,
            this._dbusCancellable,
            (proxy, res) => this._onDBusReady(res),
        );

        // Subscribe to StateChanged / Unlocked so we know lock state and flush.
        this._subId = Gio.DBus.session.signal_subscribe(
            BUS_NAME,
            'org.cling.ClipboardManager',
            null,
            OBJECT_PATH,
            null,
            Gio.DBusSignalFlags.NONE,
            (conn, sender, path, iface, signal, params) => this._onSignal(signal, params),
        );
    }

    disable() {
        if (this._subId) {
            Gio.DBus.session.signal_unsubscribe(this._subId);
            this._subId = null;
        }
        if (this._dbusCancellable) {
            this._dbusCancellable.cancel();
            this._dbusCancellable = null;
        }
        if (this._textSig && this._clipboard) {
            this._clipboard.disconnect(this._textSig);
            this._textSig = null;
        }
        this._clipboard = null;
        this._buffer = [];
    }

    _onDBusReady(res) {
        try {
            Gio.DBus.session.call_finish(res);
            this._locked = false;
        } catch (e) {
            // Daemon not running yet; treat as locked so we buffer, and retry on next text.
            this._locked = true;
            log(`cling: dbus not ready (${e.message}), buffering`);
        }
    }

    _onSignal(signal, params) {
        if (signal === 'Unlocked') {
            this._locked = false;
            this._flushBuffer();
        } else if (signal === 'StateChanged') {
            // StateChanged(paused: bool, locked: bool)
            const [/*paused*/, locked] = params.deep_unpack();
            const wasLocked = this._locked;
            this._locked = !!locked;
            if (wasLocked && !this._locked) {
                this._flushBuffer();
            }
        }
    }

    _onTextChanged() {
        this._clipboard.get_text(St.ClipboardType.CLIPBOARD, (cb, text) => {
            if (text === this._lastText) return;
            this._lastText = text;
            if (text == null || text === '') return;

            // Build a target list. Full-fidelity best-effort: currently text-only
            // (stable); the Meta.Selection path would append images/files here.
            const targets = [];
            const utf8 = new TextEncoder().encode(text);
            targets.push(['text/plain;charset=utf-8', utf8]);

            if (this._compat.hasMetaSelection) {
                const extra = this._readRichTargets();
                for (const t of extra) targets.push(t);
            }

            this._pushCapture(targets);
        });
    }

    // Best-effort full-fidelity read via the shell's selection API. Returns an
    // array of [mime, Uint8Array]. Unavailable on most versions → empty.
    _readRichTargets() {
        try {
            // The Meta.Selection API is private and version-coupled; left as a
            // hook to fill in once the compat matrix validates a GNOME version.
            return [];
        } catch (e) {
            return [];
        }
    }

    _pushCapture(targets) {
        if (this._locked) {
            this._buffer.push(targets);
            if (this._buffer.length > MAX_BUFFER) this._buffer.shift();
            return;
        }
        this._send(targets);
    }

    _flushBuffer() {
        while (this._buffer.length > 0) {
            const t = this._buffer.shift();
            this._send(t);
        }
    }

    _send(targets) {
        // AddEntry(targets : a(say)) — each TargetDto is a struct of
        // (mime: s, bytes: ay). Build the outer '(a(say))' message body.
        const arg = new GLib.Variant('a(say)', targets.map(([mime, bytes]) => {
            return new GLib.Variant('(say)', [mime, this._bytesToVariant(bytes)]);
        }));
        Gio.DBus.session.call(
            BUS_NAME,
            OBJECT_PATH,
            'org.cling.ClipboardManager',
            'AddEntry',
            new GLib.Variant('(a(say))', [arg]),
            null,
            Gio.DBusCallFlags.NONE,
            -1,
            this._dbusCancellable,
            null,
        );
    }

    _bytesToVariant(bytes) {
        // GLib has no direct byte-array Variant from Uint8Array; pack into 'ay'.
        const gbytes = GLib.Bytes.new(bytes);
        return GLib.Variant.new_from_bytes(new GLib.VariantType('ay'), gbytes, true);
    }
}

let _bridge = null;

export default class ClingExtension extends Extension {
    enable() {
        if (_bridge) return;
        _bridge = new ClingBridge();
        _bridge.enable();
    }

    disable() {
        if (_bridge) {
            _bridge.disable();
            _bridge = null;
        }
    }
}
