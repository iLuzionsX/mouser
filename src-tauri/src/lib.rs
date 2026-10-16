//! mouser: one mouse and keyboard, two computers.
//!
//! The library half of the crate. `main.rs` handles the CLI and hands off to
//! [`run`], which wires input capture, the encrypted link, and the webview
//! together.

pub mod bridge;
pub mod commands;
pub mod link;
pub mod state;

use std::sync::Arc;

use state::AppState;

/// Start the Tauri application.
///
/// `secret` is the pairing secret shared with the other machine. It is kept in
/// memory only: a config file holding a reusable secret is a liability, so the
/// UI takes it each session or it comes from `MOUSER_SECRET`.
pub fn run(config: mouser_core::config::Config, secret: String) -> anyhow::Result<()> {
    let state = Arc::new(AppState::new(config, &secret)?);
    let app_state = Arc::clone(&state);

    let mut builder = commands::builder(state.clone());

    // Start the link before the window opens so the UI's first poll already
    // shows a real connection state rather than a blank one.
    builder = builder.setup(move |_app| {
        let link_state = Arc::clone(&app_state);
        tauri::async_runtime::spawn(async move {
            link::run(link_state).await;
        });
        Ok(())
    });

    let app = builder.build(tauri::generate_context!())?;
    app.run(move |_, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            state.shutdown();
        }
    });
    Ok(())
}
