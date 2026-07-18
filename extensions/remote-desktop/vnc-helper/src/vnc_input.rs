//! 输入处理：把 `RemoteDesktopInput` 转成 libvncclient 的键鼠/剪贴板调用。
//!
//! 与原 vnc-rs 版本的区别：输入不再经 `VncClient::input(X11Event)`，而是直接调
//! `vnc_client::VncClient` 的 `send_key`/`send_pointer`/`send_cut_text`。
//! 指针按钮/滚轮 mask 逻辑（`VncPointerState`）与按键 keysym 映射（`vnc_keyboard`）
//! 与库无关，原样保留。

use crate::output_mailbox::OutputSender;
use crate::runtime::{RemoteDesktopInput, RemoteDesktopOutput, RemoteMouseButton};
use crate::vnc_client::VncClient;
use crate::vnc_keyboard::remote_key_to_keysym;

const MAX_INPUTS_PER_POLL: usize = 256;

pub(crate) enum VncInputAction {
    Continue,
    Closed,
    InputClosed,
    Reconnect,
    Failed(String),
}

pub(crate) fn handle_pending_inputs(
    client: &mut VncClient,
    latest_clipboard_text: &mut Option<String>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
    pointer: &mut VncPointerState,
    output_tx: &OutputSender,
) -> VncInputAction {
    let inputs = match drain_remote_inputs(input_rx) {
        VncInputBatch::Inputs(inputs) => inputs,
        VncInputBatch::Disconnected => return VncInputAction::InputClosed,
    };
    for input in inputs {
        let action = handle_vnc_input(client, latest_clipboard_text, pointer, input, output_tx);
        if !matches!(action, VncInputAction::Continue) {
            return action;
        }
    }
    VncInputAction::Continue
}

fn handle_vnc_input(
    client: &mut VncClient,
    latest_clipboard_text: &mut Option<String>,
    pointer: &mut VncPointerState,
    input: RemoteDesktopInput,
    output_tx: &OutputSender,
) -> VncInputAction {
    match send_vnc_input(client, latest_clipboard_text, pointer, input, output_tx) {
        Ok(action) => action,
        Err(error) => VncInputAction::Failed(error.to_string()),
    }
}

fn send_vnc_input(
    client: &mut VncClient,
    latest_clipboard_text: &mut Option<String>,
    pointer: &mut VncPointerState,
    input: RemoteDesktopInput,
    output_tx: &OutputSender,
) -> anyhow::Result<VncInputAction> {
    match input {
        RemoteDesktopInput::Close => Ok(VncInputAction::Closed),
        RemoteDesktopInput::Reconnect => Ok(VncInputAction::Reconnect),
        RemoteDesktopInput::MouseMove { x, y } => {
            pointer.move_to(x, y);
            send_pointer_event(client, pointer);
            Ok(VncInputAction::Continue)
        }
        RemoteDesktopInput::MouseButton { button, pressed } => {
            pointer.set_button(button, pressed);
            send_pointer_event(client, pointer);
            Ok(VncInputAction::Continue)
        }
        RemoteDesktopInput::Wheel { vertical, units } => {
            let (x, y, _) = pointer.snapshot();
            for mask in pointer.wheel_masks(vertical, units) {
                client.send_pointer(x, y, mask);
            }
            Ok(VncInputAction::Continue)
        }
        RemoteDesktopInput::Key { key, pressed } => {
            if let Some(keysym) = remote_key_to_keysym(&key) {
                client.send_key(keysym, pressed);
            }
            Ok(VncInputAction::Continue)
        }
        RemoteDesktopInput::ClipboardText { text } | RemoteDesktopInput::Text { text } => {
            send_clipboard_text(client, latest_clipboard_text, text, output_tx)
        }
        RemoteDesktopInput::Resize { .. } => Ok(VncInputAction::Continue),
    }
}

fn send_pointer_event(client: &mut VncClient, pointer: &VncPointerState) {
    let (x, y, mask) = pointer.snapshot();
    client.send_pointer(x, y, mask);
}

fn send_clipboard_text(
    client: &mut VncClient,
    latest_clipboard_text: &mut Option<String>,
    text: String,
    output_tx: &OutputSender,
) -> anyhow::Result<VncInputAction> {
    if !text.is_ascii() {
        send_status(
            output_tx,
            "VNC clipboard currently supports ASCII text only",
        );
        return Ok(VncInputAction::Continue);
    }
    *latest_clipboard_text = Some(text.clone());
    client.send_cut_text(&text);
    Ok(VncInputAction::Continue)
}

enum VncInputBatch {
    Inputs(Vec<RemoteDesktopInput>),
    Disconnected,
}

fn drain_remote_inputs(
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<RemoteDesktopInput>,
) -> VncInputBatch {
    let mut inputs = Vec::new();
    for _ in 0..MAX_INPUTS_PER_POLL {
        match input_rx.try_recv() {
            Ok(input) => inputs.push(input),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return VncInputBatch::Disconnected;
            }
        }
    }
    VncInputBatch::Inputs(coalesce_remote_inputs(inputs))
}

fn coalesce_remote_inputs<I>(inputs: I) -> Vec<RemoteDesktopInput>
where
    I: IntoIterator<Item = RemoteDesktopInput>,
{
    let mut coalesced = Vec::new();
    let mut pending_mouse_move = None;
    for input in inputs {
        match input {
            RemoteDesktopInput::MouseMove { .. } => pending_mouse_move = Some(input),
            input => {
                if let Some(mouse_move) = pending_mouse_move.take() {
                    coalesced.push(mouse_move);
                }
                coalesced.push(input);
            }
        }
    }
    if let Some(mouse_move) = pending_mouse_move {
        coalesced.push(mouse_move);
    }
    coalesced
}

#[derive(Default)]
pub(crate) struct VncPointerState {
    x: u16,
    y: u16,
    buttons: u8,
}

impl VncPointerState {
    fn move_to(&mut self, x: u16, y: u16) -> u8 {
        self.x = x;
        self.y = y;
        self.buttons
    }

    fn set_button(&mut self, button: RemoteMouseButton, pressed: bool) -> u8 {
        let Some(bit) = vnc_button_bit(button) else {
            return self.buttons;
        };
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.buttons
    }

    fn wheel_masks(&self, vertical: bool, units: i16) -> Vec<u8> {
        let Some(bit) = vnc_wheel_bit(vertical, units) else {
            return Vec::new();
        };
        vec![self.buttons | bit, self.buttons]
    }

    fn snapshot(&self) -> (u16, u16, u8) {
        (self.x, self.y, self.buttons)
    }
}

fn vnc_button_bit(button: RemoteMouseButton) -> Option<u8> {
    match button {
        RemoteMouseButton::Left => Some(1),
        RemoteMouseButton::Middle => Some(2),
        RemoteMouseButton::Right => Some(4),
        RemoteMouseButton::X1 => Some(128),
        RemoteMouseButton::X2 => None,
    }
}

fn vnc_wheel_bit(vertical: bool, units: i16) -> Option<u8> {
    match (vertical, units.signum()) {
        (true, -1) => Some(8),
        (true, 1) => Some(16),
        (false, -1) => Some(32),
        (false, 1) => Some(64),
        _ => None,
    }
}

fn send_status(output_tx: &OutputSender, message: &str) {
    let _ = output_tx.send(RemoteDesktopOutput::Status(message.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_state_tracks_vnc_button_mask() {
        let mut state = VncPointerState::default();

        assert_eq!(0, state.move_to(10, 11));
        assert_eq!(1, state.set_button(RemoteMouseButton::Left, true));
        assert_eq!(5, state.set_button(RemoteMouseButton::Right, true));
        assert_eq!(4, state.set_button(RemoteMouseButton::Left, false));
        assert_eq!(0, state.set_button(RemoteMouseButton::Right, false));
        assert_eq!((10, 11, 0), state.snapshot());
    }

    #[test]
    fn wheel_events_use_vnc_button_press_and_release_masks() {
        let mut state = VncPointerState::default();
        state.move_to(20, 21);

        assert_eq!(vec![8, 0], state.wheel_masks(true, -120));
        assert_eq!(vec![16, 0], state.wheel_masks(true, 120));
        assert_eq!(vec![32, 0], state.wheel_masks(false, -120));
        assert_eq!(vec![64, 0], state.wheel_masks(false, 120));
        assert_eq!((20, 21, 0), state.snapshot());
    }
}
