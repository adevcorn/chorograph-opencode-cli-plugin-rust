// chorograph-opencode-cli-plugin-rust
// Experimental plugin that:
//   1. Spawns `opencode serve --port 0 --print-logs true --log-level INFO` as a background
//      child process and parses the port from its stdout.
//   2. Spawns `opencode attach http://127.0.0.1:<PORT>` in a PTY so the user has a fully
//      interactive terminal inside Chorograph's FloatingPanel (⌘P).
//   3. Connects a non-blocking SSE observer to `GET /global/event` on the same server to
//      intercept tool calls, file writes, and turn-completion events and surface them in
//      Chorograph's activity log and spatial canvas.
//
// Host imports used (beyond the standard SDK):
//   host_spawn_pty   — spawn a PTY process
//   host_pty_resize  — resize a PTY
//   host_pty_close   — close a PTY
//   host_sse_get     — open a streaming GET SSE connection (GET variant of host_sse_post)

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chorograph_plugin_sdk_rust::prelude::*;
use chorograph_plugin_sdk_rust::sse::sse_read_raw;
use serde_json::{json, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Extra host imports not yet in the SDK
// ---------------------------------------------------------------------------

mod extra_ffi {
    extern "C" {
        /// Spawn a command in a new PTY. Returns a handle >= 10_000 on success, -1 on error.
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

        /// Open a streaming HTTP GET connection (SSE). Returns a handle >= 1 on success, 0 on error.
        /// headers_ptr/headers_len — optional JSON object of extra headers, or null/0.
        pub fn host_sse_get(
            url_ptr: *const u8,
            url_len: i32,
            headers_ptr: *const u8,
            headers_len: i32,
        ) -> i32;
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
        extra_ffi::host_spawn_pty(
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
        extra_ffi::host_pty_resize(handle, cols, rows);
    }
}

fn pty_close(handle: i32) {
    unsafe {
        extra_ffi::host_pty_close(handle);
    }
}

/// Open a non-blocking GET SSE stream. Returns handle > 0 on success, 0 on error.
fn sse_get(url: &str) -> i32 {
    unsafe { extra_ffi::host_sse_get(url.as_ptr(), url.len() as i32, core::ptr::null(), 0) }
}

// ---------------------------------------------------------------------------
// Plugin state (global statics — WASM is single-threaded)
// ---------------------------------------------------------------------------

static mut PTY_HANDLE: i32 = -1;
static mut TERMINAL_COLS: i32 = 80;
static mut TERMINAL_ROWS: i32 = 24;

/// Handle for the `opencode serve` background ChildProcess (used for cleanup).
static mut SERVE_PROC_HANDLE: i32 = -1;
/// Actual port opencode serve is listening on (0 until parsed).
static mut SERVER_PORT: u16 = 0;

/// Handle for the GET /global/event SSE stream.
static mut SSE_HANDLE: i32 = -1;
/// Accumulation buffer for partial SSE data across sidecar_poll calls.
static mut SSE_LINE_BUF: Vec<u8> = Vec::new();

// ---------------------------------------------------------------------------
// WASM plugin entry points
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    log!("[opencode-plugin] init: spawning opencode serve...");

    // -----------------------------------------------------------------------
    // Step 1 — Spawn `opencode serve --port 0 --print-logs true --log-level INFO`
    // -----------------------------------------------------------------------
    let serve_child = match ChildProcess::spawn(
        "opencode",
        vec![
            "serve",
            "--port",
            "0",
            "--print-logs",
            "true",
            "--log-level",
            "INFO",
        ],
        None,
        HashMap::new(),
    ) {
        Ok(c) => c,
        Err(_) => {
            fail_init(
                "Failed to spawn opencode serve. Make sure opencode is installed and on PATH.",
            );
            return;
        }
    };

    unsafe {
        SERVE_PROC_HANDLE = serve_child.handle();
    }

    // -----------------------------------------------------------------------
    // Step 2 — Read stdout until we see "listening on http://127.0.0.1:<PORT>"
    // -----------------------------------------------------------------------
    let port = match wait_for_port(&serve_child) {
        Some(p) => p,
        None => {
            fail_init("opencode serve did not print a port within the startup timeout.");
            return;
        }
    };

    log!(
        "[opencode-plugin] opencode serve is listening on port {}",
        port
    );
    unsafe {
        SERVER_PORT = port;
    }

    // -----------------------------------------------------------------------
    // Step 3 — Connect SSE sidecar to GET /global/event
    // -----------------------------------------------------------------------
    start_sidecar(port);

    // -----------------------------------------------------------------------
    // Step 4 — Spawn `opencode attach http://127.0.0.1:<PORT>` in a PTY
    // -----------------------------------------------------------------------
    let attach_url = format!("http://127.0.0.1:{}", port);
    let cols = 80i32;
    let rows = 24i32;

    let mut env: HashMap<&str, &str> = HashMap::new();
    env.insert("TERM", "xterm-256color");
    env.insert("COLORTERM", "truecolor");

    let pty_handle = spawn_pty("opencode", &["attach", &attach_url], None, env, cols, rows);
    if pty_handle < 0 {
        fail_init("opencode serve started but failed to spawn opencode attach PTY.");
        return;
    }

    unsafe {
        PTY_HANDLE = pty_handle;
        TERMINAL_COLS = cols;
        TERMINAL_ROWS = rows;
    }

    log!(
        "[opencode-plugin] PTY handle={}, SSE handle={}",
        pty_handle,
        unsafe { SSE_HANDLE }
    );
    push_terminal_ui(pty_handle);
}

#[chorograph_plugin]
pub fn handle_action(action_id: String, payload: Value) {
    match action_id.as_str() {
        // ------------------------------------------------------------------
        // PTY I/O actions (called by the Swift PluginTerminalView)
        // ------------------------------------------------------------------
        "pty_poll" => {
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            if handle < 0 {
                return;
            }

            let mut buf = [0u8; 4096];
            let n = unsafe {
                chorograph_plugin_sdk_rust::ffi::host_read(
                    handle,
                    1,
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                )
            };
            if n > 0 {
                let b64 = B64.encode(&buf[..n as usize]);
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
            let cols = payload.get("cols").and_then(|v| v.as_i64()).unwrap_or(80) as i32;
            let rows = payload.get("rows").and_then(|v| v.as_i64()).unwrap_or(24) as i32;
            if handle >= 0 {
                pty_resize(handle, cols, rows);
                unsafe {
                    TERMINAL_COLS = cols;
                    TERMINAL_ROWS = rows;
                }
            }
        }

        "pty_close" => {
            let handle = payload.get("handle").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            if handle >= 0 {
                pty_close(handle);
                unsafe {
                    PTY_HANDLE = -1;
                }
            }
            // Also close the SSE stream and kill the serve process.
            let sse = unsafe { SSE_HANDLE };
            if sse > 0 {
                unsafe {
                    extra_ffi::host_sse_get(core::ptr::null(), 0, core::ptr::null(), 0);
                }
                chorograph_plugin_sdk_rust::sse::sse_close(sse);
                unsafe {
                    SSE_HANDLE = -1;
                }
            }
            // Kill the serve process via host_kill — use raw FFI since ChildProcess::kill
            // requires ownership, but we stored only the handle.
            let serve_h = unsafe { SERVE_PROC_HANDLE };
            if serve_h >= 0 {
                unsafe {
                    chorograph_plugin_sdk_rust::ffi::host_kill(serve_h);
                }
                unsafe {
                    SERVE_PROC_HANDLE = -1;
                }
            }
            push_terminal_ui(-1);
        }

        // ------------------------------------------------------------------
        // SSE sidecar poll — called by Swift at ~2 fps
        // ------------------------------------------------------------------
        "sidecar_poll" => {
            sidecar_poll();
        }

        _ => {
            log!("[opencode-plugin] Unknown action: {}", action_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Startup helpers
// ---------------------------------------------------------------------------

/// Emit an error event and show an error label in the UI, then return.
fn fail_init(msg: &str) {
    log!("[opencode-plugin] FATAL: {}", msg);
    push_ai_event(
        "opencode",
        &AIEvent::Error {
            message: msg.to_string(),
        },
    );
    let err_ui = json!([{"type": "label", "text": format!("Error: {}", msg)}]);
    push_ui(&err_ui.to_string());
}

/// Read stdout from the serve child until we see "listening on http://..." and
/// parse the port. Times out after ~4 seconds (80 × 50ms spins).
fn wait_for_port(child: &ChildProcess) -> Option<u16> {
    let mut buf: Vec<u8> = Vec::new();
    let needle = b"listening on http://";

    for _ in 0..80 {
        // Drain stderr too so the pipe doesn't block.
        if let Ok(ReadResult::Data(err)) = child.read(PipeType::Stderr) {
            log!(
                "[opencode-plugin] serve stderr: {}",
                String::from_utf8_lossy(&err)
            );
        }

        match child.read(PipeType::Stdout) {
            Ok(ReadResult::Data(data)) => {
                buf.extend_from_slice(&data);
                // Scan for the listening line.
                if let Some(pos) = buf.windows(needle.len()).position(|w| w == needle) {
                    // Find the end of the line.
                    if let Some(end) = buf[pos..].iter().position(|&b| b == b'\n') {
                        let line = &buf[pos..pos + end];
                        let line_str = String::from_utf8_lossy(line);
                        // line_str is like: "listening on http://127.0.0.1:59114"
                        if let Some(port_str) = line_str.split(':').last() {
                            let clean = port_str
                                .trim()
                                .trim_end_matches(|c: char| !c.is_ascii_digit());
                            if let Ok(port) = clean.parse::<u16>() {
                                return Some(port);
                            }
                        }
                    }
                }
            }
            Ok(ReadResult::EOF) => break,
            Ok(ReadResult::Empty) => {
                // No data yet — wait a bit.
                child.wait_for_data(50);
            }
            Err(_) => break,
        }
    }
    None
}

/// Open the SSE stream to GET /global/event and store the handle.
fn start_sidecar(port: u16) {
    let url = format!("http://127.0.0.1:{}/global/event", port);
    let handle = sse_get(&url);
    if handle > 0 {
        unsafe {
            SSE_HANDLE = handle;
        }
        log!("[opencode-plugin] SSE sidecar connected, handle={}", handle);
    } else {
        log!(
            "[opencode-plugin] SSE sidecar failed to connect (handle={})",
            handle
        );
    }
}

// ---------------------------------------------------------------------------
// SSE sidecar polling + event mapping
// ---------------------------------------------------------------------------

fn sidecar_poll() {
    let sse_handle = unsafe { SSE_HANDLE };
    if sse_handle <= 0 {
        return;
    }

    // Drain available bytes into the line buffer (non-blocking).
    let mut read_buf = [0u8; 4096];
    let n = sse_read_raw(sse_handle, &mut read_buf);
    match n {
        n if n > 0 => unsafe {
            SSE_LINE_BUF.extend_from_slice(&read_buf[..n as usize]);
        },
        -1 => {
            // Stream ended or error.
            log!("[opencode-plugin] SSE stream ended");
            unsafe {
                SSE_HANDLE = -1;
            }
            return;
        }
        _ => {
            // 0 = no data yet, stream still open. Nothing to do.
        }
    }

    // Parse complete SSE events (separated by blank lines "\n\n").
    loop {
        let buf = unsafe { &SSE_LINE_BUF };
        // Find a double-newline event boundary.
        let boundary = find_double_newline(buf);
        match boundary {
            None => break, // No complete event yet.
            Some(end) => {
                // Extract the complete event bytes and remove from buffer.
                let event_bytes: Vec<u8> = unsafe { SSE_LINE_BUF.drain(..end + 2).collect() };
                process_sse_event(&event_bytes);
            }
        }
    }
}

/// Find the position just after a `\n\n` sequence (SSE event boundary).
/// Returns the index of the second `\n`.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 1);
        }
    }
    None
}

/// Parse a raw SSE event blob (which may contain multiple `data: ...` lines)
/// and map it to Chorograph AIEvents.
fn process_sse_event(raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);

    // Collect all `data: ` lines in this event block.
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data_lines.push(rest);
        }
    }
    if data_lines.is_empty() {
        return;
    }

    // Join multi-line data (rare, but SSE allows it).
    let data_str = data_lines.join("");

    let v: Value = match serde_json::from_str(&data_str) {
        Ok(v) => v,
        Err(_) => return,
    };

    map_sse_event_to_ai_events(&v);
}

/// Map a parsed SSE event JSON value to one or more Chorograph AIEvents.
fn map_sse_event_to_ai_events(v: &Value) {
    let event_type = match v.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return,
    };

    const SESSION: &str = "opencode";

    match event_type {
        // ------------------------------------------------------------------
        // Tool call events
        // ------------------------------------------------------------------
        "message.part.updated" => {
            let part = match v.get("part") {
                Some(p) => p,
                None => return,
            };
            let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

            if part_type == "tool" {
                let state = part.get("state").unwrap_or(&Value::Null);
                let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let tool_name = part.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
                let input = state.get("input").unwrap_or(&Value::Null);

                match status {
                    "running" => {
                        // Show the tool name as it starts.
                        push_ai_event(
                            SESSION,
                            &AIEvent::ToolCall {
                                name: tool_name.to_string(),
                            },
                        );
                    }
                    "completed" => {
                        match tool_name {
                            "write" | "edit" => {
                                // Try both "path" and "file_path" field names.
                                let path = input
                                    .get("path")
                                    .or_else(|| input.get("file_path"))
                                    .and_then(|p| p.as_str())
                                    .unwrap_or("unknown");
                                push_ai_event(
                                    SESSION,
                                    &AIEvent::ToolCall {
                                        name: format!("WRITE {}", path),
                                    },
                                );
                                // Emit a CrdtWrite so the spatial canvas shows a speculative overlay.
                                match read_host_file(path) {
                                    Ok(content) => {
                                        push_ai_event(
                                            SESSION,
                                            &AIEvent::CrdtWrite {
                                                session_id: SESSION.to_string(),
                                                path: path.to_string(),
                                                content,
                                            },
                                        );
                                    }
                                    Err(e) => {
                                        log!(
                                            "[opencode-plugin] CrdtWrite: failed to read {}: {:?}",
                                            path,
                                            e
                                        );
                                    }
                                }
                            }
                            "read" => {
                                let path = input
                                    .get("file_path")
                                    .or_else(|| input.get("path"))
                                    .and_then(|p| p.as_str())
                                    .unwrap_or("unknown");
                                push_ai_event(
                                    SESSION,
                                    &AIEvent::ToolCall {
                                        name: format!("READ {}", path),
                                    },
                                );
                            }
                            "bash" | "glob" | "grep" => {
                                // Use the state title if available, otherwise the input command/pattern.
                                let title = state
                                    .get("title")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_else(|| {
                                        input
                                            .get("command")
                                            .or_else(|| input.get("pattern"))
                                            .or_else(|| input.get("query"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                    });
                                push_ai_event(
                                    SESSION,
                                    &AIEvent::ToolCall {
                                        name: format!("{} {}", tool_name, title),
                                    },
                                );
                            }
                            other => {
                                // Generic tool completion.
                                let title =
                                    state.get("title").and_then(|t| t.as_str()).unwrap_or(other);
                                push_ai_event(
                                    SESSION,
                                    &AIEvent::ToolCall {
                                        name: title.to_string(),
                                    },
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            // message.part.delta (streaming LLM tokens) — silently drop.
        }

        // ------------------------------------------------------------------
        // Session lifecycle events
        // ------------------------------------------------------------------
        "session.idle" => {
            push_ai_event(
                SESSION,
                &AIEvent::TurnCompleted {
                    session_id: SESSION.to_string(),
                },
            );
        }

        "session.error" => {
            let msg = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("opencode session error");
            push_ai_event(
                SESSION,
                &AIEvent::Error {
                    message: msg.to_string(),
                },
            );
        }

        _ => {
            // All other event types (message.part.delta, session.created, etc.) are dropped.
        }
    }
}

// ---------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------

fn push_terminal_ui(handle: i32) {
    let cols = unsafe { TERMINAL_COLS };
    let rows = unsafe { TERMINAL_ROWS };
    let components = json!([{
        "type": "terminal",
        "handle": handle,
        "cols": cols,
        "rows": rows
    }]);
    push_ui(&components.to_string());
}

fn push_ai_event(session_id: &str, event: &AIEvent) {
    chorograph_plugin_sdk_rust::ui::push_ai_event(session_id, event);
}

fn push_ui(json: &str) {
    chorograph_plugin_sdk_rust::ui::push_ui(json);
}
