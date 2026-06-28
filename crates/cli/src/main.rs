//! cling-cli: scriptable front-end over the `org.cling.ClipboardManager`
//! D-Bus service. Pipe-friendly for keybindings and automation.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cling_dbus_iface::{EntryDto, SummaryDto, TargetDto, OBJECT_PATH};
use zvariant::Type;

#[derive(Parser, Debug)]
#[command(name = "cling-cli", version, about = "cling clipboard manager client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List recent entries (id | kind | use | preview).
    List {
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Full-text search the history.
    Search {
        query: String,
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Copy entry N back to the clipboard.
    Pick {
        id: i64,
        #[arg(long)]
        auto_paste: bool,
    },
    /// Add the contents of stdin as a new entry.
    Add {
        #[arg(long, default_value = "text/plain;charset=utf-8")]
        mime: String,
    },
    /// Delete one or more entries by id.
    Delete { ids: Vec<i64> },
    /// Pin / unpin an entry.
    Pin {
        id: i64,
        #[arg(long)]
        unpin: bool,
    },
    /// Clear all history.
    Clear,
    /// Pause / resume capture.
    Pause {
        #[arg(long)]
        off: bool,
    },
    /// Print daemon state.
    State,
    /// Print the text/plain target of an entry (for piping / inspection).
    Get { id: i64 },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let proxy = connect().await?;
    match cli.cmd {
        Cmd::List { limit } => {
            let rows: Vec<SummaryDto> = proxy
                .call("Query", &(0i64, limit, Option::<i64>::None))
                .await?;
            print_summary(&rows);
        }
        Cmd::Search { query, limit } => {
            let rows: Vec<SummaryDto> = proxy.call("Search", &(&query, limit)).await?;
            print_summary(&rows);
        }
        Cmd::Pick { id, auto_paste } => {
            let _: () = proxy.call("Pick", &(id, auto_paste)).await?;
        }
        Cmd::Add { mime } => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            let target = TargetDto { mime, bytes: buf };
            let _id: i64 = proxy.call("AddEntry", &(vec![target],)).await?;
        }
        Cmd::Delete { ids } => {
            let _: () = proxy.call("Delete", &(ids,)).await?;
        }
        Cmd::Pin { id, unpin } => {
            let _: () = proxy.call("SetPinned", &(id, !unpin)).await?;
        }
        Cmd::Clear => {
            let _: () = proxy.call("Clear", &()).await?;
        }
        Cmd::Pause { off } => {
            let _: () = proxy.call("Pause", &(!off)).await?;
        }
        Cmd::State => {
            let st: cling_dbus_iface::StateDto = proxy.call("State", &()).await?;
            println!("{}", serde_json::to_string_pretty(&st)?);
        }
        Cmd::Get { id } => {
            let entry: Option<EntryDto> = proxy.call("GetEntry", &(id,)).await?;
            match entry {
                None => bail!("no entry {id}"),
                Some(e) => {
                    for t in &e.targets {
                        if t.mime.eq_ignore_ascii_case("text/plain")
                            || t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
                        {
                            std::io::stdout().write_all(&t.bytes)?;
                            return Ok(());
                        }
                    }
                    for t in &e.targets {
                        println!("{} ({} bytes)", t.mime, t.bytes.len());
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_summary(rows: &[SummaryDto]) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{:<8} {:<8} {:<5} PREVIEW", "ID", "KIND", "USE");
    for r in rows {
        let preview = r.preview_text.clone().unwrap_or_default();
        let preview = preview.replace(['\n', '\r'], " ");
        let _ = writeln!(
            out,
            "{:<8} {:<8} {:<5} {}",
            r.id, r.preview_kind, r.use_count, preview
        );
    }
}

async fn connect() -> Result<zbus::Proxy<'static>> {
    let conn = zbus::Connection::session()
        .await
        .context("connect to session bus")?;
    let proxy = zbus::Proxy::new(
        &conn,
        cling_dbus_iface::BUS_NAME,
        OBJECT_PATH,
        cling_dbus_iface::BUS_NAME,
    )
    .await?;
    Ok(proxy)
}

// Keep the `Type` import used in any future generic helpers.
#[allow(dead_code)]
fn _type_anchor<T: Type>(_t: T) {}
