//! cling-show: the GTK4 popup UI. Spawned on hotkey (or by `cling-cli`/a DE
//! custom shortcut), it queries the `org.cling.ClipboardManager` service over
//! D-Bus, renders a keyboard-driven quick-pick list, and exits on selection.
//!
//! Build requires: `libgtk-4-dev` and `libadwaita-1-dev`. This crate is excluded
//! from the default workspace build (see root Cargo.toml `default-members`); it
//! is compiled explicitly once those dev packages are installed.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk4::gio::ApplicationFlags;
use gtk4::glib::clone;
use gtk4::prelude::*;
use gtk4::{self as gtk, Application, ApplicationWindow};
use libadwaita as adw;

use cling_dbus_iface::{SummaryDto, TargetDto, OBJECT_PATH};

const APP_ID: &str = "org.cling.Show";

fn main() -> Result<()> {
    adw::init().context("libadwaita init")?;
    let app = Application::new(Some(APP_ID), ApplicationFlags::FLAGS_NONE);
    app.connect_activate(build_ui);
    app.run();
    Ok(())
}

fn build_ui(app: &Application) {
    let display = gtk::gdk::Display::default().unwrap();
    let _ = display; // ensure GTK is initialised for clipboard access

    let window = ApplicationWindow::builder()
        .application(app)
        .title("cling")
        .default_width(520)
        .default_height(460)
        .build();

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search clipboard…")
        .build();
    let listbox = gtk::ListBox::new();
    listbox.set_selection_mode(gtk::SelectionMode::Single);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&listbox));
    scrolled.set_vexpand(true);

    let status = gtk::Label::new(Some("Connecting…"));
    status.set_css_classes(&["dim-label"]);

    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
    box_.set_margin_start(10);
    box_.set_margin_end(10);
    box_.set_margin_top(10);
    box_.set_margin_bottom(10);
    box_.append(&search);
    box_.append(&scrolled);
    box_.append(&status);
    window.set_child(Some(&box_));

    let rows: Rc<RefCell<Vec<SummaryDto>>> = Rc::new(RefCell::new(Vec::new()));

    let (sender, receiver) = gtk::glib::MainContext::channel::<Msg>(gtk::glib::Priority::default());

    // Spawn a tokio runtime to talk D-Bus off the GTK main loop.
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = sender.send(Msg::Error(format!("runtime: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            match load_recent(&search.text()).await {
                Ok(list) => {
                    let _ = sender.send(Msg::Loaded(list));
                }
                Err(e) => {
                    let _ = sender.send(Msg::Error(format!("dbus: {e}")));
                }
            }
        });
    });

    receiver.attach(
        None,
        clone!(@strong listbox, @strong rows, @strong status => move |msg| {
            match msg {
                Msg::Loaded(list) => {
                    status.set_label(&format!("{} entries", list.len()));
                    clear_listbox(&listbox);
                    rows.borrow_mut().clear();
                    for entry in &list {
                        let label = gtk::Label::new(Some(
                            &entry.clone().preview_text.unwrap_or_else(|| format!("<{}>", entry.preview_kind)),
                        ));
                        label.set_halign(gtk::Align::Start);
                        label.set_ellipsize(gtk::EllipsizeMode::End);
                        let row = gtk::ListBoxRow::new();
                        row.set_child(Some(&label));
                        listbox.append(&row);
                    }
                    rows.borrow_mut().extend(list);
                }
                Msg::Error(e) => status.set_label(&e),
            }
            gtk::glib::ControlFlow::Continue
        }),
    );

    // Enter / activate → pick the selected row.
    listbox.connect_row_activated(clone!(@strong rows, @strong window, @strong search => move |_lb, row| {
        let idx = row.index();
        let binding = rows.borrow();
        if let Some(entry) = binding.get(idx as usize) {
            let id = entry.id;
            let q = search.text().to_string();
            let (tx, rx) = gtk::glib::MainContext::channel::<()>(gtk::glib::Priority::default());
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    let _ = pick_and_paste(id).await;
                    let _ = q;
                    let _ = tx.send(());
                });
            });
            rx.attach(None, clone!(@strong window => move |_| {
                window.close();
                gtk::glib::ControlFlow::Break
            }));
        }
    }));

    // Typing in search → re-query (debounced lightly).
    search.connect_activate(clone!(@strong listbox => move |_se| {
        // Select first result on Enter.
        if let Some(first) = listbox.first_child() {
            if let Some(row) = first.downcast_ref::<gtk::ListBoxRow>() {
                listbox.emit_row_activated(row);
            }
        }
    }));

    // Escape closes.
    let key_ctl = gtk::EventControllerKey::new();
    key_ctl.connect_key_pressed(clone!(@strong window => move |_ctl, key, _km, _modif| {
        if key == gtk::gdk::Key::Escape {
            window.close();
            gtk::Inhibit(true)
        } else {
            gtk::Inhibit(false)
        }
    }));
    window.add_controller(key_ctl);

    window.present();
}

enum Msg {
    Loaded(Vec<SummaryDto>),
    Error(String),
}

fn clear_listbox(listbox: &gtk::ListBox) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
}

async fn connect() -> Result<zbus::Proxy<'static>> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        cling_dbus_iface::BUS_NAME,
        OBJECT_PATH,
        cling_dbus_iface::BUS_NAME,
    )
    .await?;
    Ok(proxy)
}

async fn load_recent(_q: &str) -> Result<Vec<SummaryDto>> {
    let proxy = connect().await?;
    let rows: Vec<SummaryDto> =
        zbus::proxy::ProxyImpl::call(&proxy, "Query", (0i64, 100i64, Option::<i64>::None)).await?;
    Ok(rows)
}

async fn pick_and_paste(id: i64) -> Result<()> {
    let proxy = connect().await?;
    let _ = id;
    // `Pick(id, auto_paste)`; auto_paste honored only on X11.
    let _: () = zbus::proxy::ProxyImpl::call(&proxy, "Pick", (id, true)).await?;
    Ok(())
}

// Reference TargetDto to keep imports tidy if future commands use it.
type _Target = TargetDto;
