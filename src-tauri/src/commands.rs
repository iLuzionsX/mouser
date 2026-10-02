//! Tauri commands: the only surface the webview can reach.
//!
//! Every command is a thin wrapper. Anything that needs to hold a lock does it
//! inside [`AppState`], so no command can block the UI thread for long.

use std::sync::Arc;

use mouser_core::layout::Edge;
use tauri::{Manager, State};

use crate::state::AppState;

/// Managed handle so commands can take `State<AppState>`.
pub type Shared = Arc<AppState>;

/// Full state for the UI.
///
/// Input is processed on a dedicated thread the moment it is captured, so
/// this poll only drives rendering; nothing about a cursor crossing waits
/// for it.
#[tauri::command]
pub fn snapshot(state: State<'_, Shared>) -> crate::state::Snapshot {
    state.snapshot()
}

/// Choose which side the other machine's screen is on.
#[tauri::command]
pub fn set_edge(state: State<'_, Shared>, edge: Edge) -> Result<(), String> {
    state.set_edge(edge);
    Ok(())
}

/// Store the pairing secret.
///
/// The secret is write-only from the UI's perspective: it is never returned in
/// [`snapshot`], only the derived fingerprint is, so it cannot be read back
/// off the screen.
#[tauri::command]
pub fn set_secret(state: State<'_, Shared>, secret: String) -> Result<(), String> {
    let secret = secret.trim().to_string();
    mouser_core::config::Config::validate_secret(&secret).map_err(|e| e.to_string())?;
    state.set_secret(secret);
    Ok(())
}

/// Stop capturing input and exit cleanly.
///
/// Capture runs on a hook thread, so the app must release it rather than
/// letting the process teardown strand a global hook.
#[tauri::command]
pub fn quit(app: tauri::AppHandle) {
    if let Some(state) = app.try_state::<Shared>() {
        state.shutdown();
    }
    app.exit(0);
}

/// Set up the app. Called from `main`.
pub fn builder(state: Shared) -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            snapshot, set_edge, set_secret, quit
        ])
}
