//! VNC 会话生命周期：连接、事件循环、断线重连。
//!
//! 与原 vnc-rs 版本的区别：vnc-rs 用 tokio 异步 + `poll_event()`，本版改用
//! libvncclient 的同步阻塞 `WaitForMessage`/`HandleRFBServerMessage`，在本线程内
//! 直接跑事件循环（不再 `block_on`）。重连框架（退避、手动重连、剪贴板恢复）保留。

use std::time::{Duration, Instant};

use crate::output_mailbox::OutputSender;
use crate::runtime::{RemoteDesktopConnectionOptions, RemoteDesktopInput, RemoteDesktopOutput};
use crate::vnc_client::VncClient;
use crate::vnc_encoding::FramePump;
use crate::vnc_input::{VncInputAction, VncPointerState, handle_pending_inputs};

/// 事件循环里 `WaitForMessage` 的阻塞超时（决定输入处理与帧处理的最小粒度）。
const POLL_TIMEOUT: Duration = Duration::from_millis(8);

pub fn run_vnc_thread(
    options: RemoteDesktopConnectionOptions,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    output_tx: OutputSender,
) {
    run_vnc_backend(options, input_rx, &output_tx);
}

fn run_vnc_backend(
    options: RemoteDesktopConnectionOptions,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    output_tx: &OutputSender,
) {
    let mut latest_clipboard_text = None;
    let mut reconnect_attempt = 0usize;
    loop {
        match run_vnc_session(&options, &mut latest_clipboard_text, input_rx, output_tx) {
            VncSessionResult::Closed | VncSessionResult::InputClosed => break,
            VncSessionResult::Reconnect {
                reason,
                manual,
                was_connected,
            } => {
                if was_connected || manual {
                    reconnect_attempt = 0;
                }
                if manual {
                    send_status(output_tx, "reconnecting VNC session");
                    continue;
                }
                let delay = reconnect_delay(reconnect_attempt);
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                send_status(output_tx, &reconnect_status_message(&reason, delay));
                if !wait_before_reconnect(input_rx, &mut latest_clipboard_text, delay) {
                    break;
                }
            }
        }
    }
}

enum VncSessionResult {
    Closed,
    InputClosed,
    Reconnect {
        reason: String,
        manual: bool,
        was_connected: bool,
    },
}

fn run_vnc_session(
    options: &RemoteDesktopConnectionOptions,
    latest_clipboard_text: &mut Option<String>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    output_tx: &OutputSender,
) -> VncSessionResult {
    send_status(
        output_tx,
        &format!("connecting to VNC {}", options.destination),
    );
    let mut client = match VncClient::connect(
        &options.destination,
        options.username.as_deref(),
        options.password.as_deref(),
    ) {
        Ok(client) => client,
        Err(error) => return reconnect_result(error.to_string(), false, false),
    };
    // 恢复上次会话的剪贴板。
    if let Some(text) = latest_clipboard_text.clone() {
        if text.is_ascii() {
            client.send_cut_text(&text);
        }
    }
    // 不再额外 request_refresh：rfbClientInitialise 已发过非增量全量首帧请求，
    // 重复请求会干扰 ARD 的首帧推送（实测导致收不到帧）。

    run_connected_vnc_session(client, latest_clipboard_text, input_rx, output_tx)
}

fn run_connected_vnc_session(
    mut client: VncClient,
    latest_clipboard_text: &mut Option<String>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    output_tx: &OutputSender,
) -> VncSessionResult {
    let mut pump = FramePump::new();
    let mut pointer = VncPointerState::default();
    let mut was_connected = false;

    loop {
        // 1. 处理输入（非阻塞 drain）。
        let action = handle_pending_inputs(
            &mut client,
            latest_clipboard_text,
            input_rx,
            &mut pointer,
            output_tx,
        );
        if let Some(result) = session_result_from_action(action, was_connected) {
            return result;
        }

        // 2. 等待并处理服务端消息（触发帧/剪贴板回调）。
        let msg = client.wait_for_message(POLL_TIMEOUT.as_micros() as u32);
        if msg < 0 {
            return reconnect_result("VNC server closed connection".to_string(), false, was_connected);
        }
        if msg != 0 && !client.handle_message() {
            return reconnect_result("VNC message handling failed".to_string(), false, was_connected);
        }

        // 3. 合并脏矩形并上送一帧。
        pump.pump(&mut client, output_tx);
        if client.saw_framebuffer() {
            was_connected = true;
        }
        pump.flush(output_tx);

        // 4. 增量刷新请求由 libvncclient 在处理完每帧后自动发出
        // （HandleRFBServerMessage 内部的 SendIncrementalFramebufferUpdateRequest），
        // 无需手动 request_refresh。手动发会干扰首帧协商。
    }
}

fn session_result_from_action(
    action: VncInputAction,
    was_connected: bool,
) -> Option<VncSessionResult> {
    match action {
        VncInputAction::Continue => None,
        VncInputAction::Closed => Some(VncSessionResult::Closed),
        VncInputAction::InputClosed => Some(VncSessionResult::InputClosed),
        VncInputAction::Reconnect => Some(reconnect_result(
            "manual reconnect".to_string(),
            true,
            was_connected,
        )),
        VncInputAction::Failed(reason) => Some(reconnect_result(reason, false, was_connected)),
    }
}

fn reconnect_delay(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_secs(1),
        1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        _ => Duration::from_secs(10),
    }
}

fn reconnect_status_message(reason: &str, delay: Duration) -> String {
    format!(
        "VNC disconnected: {reason}. Reconnecting in {}s",
        delay.as_secs()
    )
}

fn reconnect_result(reason: String, manual: bool, was_connected: bool) -> VncSessionResult {
    VncSessionResult::Reconnect {
        reason,
        manual,
        was_connected,
    }
}

fn wait_before_reconnect(
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    latest_clipboard_text: &mut Option<String>,
    delay: Duration,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        match handle_wait_input(input_rx, latest_clipboard_text) {
            WaitAction::Continue => {}
            WaitAction::ReconnectNow => return true,
            WaitAction::Stop => return false,
        }
        if Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(POLL_TIMEOUT);
    }
}

enum WaitAction {
    Continue,
    ReconnectNow,
    Stop,
}

fn handle_wait_input(
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    latest_clipboard_text: &mut Option<String>,
) -> WaitAction {
    match input_rx.try_recv() {
        Ok(RemoteDesktopInput::Close) => WaitAction::Stop,
        Ok(RemoteDesktopInput::Reconnect) => WaitAction::ReconnectNow,
        Ok(RemoteDesktopInput::ClipboardText { text }) => {
            *latest_clipboard_text = Some(text);
            WaitAction::Continue
        }
        Ok(RemoteDesktopInput::Text { text }) => {
            *latest_clipboard_text = Some(text);
            WaitAction::Continue
        }
        Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => WaitAction::Continue,
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => WaitAction::Stop,
    }
}

fn send_status(output_tx: &OutputSender, message: &str) {
    let _ = output_tx.send(RemoteDesktopOutput::Status(message.to_string()));
}
