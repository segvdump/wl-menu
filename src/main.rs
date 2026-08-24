//! wl-menu: Wayland layer-shell menu daemon and IPC client CLI.

mod apps;
mod config;
mod debug;
mod icons;
mod ipc;
mod menu;
mod render;
mod state;
mod wayland;

use clap::{Parser, Subcommand};
use config::Config;
use ipc::{client_call, socket_path, IpcRequest, IpcResponse};
use std::path::PathBuf;
use std::process::Command;

const LOG_PREFIX: &str = "wl-menu";

#[derive(Parser, Debug)]
#[command(name = "wl-menu", about = "Wayland layer-shell hierarchical menu daemon")]
struct Args {
    #[command(subcommand)]
    command: Option<CommandKind>,

    /// Run as background daemon (used by autostart)
    #[arg(long, global = true)]
    daemon: bool,

    /// Config file (default: $XDG_CONFIG_HOME/wl-menu/wl-menu.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    /// Open the menu at an optional route (default: root)
    Open {
        #[arg(default_value = "root")]
        route: String,
    },
    /// Toggle the menu at an optional route
    Toggle {
        #[arg(default_value = "root")]
        route: String,
    },
    /// Close the menu
    Close,
    /// Reload menu JSONC and theme config
    Reload,
    /// Ping the daemon
    Ping,
}

fn main() {
    let args = Args::parse();
    if args.daemon || args.command.is_none() {
        run_daemon(args.config.as_deref());
        return;
    }

    let response = match args.command.unwrap() {
        CommandKind::Open { route } => {
            ipc_call(IpcRequest::Open { menu: route }, args.config.as_deref())
        }
        CommandKind::Toggle { route } => {
            ipc_call(IpcRequest::Toggle { menu: route }, args.config.as_deref())
        }
        CommandKind::Close => ipc_call(IpcRequest::Close, args.config.as_deref()),
        CommandKind::Reload => ipc_call(IpcRequest::Reload, args.config.as_deref()),
        CommandKind::Ping => ipc_call(IpcRequest::Ping, args.config.as_deref()),
    };

    if response.ok {
        std::process::exit(0);
    }
    eprintln!(
        "{LOG_PREFIX}: {}",
        response.error.unwrap_or_else(|| "ipc failed".into())
    );
    std::process::exit(1);
}

fn ipc_call(request: IpcRequest, config: Option<&std::path::Path>) -> IpcResponse {
    let path = socket_path();
    match client_call(&path, &request) {
        Ok(response) => response,
        Err(err) => {
            if matches!(request, IpcRequest::Toggle { .. } | IpcRequest::Open { .. }) {
                eprintln!("{LOG_PREFIX}: {err}, starting daemon");
                spawn_daemon(config);
                std::thread::sleep(std::time::Duration::from_millis(250));
                return client_call(&path, &request).unwrap_or(IpcResponse::err(err));
            }
            IpcResponse::err(err)
        }
    }
}

/// Spawn a detached daemon, forwarding `--config` when the client used one.
fn spawn_daemon(config: Option<&std::path::Path>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wl-menu"));
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");
    if let Some(path) = config {
        cmd.arg("--config").arg(path);
    }
    let _ = cmd.spawn();
}

fn run_daemon(config_path: Option<&std::path::Path>) {
    let config = Config::load(config_path);
    let socket = socket_path();
    if let Err(()) = wayland::run_daemon(config, socket) {
        std::process::exit(1);
    }
}
