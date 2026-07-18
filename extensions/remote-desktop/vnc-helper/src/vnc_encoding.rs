//! 帧缓冲处理：把 libvncclient 的脏矩形合并进 `RgbaFramebuffer` 并按需 flush。
//!
//! 与原 vnc-rs 版本的区别：事件不再来自 `VncClient::poll_event()`，而是由
//! `vnc_client` 的 C 回调累积脏矩形，事件循环在合适时机调用 `pump`/`flush`
//! 把 `client.framebuffer()`（RGBA）合并到本地 `RgbaFramebuffer`，再转 BGRA 上送。

use crate::framebuffer::RgbaFramebuffer;
use crate::output_mailbox::OutputSender;
use crate::runtime::{RemoteDesktopCapabilities, RemoteDesktopOutput, ResizeSupport};
use crate::vnc_client::VncClient;

/// 已连接会话的帧缓冲状态：负责把脏矩形合成帧并上送。
pub(crate) struct FramePump {
    framebuffer: Option<RgbaFramebuffer>,
    dirty: bool,
}

impl FramePump {
    pub(crate) fn new() -> Self {
        Self {
            framebuffer: None,
            dirty: false,
        }
    }

    /// 处理一轮：读取客户端累积的脏矩形与剪贴板，合并帧。
    pub(crate) fn pump(&mut self, client: &mut VncClient, output_tx: &OutputSender) {
        // 剪贴板（服务端 → 本地）
        for text in client.take_clipboard() {
            let _ = output_tx.send(RemoteDesktopOutput::ClipboardText { text });
        }

        let dirty_rects = client.take_dirty();
        if dirty_rects.is_empty() {
            return;
        }

        // 首帧时建立本地帧缓冲并上报 Connected。
        let (w, h) = client.size();
        if w == 0 || h == 0 {
            return;
        }
        if self
            .framebuffer
            .as_ref()
            .map_or(true, |fb| fb.width() != w || fb.height() != h)
        {
            self.framebuffer = Some(RgbaFramebuffer::new(w, h));
            let _ = output_tx.send(RemoteDesktopOutput::Connected {
                width: w,
                height: h,
                capabilities: vnc_capabilities(),
            });
        }

        let src = client.framebuffer();
        if src.is_empty() {
            return;
        }
        let stride = w as usize * 4;
        let Some(fb) = self.framebuffer.as_mut() else {
            return;
        };
        for rect in dirty_rects {
            // 从整帧 RGBA 中抠出脏矩形，patch 进本地帧缓冲。
            let mut buf = Vec::with_capacity(rect.width as usize * rect.height as usize * 4);
            for row in 0..rect.height as usize {
                let start = (rect.y as usize + row) * stride + rect.x as usize * 4;
                let end = start + rect.width as usize * 4;
                if end <= src.len() {
                    buf.extend_from_slice(&src[start..end]);
                }
            }
            if buf.len() == rect.width as usize * rect.height as usize * 4 {
                let _ = fb.patch_rgba_rect(rect.x, rect.y, rect.width, rect.height, &buf);
            }
        }
        self.dirty = true;
    }

    /// 把本地帧缓冲（若有更新）转 BGRA 上送一帧。
    pub(crate) fn flush(&mut self, output_tx: &OutputSender) {
        if !self.dirty {
            return;
        }
        let Some(fb) = &self.framebuffer else {
            return;
        };
        let _ = output_tx.send(RemoteDesktopOutput::Frame {
            width: fb.width(),
            height: fb.height(),
            rgba: fb.clone_bgra(),
        });
        self.dirty = false;
    }
}

fn vnc_capabilities() -> RemoteDesktopCapabilities {
    RemoteDesktopCapabilities {
        resize: ResizeSupport::LocalScaleOnly,
        clipboard_text: true,
        ..RemoteDesktopCapabilities::vnc_mvp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_mailbox::output_mailbox;

    #[test]
    fn capabilities_report_local_scale_and_clipboard() {
        let caps = vnc_capabilities();
        assert_eq!(caps.resize, ResizeSupport::LocalScaleOnly);
        assert!(caps.clipboard_text);
    }

    #[test]
    fn flush_without_dirty_sends_nothing() {
        let (output_tx, output_rx) = output_mailbox();
        let mut pump = FramePump::new();
        pump.flush(&output_tx);
        drop(output_tx);
        assert_eq!(None, output_rx.recv());
    }
}
