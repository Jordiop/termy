//! termy — native multi-session terminal workspace for AI-agent workflows.
//!
//! Entry point: starts the `winit` event loop with [`app::App`], which owns
//! the sessions, workspace, renderer, AI monitor and config hot-reload.

// Release builds on Windows are GUI apps: no extra console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai_monitor;
mod app;
mod chrome;
mod config;
mod input;
mod links;
mod osc;
mod paste;
mod prompts;
mod pty;
mod renderer;
mod resources;
mod session;
mod settings_ui;
mod stats_ui;
mod theme;
mod usage_stats;
mod workspace;

use winit::event_loop::EventLoop;

/// Thread-safe "please redraw" callback. In the app it wraps an
/// `EventLoopProxy`; background threads (PTY readers, AI monitor, config
/// watcher) call it instead of touching UI state.
pub type Wake = std::sync::Arc<dyn Fn() + Send + Sync>;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    log::info!("termy {}", env!("CARGO_PKG_VERSION"));

    let event_loop = match EventLoop::<app::UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("termy: cannot start the event loop: {e}");
            std::process::exit(1);
        }
    };
    let mut app = app::App::new(event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("termy: {e}");
        std::process::exit(1);
    }
}
