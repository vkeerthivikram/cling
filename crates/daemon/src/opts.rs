//! Daemon CLI options.

use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct DaemonOpts {
    /// Override the history database path.
    #[arg(long)]
    pub db_path: Option<String>,

    /// Passphrase for the encrypted history DB. Prefer `CLING_PASSPHRASE` env
    /// or the unlock dialog over passing this on the command line.
    #[arg(long)]
    pub passphrase: Option<String>,

    /// Force the clipboard backend: "x11" or "wayland".
    #[arg(long)]
    pub backend: Option<String>,

    /// Assume a Wayland display is present even if `WAYLAND_DISPLAY` is unset.
    #[arg(long, default_value_t = false)]
    pub wayland: bool,

    /// Assume an X11 display is present even if `DISPLAY` is unset.
    #[arg(long, default_value_t = false)]
    pub x11: bool,

    /// Path/name of the `cling-show` binary used for the unlock dialog.
    #[arg(long, default_value = "cling-show")]
    pub show_binary: String,
}
