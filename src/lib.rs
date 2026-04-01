// chorograph-opencode-cli-plugin-rust
// Experimental plugin that spawns opencode in a PTY and renders it as an
// interactive terminal inside Chorograph's plugin UI panel.
//
// Host imports used (beyond the standard SDK):
//   host_spawn_pty(cmd, args_json, cwd, env_json, cols, rows) -> i32 handle
//   host_pty_resize(handle, cols, rows) -> i32
//   host_pty_close(handle) -> i32
// These are not yet in the SDK (v0.2.2), so they are declared directly here.
//
// Standard SDK imports used:
//   host_write(handle, ptr, len) -> i32   (send keyboard input to PTY)
//   host_read(handle, pipe, ptr, len) -> i32  (read PTY output; pipe=1 for stdout)
//   host_push_ui(ptr, len) -> i32
//   host_push_ai_event(s_ptr, s_len, e_ptr, e_len) -> i32

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chorograph_plugin_sdk_rust::prelude::*;
use serde_json::{json, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Extra PTY host imports not yet in the SDK
// ---------------------------------------------------------------------------

mod pty_ffi {
    extern "C" {
        /// Spawn a command in a new PTY. Returns a handle >= 10_000 on success, -1 on error.
        /// args_json: JSON array of strings e.g. `["--flag"]` or `[]`
        /// env_json:  JSON object of extra env vars e.g. `{"TERM":"xterm-256color"}`
        /// cwd:       working directory string or empty string for inherit
        pub fn host_spawn_pty(
            cmd_ptr: *const u8,
            cmd_len: i32,
            args_ptr: *const u8,
            args_len: i32,
            cwd_ptr: *const u8,
            cwd_len: i32,
            env_ptr: *const u8,
            env_len: i32,
            cols: i32,
            rows: i32,
        ) -> i32;

        /// Resize the PTY. Returns 0 on success, -1 on error.
        pub fn host_pty_resize(handle: i32, cols: i32, rows: i32) -> i32;

        /// Close and deallocate the PTY. Returns 0.
        pub fn host_pty_close(handle: i32) -> i32;
    }
}

fn spawn_pty(
    cmd: &str,
    args: &[&str],
    cwd: Option<&str>,
    env: HashMap<&str, &str>,
    cols: i32,
    rows: i32,
) -> i32 {
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
    let env_json = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string());
    let cwd_str = cwd.unwrap_or("");
    unsafe {
        pty_ffi::host_spawn_pty(
            cmd.as_ptr(),
            cmd.len() as i32,
            args_json.as_ptr(),
            args_json.len() as i32,
            cwd_str.as_ptr(),
            cwd_str.len() as i32,
            env_json.as_ptr(),
            env_json.len() as i32,
            cols,
            rows,
        )
    }
}

fn pty_resize(handle: i32, cols: i32, rows: i32) {
    unsafe {
        pty_ffi::host_pty_resize(handle, cols, rows);
    }
}

fn pty_close(handle: i32) {
    unsafe {
        pty_ffi::host_pty_close(handle);
    }
}

// ---------------------------------------------------------------------------
// Plugin state (global statics — WASM is single-threaded)
// ---------------------------------------------------------------------------

static mut PTY_HANDLE: i32 = -1;
static mut TERMINAL_COLS: i32 = 220;
static mut TERMINAL_ROWS: i32 = 50;

// ---------------------------------------------------------------------------
// WASM plugin entry points
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    // Auto-spawn opencode immediately with conservative default dimensions.
    // The Swift host's GeometryReader will send pty_resize with the real panel
    // size as soon as the view lays out, so these are just initial values.
    let cols = 80i32;
    let rows = 24i32;

    let mut env: HashMap<&str, &str> = HashMap::new();
    env.insert("TERM", "xterm-256color");
    env.insert("COLORTERM", "truecolor");

    let handle = spawn_pty("opencode", &[], None, env, cols, rows);

    if handle < 0 {
        log!("[opencode-plugin] Failed to spawn opencode PTY");
        push_ai_event(
            "opencode",
            &AIEvent::Error {
                message: "Failed to spawn opencode. Make sure opencode is installed and on PATH."
                    .to_string(),
            },
        );
        // Show an error label so the user knows what happened.
        let err_ui = serde_json::json!([{
            "type": "label",
            "text": "Error: could not spawn opencode. Check that it is on PATH."
        }]);
        push_ui(&err_ui.to_string());
        return;
    }

    unsafe {
        PTY_HANDLE = handle;
        TERMINAL_COLS = cols;
        TERMINAL_ROWS = rows;
    }

    log!(
        "[opencode-plugin] Auto-spawned opencode, PTY handle={}",
        handle
    );
    push_terminal_ui(handle);
}

#[chorograph_plugin]
pub fn handle_action(action_id: String, payload: Value) {
    match action_id.as_str() {
        "spawn_opencode" => {
            let cols = payload.get("cols").and_then(|v| v.as_i64()).unwrap_or(220) as i32;
            let rows = payload.get("rows").and_then(|v| v.as_i64()).unwrap_or(50) as i32;
            let cwd = payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let mut env: HashMap<&str, &str> = HashMap::new();
            env.insert("TERM", "xterm-256color");
            env.insert("COLORTERM", "truecolor");

            let handle = spawn_pty("opencode", &[], cwd.as_deref(), env, cols, rows);

            if handle < 0 {
                log!("[opencode-plugin] Failed to spawn opencode PTY");
                push_ai_event(
                    "opencode",
                    &AIEvent::Error {
                        message:
                            "Failed to spawn opencode. Make sure opencode is installed and on PATH."
                                .to_string(),
                    },
                );
                return;
            }

            unsafe {
                PTY_HANDLE = handle;
                TERMINAL_COLS = cols;
                TERMINAL_ROWS = rows;
            }

            log!("[opencode-plugin] Spawned opencode, PTY handle={}", handle);
            // Update UI to show the live terminal.
            push_terminal_ui(handle);
        }

        "pty_poll" => {
            // Called by the Swift host every ~16ms to drain PTY output.
            // We read up to 4096 bytes and push them back as a "ptyData" AI event
            // so the Swift PluginTerminalView can feed them into its VirtualTerminal.
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            if handle < 0 {
                return;
            }

            let mut buf = [0u8; 4096];
            // pipe=1 means stdout (the PTY output stream)
            let n = unsafe {
                chorograph_plugin_sdk_rust::ffi::host_read(
                    handle,
                    1, // stdout/PTY
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                )
            };

            if n > 0 {
                let data = &buf[..n as usize];
                let b64 = B64.encode(data);
                push_ai_event(
                    "opencode-pty",
                    &AIEvent::StreamingDelta {
                        session_id: "opencode-pty".to_string(),
                        text: b64,
                    },
                );
            }
        }

        "pty_input" => {
            // Keyboard input from the Swift view, base64-encoded.
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            if handle < 0 {
                return;
            }
            let b64 = payload.get("data").and_then(|v| v.as_str()).unwrap_or("");
            if let Ok(bytes) = B64.decode(b64) {
                if !bytes.is_empty() {
                    let _ = unsafe {
                        chorograph_plugin_sdk_rust::ffi::host_write(
                            handle,
                            bytes.as_ptr(),
                            bytes.len() as i32,
                        )
                    };
                }
            }
        }

        "pty_resize" => {
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            let cols = payload.get("cols").and_then(|v| v.as_i64()).unwrap_or(220) as i32;
            let rows = payload.get("rows").and_then(|v| v.as_i64()).unwrap_or(50) as i32;
            if handle >= 0 {
                pty_resize(handle, cols, rows);
            }
        }

        "pty_close" => {
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            if handle >= 0 {
                pty_close(handle);
                unsafe {
                    PTY_HANDLE = -1;
                }
                push_terminal_ui(-1);
            }
        }

        _ => {
            log!("[opencode-plugin] Unknown action: {}", action_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn push_terminal_ui(handle: i32) {
    let cols = unsafe { TERMINAL_COLS };
    let rows = unsafe { TERMINAL_ROWS };

    let components = json!([
        {
            "type": "terminal",
            "handle": handle,
            "cols": cols,
            "rows": rows
        }
    ]);

    push_ui(&components.to_string());
}

fn push_ai_event(session_id: &str, event: &AIEvent) {
    chorograph_plugin_sdk_rust::ui::push_ai_event(session_id, event);
}

fn push_ui(json: &str) {
    chorograph_plugin_sdk_rust::ui::push_ui(json);
}
