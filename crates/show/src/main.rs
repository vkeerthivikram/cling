// glib 0.20 deprecates the `clone!` macro (pending a replacement); it still
// works and remains the idiomatic GTK-Rust pattern, so we allow it here.
#![allow(deprecated)]

//! cling-show — the GTK4/libadwaita popup UI.
//!
//! Modes:
//!   * default (popup): keyboard-driven quick-pick of clipboard history over
//!     the `org.cling.ClipboardManager` D-Bus service.
//!   * `--socket <path>` (unlock): passphrase dialog writing to a private Unix
//!     socket the daemon listens on (passphrase never crosses the D-Bus bus).
//!
//! P5: per-row drag-out (text content provider) + multi-select bulk paste
//! (concatenate selected entries' text onto the clipboard).
//!
//! D-Bus uses the blocking API; each call opens a short-lived session
//! connection (cheap; avoids self-referential proxy lifetimes and channels).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use anyhow::{Context, Result};
use clap::Parser;
use gtk4::gdk::Display;
use gtk4::gio::ApplicationFlags;
use gtk4::glib::clone;
use gtk4::glib::Propagation;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    self as gtk, Application, ApplicationWindow, Box as GtkBox, Button, DragSource, Label, ListBox,
    ListBoxRow, Orientation, PasswordEntry, SearchEntry, SelectionMode, Separator, ToggleButton,
};
use gtk4::{gdk, glib};
use libadwaita as adw;

use cling_dbus_iface::{EntryDto, SummaryDto, OBJECT_PATH};

const APP_ID: &str = "org.cling.Show";

#[derive(Parser, Debug)]
#[command(
    name = "cling-show",
    version,
    about = "cling clipboard popup / unlock dialog"
)]
struct Cli {
    /// Unlock mode: write the entered passphrase to this Unix socket path.
    #[arg(long)]
    socket: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(socket) = cli.socket.clone() {
        let app = Application::new(
            Some(&format!("{APP_ID}.Unlock")),
            ApplicationFlags::FLAGS_NONE,
        );
        app.connect_activate(move |a| build_unlock(a, socket.clone()));
        app.run();
        return Ok(());
    }

    adw::init().context("libadwaita init")?;
    let app = Application::new(Some(APP_ID), ApplicationFlags::FLAGS_NONE);
    app.connect_activate(build_popup);
    app.run();
    Ok(())
}

/// Run `f` against a freshly-built blocking D-Bus proxy for the service.
fn db<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&zbus::blocking::Proxy<'_>) -> zbus::Result<R>,
{
    let conn = zbus::blocking::Connection::session().context("session bus")?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        cling_dbus_iface::BUS_NAME,
        OBJECT_PATH,
        cling_dbus_iface::BUS_NAME,
    )?;
    Ok(f(&proxy)?)
}

// ============================ popup UI =====================================

fn build_popup(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("cling")
        .default_width(560)
        .default_height(480)
        .build();

    let search = SearchEntry::builder()
        .placeholder_text("Search clipboard…")
        .build();
    let listbox = ListBox::new();
    listbox.set_selection_mode(SelectionMode::Single);
    listbox.set_activate_on_single_click(true);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&listbox));
    scrolled.set_vexpand(true);

    let status = Label::new(None);
    status.set_css_classes(&["dim-label", "caption"]);
    status.set_halign(gtk::Align::Start);

    let multi_btn = ToggleButton::builder().label("Multi").build();
    let paste_all_btn = Button::with_label("Paste selected");
    paste_all_btn.set_sensitive(false);
    let pin_btn = Button::with_label("Pin");
    let delete_btn = Button::with_label("Delete");
    let toolbar = GtkBox::new(Orientation::Horizontal, 6);
    toolbar.append(&multi_btn);
    toolbar.append(&paste_all_btn);
    toolbar.append(&Separator::new(Orientation::Vertical));
    toolbar.append(&pin_btn);
    toolbar.append(&delete_btn);

    let vbox = GtkBox::new(Orientation::Vertical, 8);
    vbox.set_margin_start(10);
    vbox.set_margin_end(10);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    vbox.append(&search);
    vbox.append(&scrolled);
    vbox.append(&toolbar);
    vbox.append(&status);
    window.set_child(Some(&vbox));

    // Parallel store: row index i ↔ shown_entries[i].
    let entries: Rc<RefCell<Vec<SummaryDto>>> = Rc::new(RefCell::new(Vec::new()));

    let refresh = clone!(@strong search, @strong listbox, @strong entries, @strong status => move || {
        let q = search.text().to_string();
        let result: Result<Vec<SummaryDto>> = db(|p| {
            if q.trim().is_empty() {
                p.call::<_, _, Vec<SummaryDto>>("Query", &(0i64, 200i64, Option::<i64>::None))
            } else {
                p.call::<_, _, Vec<SummaryDto>>("Search", &(q.as_str(), 200i64))
            }
        });
        match result {
            Ok(rows) => {
                status.set_label(&format!("{} entries", rows.len()));
                clear_listbox(&listbox);
                entries.borrow_mut().clear();
                for entry in &rows {
                    listbox.append(&make_row(entry));
                }
                entries.borrow_mut().extend(rows);
            }
            Err(e) => status.set_label(&format!("query failed: {e}")),
        }
    });

    refresh();
    search.connect_search_changed(clone!(@strong refresh => move |_| refresh()));
    search.connect_activate(clone!(@strong listbox => move |_| {
        if let Some(first) = listbox.first_child().and_then(|c| c.downcast::<ListBoxRow>().ok()) {
            first.activate();
        }
    }));

    // Pick on row activation.
    let entries_for_pick = entries.clone();
    listbox.connect_row_activated(
        clone!(@strong window, @strong entries_for_pick => move |_lb, row| {
            let id = {
                let b = entries_for_pick.borrow();
                b.get(row.index() as usize).map(|e| e.id)
            };
            if let Some(id) = id {
                let _: Result<()> = db(|p| p.call::<_, _, ()>("Pick", &(id, true)));
                window.close();
            }
        }),
    );

    // Multi-select toggle (P5).
    multi_btn.connect_toggled(
        clone!(@strong listbox, @strong paste_all_btn => move |btn| {
            listbox.set_selection_mode(if btn.is_active() {
                SelectionMode::Multiple
            } else {
                SelectionMode::Single
            });
            paste_all_btn.set_sensitive(btn.is_active());
        }),
    );

    // Bulk paste (P5): concatenate selected text/plain onto the clipboard.
    let entries_for_bulk = entries.clone();
    paste_all_btn.connect_clicked(
        clone!(@strong listbox, @strong entries_for_bulk, @strong status => move |_| {
            let mut texts: Vec<String> = Vec::new();
            for row in listbox.selected_rows() {
                let id = entries_for_bulk.borrow().get(row.index() as usize).map(|e| e.id);
                if let Some(id) = id {
                    if let Ok(Some(entry)) = db(|p| p.call::<_, _, Option<EntryDto>>("GetEntry", &(id,))) {
                        if let Some(t) = entry_text(&entry) { texts.push(t); }
                    }
                }
            }
            if texts.is_empty() { return; }
            let joined = texts.join("\n");
            if let Some(d) = Display::default() {
                d.clipboard().set_text(&joined);
                status.set_label(&format!("Pasted {} items", texts.len()));
            }
        }),
    );

    // Pin (single selection).
    let entries_for_pin = entries.clone();
    pin_btn.connect_clicked(
        clone!(@strong listbox, @strong status, @strong entries_for_pin => move |_| {
            let (id, pinned) = match listbox.selected_row() {
                Some(row) => {
                    let b = entries_for_pin.borrow();
                    match b.get(row.index() as usize) {
                        Some(e) => (e.id, e.pinned),
                        None => return,
                    }
                }
                None => return,
            };
            let _: Result<()> = db(|p| p.call::<_, _, ()>("SetPinned", &(id, !pinned)));
            status.set_label(if pinned { "Unpinned" } else { "Pinned" });
        }),
    );

    // Delete (single or multiple).
    let entries_for_del = entries.clone();
    delete_btn.connect_clicked(
        clone!(@strong listbox, @strong status, @strong entries_for_del => move |_| {
            let mut ids: Vec<i64> = Vec::new();
            for row in listbox.selected_rows() {
                if let Some(e) = entries_for_del.borrow().get(row.index() as usize) {
                    ids.push(e.id);
                }
            }
            if ids.is_empty() { return; }
            let n = ids.len();
            let _: Result<()> = db(|p| p.call::<_, _, ()>("Delete", &(ids,)));
            status.set_label(&format!("Deleted {n}"));
        }),
    );

    let key_ctl = gtk::EventControllerKey::new();
    key_ctl.connect_key_pressed(clone!(@strong window => move |_ctl, key, _km, _mods| {
        if key == gtk4::gdk::Key::Escape {
            window.close();
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    }));
    window.add_controller(key_ctl);

    window.present();
}

/// Build one list row from a summary, with a drag-out source (P5).
fn make_row(entry: &SummaryDto) -> ListBoxRow {
    let label = Label::new(Some(
        &entry
            .preview_text
            .clone()
            .unwrap_or_else(|| format!("<{}>", entry.preview_kind)),
    ));
    label.set_halign(gtk::Align::Start);
    label.set_ellipsize(EllipsizeMode::End);
    label.set_xalign(0.0);

    let row = ListBoxRow::new();
    row.set_child(Some(&label));

    let drag = DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);
    let id_for_drag = entry.id;
    drag.connect_prepare(
        clone!(@strong id_for_drag => @default-return None, move |_drag, _x, _y| {
            if let Ok(Some(entry)) = db(|p| p.call::<_, _, Option<EntryDto>>("GetEntry", &(id_for_drag,))) {
                if let Some(text) = entry_text(&entry) {
                    let bytes = glib::Bytes::from(text.as_bytes());
                    return Some(gdk::ContentProvider::for_bytes(
                        "text/plain;charset=utf-8",
                        &bytes,
                    ));
                }
            }
            None
        }),
    );
    label.add_controller(drag);
    row
}

fn entry_text(entry: &EntryDto) -> Option<String> {
    entry
        .targets
        .iter()
        .find(|t| {
            t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
                || t.mime.eq_ignore_ascii_case("text/plain")
        })
        .and_then(|t| String::from_utf8(t.bytes.clone()).ok())
}

fn clear_listbox(listbox: &ListBox) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
}

// ============================ unlock dialog ================================

fn build_unlock(app: &Application, socket: String) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Unlock cling history")
        .default_width(360)
        .resizable(false)
        .build();

    let entry = PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("Passphrase")
        .build();
    let label = Label::new(Some("Enter passphrase to unlock clipboard history:"));
    label.set_halign(gtk::Align::Start);
    let err = Label::new(None);
    err.set_css_classes(&["error"]);

    let vbox = GtkBox::new(Orientation::Vertical, 10);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.append(&label);
    vbox.append(&entry);
    vbox.append(&err);
    window.set_child(Some(&vbox));

    let app_clone = app.clone();
    let err_clone = err.clone();
    let entry_clone = entry.clone();
    let socket_clone = socket.clone();
    entry.connect_activate(move |_| {
        let passphrase = entry_clone.text().to_string();
        if passphrase.is_empty() {
            err_clone.set_label("Passphrase is empty");
            return;
        }
        match write_passphrase(&socket_clone, &passphrase) {
            Ok(()) => app_clone.quit(),
            Err(e) => err_clone.set_label(&format!("Failed to send: {e}")),
        }
    });

    let app_esc = app.clone();
    let key_ctl = gtk::EventControllerKey::new();
    key_ctl.connect_key_pressed(move |_ctl, key, _km, _mods| {
        if key == gtk4::gdk::Key::Escape {
            app_esc.quit();
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    window.add_controller(key_ctl);
    window.present();
}

fn write_passphrase(socket: &str, passphrase: &str) -> Result<()> {
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(socket).context("connect unlock socket")?;
    s.write_all(passphrase.as_bytes())
        .context("write passphrase")?;
    s.flush().ok();
    Ok(())
}
