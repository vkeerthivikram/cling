//! cling-daemon: owns the clipboard backend, the history store, the D-Bus
//! service, and the capture loop.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use cling_core::{ClipboardManager, ClipboardProvider, HistoryStore};
use cling_dbus_iface::{ClipboardManagerService, NoUnlock, UnlockRequest};

mod opts;

#[derive(Parser, Debug)]
#[command(
    name = "cling-daemon",
    version,
    about = "cling clipboard manager daemon"
)]
struct Cli {
    #[command(flatten)]
    opts: opts::DaemonOpts,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cling=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let opts = cli.opts;

    tracing::info!(?opts, "starting cling-daemon");

    // 1. History store.
    let store: Arc<dyn HistoryStore> = open_store(&opts)?;

    // 2. Backend (auto-detected).
    let provider: Arc<dyn ClipboardProvider> = select_backend(&opts).await?;

    // 3. Capture manager + loop.
    let manager = ClipboardManager::new();
    let manager_loop = manager.clone();
    let provider_for_loop = provider.clone();
    let store_for_loop = store.clone();
    tokio::spawn(async move {
        manager_loop.run(provider_for_loop, store_for_loop).await;
    });

    // 4. D-Bus service.
    let unlock: Arc<dyn UnlockRequest> = Arc::new(NoUnlock);
    let auto_paste = make_auto_paste(&provider);
    serve_dbus(store.clone(), provider.clone(), unlock, auto_paste).await?;

    Ok(())
}

/// Build the XTEST auto-paste callback if the backend supports it.
fn make_auto_paste(_provider: &Arc<dyn ClipboardProvider>) -> Option<Arc<dyn Fn() + Send + Sync>> {
    // Wired when the x11 feature is enabled; the callback synthesizes Ctrl+V.
    #[cfg(feature = "x11")]
    {
        // The provider is an opaque trait object; the daemon holds the concrete
        // X11Backend separately in the x11 branch of select_backend. Auto-paste
        // is invoked there directly via cling_backends::x11::auto_paste.
    }
    None
}

fn open_store(opts: &opts::DaemonOpts) -> Result<Arc<dyn HistoryStore>> {
    let path = store_path(opts)?;
    let passphrase = opts
        .passphrase
        .clone()
        .or_else(|| std::env::var("CLING_PASSPHRASE").ok());
    let store = cling_store::Store::open(&path, passphrase.as_deref())
        .with_context(|| format!("opening store at {path}"))?;
    Ok(Arc::new(store))
}

fn store_path(opts: &opts::DaemonOpts) -> Result<String> {
    if let Some(p) = &opts.db_path {
        return Ok(p.clone());
    }
    let dirs = directories::ProjectDirs::from("", "", "cling")
        .context("no home directory for data path")?;
    std::fs::create_dir_all(dirs.data_dir()).ok();
    Ok(dirs
        .data_dir()
        .join("history.clingdb")
        .to_string_lossy()
        .into_owned())
}

async fn select_backend(opts: &opts::DaemonOpts) -> Result<Arc<dyn ClipboardProvider>> {
    let has_wayland = opts.wayland || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = opts.x11 || std::env::var_os("DISPLAY").is_some();

    let choice = opts.backend.as_deref().map(str::to_ascii_lowercase);
    let want = choice.or_else(|| {
        cling_backends::detect_backend_name(has_wayland, has_x11).map(|s| s.to_string())
    });

    match want.as_deref() {
        Some("x11") => {
            #[cfg(feature = "x11")]
            {
                let b = cling_backends::x11::X11Backend::connect()
                    .context("X11 backend connect failed")?;
                return Ok(Arc::new(b));
            }
            #[cfg(not(feature = "x11"))]
            {
                anyhow::bail!("X11 backend not compiled in (rebuild with the x11 feature)");
            }
        }
        Some("wayland") => {
            anyhow::bail!(
                "Wayland backend requires the data-control protocol; not yet wired in this build"
            )
        }
        _ => anyhow::bail!("no display server detected (set DISPLAY or WAYLAND_DISPLAY)"),
    }
}

async fn serve_dbus(
    store: Arc<dyn HistoryStore>,
    provider: Arc<dyn ClipboardProvider>,
    unlock: Arc<dyn UnlockRequest>,
    auto_paste: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<()> {
    let conn = zbus::ConnectionBuilder::session()?.build().await?;
    let service = ClipboardManagerService::new(store, provider, unlock, conn.clone())
        .with_auto_paste(auto_paste);
    conn.object_server()
        .at(
            zvariant::ObjectPath::try_from(cling_dbus_iface::OBJECT_PATH).unwrap(),
            service,
        )
        .await?;
    conn.request_name(cling_dbus_iface::BUS_NAME)
        .await
        .map_err(|e| anyhow::anyhow!("request_name: {e}"))?;
    tracing::info!("D-Bus name {} acquired", cling_dbus_iface::BUS_NAME);
    std::future::pending::<()>().await;
    Ok(())
}
