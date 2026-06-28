//! The X11 event-loop thread: XFIXES selection monitoring, selection reading
//! (TARGETS + per-target data with INCR support), selection ownership for
//! copy-from-history, source-app identity, and XTEST auto-paste.
//!
//! The connection lives on this thread. Commands arrive on `cmd_rx` (drained
//! each loop iteration); clipboard events are pushed to `evt_tx`. When idle the
//! loop sleeps briefly so command latency stays ~5ms.
//!
//! Note: this needs a real X server (or Xephyr) to exercise at runtime; covered
//! by integration tests where a display is available.

use std::collections::HashMap;
use std::sync::mpsc::Receiver as StdReceiver;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc::UnboundedSender, Mutex};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::{Cache, XCmd};
use cling_common::{AppId, ClipboardEvent, MimeBlob};
use cling_core::BackendError;

const POLL_IDLE: Duration = Duration::from_millis(5);
const SELECTION_TIMEOUT: Duration = Duration::from_millis(500);

// X core event type numbers.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

/// Entry point for the X11 thread.
pub fn run_loop(
    conn: RustConnection,
    screen_num: usize,
    cache: std::sync::Arc<Mutex<Cache>>,
    evt_tx: UnboundedSender<ClipboardEvent>,
    cmd_rx: StdReceiver<XCmd>,
) {
    if let Err(e) = run_loop_inner(&conn, screen_num, &cache, &evt_tx, &cmd_rx) {
        tracing::error!(error = %e, "x11 loop exited with error");
    }
}

struct Ctx {
    win: Atom,
    clipboard: Atom,
    primary: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    text_plain: Atom,
    /// When we own the selection: mime-atom → bytes to serve.
    offered: HashMap<Atom, Vec<u8>>,
    /// When we own the selection: the list of target atoms (for TARGETS reply).
    offered_targets: Vec<Atom>,
    offered_time: xproto::Timestamp,
}

fn run_loop_inner(
    conn: &RustConnection,
    screen_num: usize,
    cache: &std::sync::Arc<Mutex<Cache>>,
    evt_tx: &UnboundedSender<ClipboardEvent>,
    cmd_rx: &StdReceiver<XCmd>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let xfixes_first_event = conn
        .extension_information("XFIXES")?
        .map(|i| i.first_event)
        .unwrap_or(0);
    if xfixes_first_event == 0 {
        tracing::error!("X server has no XFIXES extension; X11 backend unusable");
        return Ok(());
    }
    // Initialise XFIXES (negotiate version so the server enables the extension).
    let _ = conn.xfixes_query_version(5, 0)?.reply()?;

    let screen = conn.setup().roots[screen_num].clone();
    let win = conn.generate_id()?;
    conn.create_window(
        0, // depth 0 = copy from parent
        win,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0, // visual 0 = copy from parent
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    let clipboard = intern(conn, "CLIPBOARD")?;
    let primary = u32::from(AtomEnum::PRIMARY);
    let targets = intern(conn, "TARGETS")?;
    let timestamp = intern(conn, "TIMESTAMP")?;
    let incr = intern(conn, "INCR")?;
    let text_plain = intern(conn, "text/plain;charset=utf-8")?;

    let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
        | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
        | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
    conn.xfixes_select_selection_input(win, clipboard, mask)?;
    conn.xfixes_select_selection_input(win, primary, mask)?;
    conn.flush()?;

    let mut ctx = Ctx {
        win,
        clipboard,
        primary,
        targets,
        timestamp,
        incr,
        text_plain,
        offered: HashMap::new(),
        offered_targets: Vec::new(),
        offered_time: x11rb::CURRENT_TIME,
    };

    loop {
        let mut work = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            work = true;
            handle_cmd(conn, &mut ctx, cmd);
        }
        while let Some(ev) = conn.poll_for_event()? {
            work = true;
            handle_event(conn, &mut ctx, cache, evt_tx, ev);
        }
        let _ = conn.flush();
        if !work {
            std::thread::sleep(POLL_IDLE);
        }
    }
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, ReplyError> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn now_millis() -> xproto::Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

fn handle_cmd(conn: &RustConnection, ctx: &mut Ctx, cmd: XCmd) {
    let to_backend = |e: ReplyError| BackendError::Protocol(e.to_string());
    match cmd {
        XCmd::Offer(targets, reply) => {
            let _ = reply.send(offer(conn, ctx, targets).map_err(to_backend));
        }
        XCmd::ReadTargets(reply) => {
            let _ = reply.send(read_selection_all(conn, ctx, ctx.clipboard).map_err(to_backend));
        }
        XCmd::SourceHint(reply) => {
            let _ = reply.send(source_of(conn, ctx, ctx.clipboard));
        }
        XCmd::AutoPaste(reply) => {
            let _ = reply.send(auto_paste(conn).map_err(to_backend));
        }
        XCmd::Shutdown => {
            tracing::info!("x11 shutdown requested");
        }
    }
}

fn handle_event(
    conn: &RustConnection,
    ctx: &mut Ctx,
    cache: &std::sync::Arc<Mutex<Cache>>,
    evt_tx: &UnboundedSender<ClipboardEvent>,
    ev: Event,
) {
    match ev {
        Event::XfixesSelectionNotify(e) if e.selection == ctx.clipboard => {
            // A new selection owner appeared on CLIPBOARD — capture it.
            let targets = match read_selection_all(conn, ctx, ctx.clipboard) {
                Ok(t) if !t.is_empty() => t,
                _ => return,
            };
            let source = source_of(conn, ctx, ctx.clipboard);
            if let Ok(mut g) = cache.try_lock() {
                g.targets = Some(targets);
                g.source = source.clone();
            }
            let _ = evt_tx.send(ClipboardEvent::SelectionChanged { source });
        }
        Event::SelectionRequest(req) => {
            serve_selection_request(conn, ctx, &req);
        }
        Event::SelectionClear(_) => {
            // We lost selection ownership.
            ctx.offered.clear();
            ctx.offered_targets.clear();
        }
        _ => {}
    }
}

/// Read all offered targets + their data for `selection`.
fn read_selection_all(
    conn: &RustConnection,
    ctx: &mut Ctx,
    selection: Atom,
) -> Result<Vec<MimeBlob>, ReplyError> {
    let target_atoms = match read_selection(conn, ctx, selection, ctx.targets) {
        Ok(Some(bytes)) => atoms_from_bytes(&bytes),
        _ => Vec::new(),
    };
    if target_atoms.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(target_atoms.len());
    for atom in target_atoms {
        if atom == ctx.targets || atom == ctx.timestamp || atom == ctx.incr {
            continue;
        }
        if let Ok(Some(bytes)) = read_selection(conn, ctx, selection, atom) {
            let label = atom_name(conn, atom).unwrap_or_else(|_| format!("atom:{atom}"));
            out.push(MimeBlob { mime: label, bytes });
        }
    }
    Ok(out)
}

/// Convert a selection target into our window's property and read the bytes.
fn read_selection(
    conn: &RustConnection,
    ctx: &Ctx,
    selection: Atom,
    target: Atom,
) -> Result<Option<Vec<u8>>, ReplyError> {
    let prop = intern(conn, "CLING_SELECTION")?;
    conn.convert_selection(ctx.win, selection, target, prop, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    let deadline = Instant::now() + SELECTION_TIMEOUT;
    while Instant::now() < deadline {
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::SelectionNotify(sne) = ev {
                if sne.requestor == ctx.win && sne.selection == selection && sne.target == target {
                    if sne.property == 0 || sne.property == u32::from(AtomEnum::NONE) {
                        return Ok(None);
                    }
                    return read_property_maybe_incr(conn, ctx, prop);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(None)
}

fn read_property_maybe_incr(
    conn: &RustConnection,
    ctx: &Ctx,
    prop: Atom,
) -> Result<Option<Vec<u8>>, ReplyError> {
    let first = conn
        .get_property(true, ctx.win, prop, 0u32, 0, 8192)?
        .reply()?;
    conn.delete_property(ctx.win, prop)?;
    if first.type_ == ctx.incr {
        let mut buf = Vec::new();
        let deadline = Instant::now() + SELECTION_TIMEOUT;
        loop {
            // Wait for a PropertyNotify (NewValue) on our window for `prop`.
            while Instant::now() < deadline {
                if let Some(ev) = conn.poll_for_event()? {
                    if let Event::PropertyNotify(pne) = ev {
                        if pne.window == ctx.win && pne.atom == prop {
                            let chunk = conn
                                .get_property(true, ctx.win, prop, 0u32, 0, 8192)?
                                .reply()?;
                            conn.delete_property(ctx.win, prop)?;
                            buf.extend_from_slice(&chunk.value);
                            if chunk.value_len == 0 {
                                return Ok(Some(buf));
                            }
                            break;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            if Instant::now() >= deadline {
                return Ok(Some(buf));
            }
        }
    } else {
        Ok(Some(first.value))
    }
}

fn atoms_from_bytes(bytes: &[u8]) -> Vec<Atom> {
    bytes
        .chunks_exact(4)
        .filter_map(|c| Some(u32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
        .collect()
}

fn atom_name(conn: &RustConnection, atom: Atom) -> Result<String, ReplyError> {
    Ok(String::from_utf8_lossy(&conn.get_atom_name(atom)?.reply()?.name).into_owned())
}

/// Best-effort source-app identity of the current selection owner.
fn source_of(conn: &RustConnection, ctx: &Ctx, selection: Atom) -> AppId {
    let owner = match conn
        .get_selection_owner(selection)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        Some(r) => r.owner,
        None => return AppId::default(),
    };
    if owner == 0 || owner == ctx.win {
        return AppId {
            id: Some("cling".into()),
            label: Some("cling".into()),
        };
    }
    let label = wm_class_of(conn, owner).unwrap_or_else(|| format!("window:{owner}"));
    AppId {
        id: Some(label.to_ascii_lowercase()),
        label: Some(label),
    }
}

fn wm_class_of(conn: &RustConnection, win: Atom) -> Option<String> {
    let prop = conn
        .get_property(false, win, AtomEnum::WM_CLASS, 0u32, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    let s = String::from_utf8_lossy(&prop.value);
    let parts: Vec<&str> = s.split('\0').filter(|p| !p.is_empty()).collect();
    parts
        .get(1)
        .or_else(|| parts.first())
        .map(|s| s.to_string())
}

/// Take ownership of the CLIPBOARD selection and remember targets to serve.
fn offer(conn: &RustConnection, ctx: &mut Ctx, targets: Vec<MimeBlob>) -> Result<(), ReplyError> {
    ctx.offered.clear();
    ctx.offered_targets.clear();
    ctx.offered_targets.push(ctx.targets);
    ctx.offered_targets.push(ctx.timestamp);
    for t in &targets {
        let atom = intern(conn, &t.mime).unwrap_or(ctx.text_plain);
        ctx.offered.insert(atom, t.bytes.clone());
        ctx.offered_targets.push(atom);
    }
    ctx.offered_time = now_millis();
    conn.set_selection_owner(ctx.win, ctx.clipboard, x11rb::CURRENT_TIME)?;
    conn.flush()?;
    Ok(())
}

/// Answer a SelectionRequest when we own the selection.
fn serve_selection_request(conn: &RustConnection, ctx: &Ctx, req: &SelectionRequestEvent) {
    if req.selection != ctx.clipboard && req.selection != ctx.primary {
        return;
    }
    let prop = if req.property == 0 {
        req.target
    } else {
        req.property
    };

    let ok = if req.target == ctx.targets {
        // Reply with the list of offered target atoms (format 32).
        let mut data = Vec::with_capacity(ctx.offered_targets.len() * 4);
        for a in &ctx.offered_targets {
            data.extend_from_slice(&a.to_ne_bytes());
        }
        let n = ctx.offered_targets.len() as u32;
        conn.change_property(
            PropMode::REPLACE,
            req.requestor,
            prop,
            ctx.targets,
            32,
            n,
            &data,
        )
        .is_ok()
    } else if req.target == ctx.timestamp {
        conn.change_property(
            PropMode::REPLACE,
            req.requestor,
            prop,
            ctx.timestamp,
            32,
            1,
            &ctx.offered_time.to_ne_bytes(),
        )
        .is_ok()
    } else {
        match ctx.offered.get(&req.target) {
            Some(bytes) => conn
                .change_property(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    req.target,
                    8,
                    bytes.len() as u32,
                    bytes,
                )
                .is_ok(),
            None => false,
        }
    };

    let notify = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: if ok { prop } else { 0 },
    };
    let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    let _ = conn.flush();
}

/// Synthesize Ctrl+V via XTEST.
fn auto_paste(conn: &RustConnection) -> Result<(), ReplyError> {
    const CTRL_L: u8 = 37; // X keycode for Control_L
    const V: u8 = 55; // X keycode for 'v' on a typical layout
                      // deviceid 0x80 (keyboard) on the extended XTEST core.
    conn.xtest_fake_input(KEY_PRESS, CTRL_L, x11rb::CURRENT_TIME, 0, 0, 0, 0x80)?;
    conn.xtest_fake_input(KEY_PRESS, V, x11rb::CURRENT_TIME, 0, 0, 0, 0x80)?;
    conn.xtest_fake_input(KEY_RELEASE, V, x11rb::CURRENT_TIME, 0, 0, 0, 0x80)?;
    conn.xtest_fake_input(KEY_RELEASE, CTRL_L, x11rb::CURRENT_TIME, 0, 0, 0, 0x80)?;
    conn.flush()?;
    Ok(())
}

// ---- keep selected public symbols referenced for API stability ----
#[allow(dead_code)]
fn _keep(_a: ClientMessageEvent, _b: SelectionNotifyEvent, _g: PropMode, _e: ConnectionError) {}
