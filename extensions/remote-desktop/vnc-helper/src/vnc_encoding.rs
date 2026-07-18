//! 帧缓冲处理：把 libvncclient 的脏矩形合并进 `RgbaFramebuffer`，按需以**增量帧**
//! 或整帧上送。
//!
//! 性能关键：静止/小改动时只把脏矩形的 BGRA 发给主程序（KB 级），而非每帧整帧
//! （2880×1620 ≈ 18.6MB）。脏面积过大或矩形过多时退化为整帧，避免"一堆矩形比整帧还大"。

use crate::framebuffer::RgbaFramebuffer;
use crate::output_mailbox::OutputSender;
use crate::protocol::FrameRect;
use crate::runtime::{RemoteDesktopCapabilities, RemoteDesktopOutput, ResizeSupport};
use crate::vnc_client::{DirtyRect, VncClient};

/// 脏面积达到全屏的该比例即退化整帧。
const FULL_FRAME_AREA_RATIO_NUM: usize = 1;
const FULL_FRAME_AREA_RATIO_DEN: usize = 2;
/// 脏矩形数量超过该值即退化整帧。
const FULL_FRAME_RECT_LIMIT: usize = 32;

/// 已连接会话的帧缓冲状态：合并脏矩形、按需增量/整帧上送。
pub(crate) struct FramePump {
    framebuffer: Option<RgbaFramebuffer>,
    /// 本轮（自上次 flush 以来）合并后的脏矩形。
    dirty_rects: Vec<FrameRect>,
    /// 是否已向主程序发过完整底帧（首帧/尺寸变化/重连后必须整帧）。
    sent_base_frame: bool,
}

impl FramePump {
    pub(crate) fn new() -> Self {
        Self {
            framebuffer: None,
            dirty_rects: Vec::new(),
            sent_base_frame: false,
        }
    }

    /// 处理一轮：读取客户端累积的脏矩形与剪贴板，合并并 patch 进本地帧缓冲。
    pub(crate) fn pump(&mut self, client: &mut VncClient, output_tx: &OutputSender) {
        // 剪贴板（服务端 → 本地）
        for text in client.take_clipboard() {
            let _ = output_tx.send(RemoteDesktopOutput::ClipboardText { text });
        }

        let dirty = client.take_dirty();
        if dirty.is_empty() {
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
            self.dirty_rects.clear();
            self.sent_base_frame = false; // 尺寸变化后必须重发整帧
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

        // 合并客户端上报的脏矩形（含与本论已累积的）。
        let mut rects: Vec<FrameRect> = self.dirty_rects.clone();
        rects.extend(dirty.iter().map(to_frame_rect));
        let merged = merge_rects(rects);

        // 把整帧 RGBA 中的脏矩形 patch 进本地帧缓冲。
        for rect in &merged {
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
        self.dirty_rects = merged;
    }

    /// 把本地帧缓冲上送：脏面积小发增量帧，否则发整帧。
    pub(crate) fn flush(&mut self, output_tx: &OutputSender) {
        let Some(fb) = &self.framebuffer else {
            return;
        };
        if self.dirty_rects.is_empty() && self.sent_base_frame {
            return;
        }

        let (fw, fh) = (fb.width(), fb.height());
        let full_area = fw as usize * fh as usize;
        let dirty_area: usize = self
            .dirty_rects
            .iter()
            .map(|r| r.width as usize * r.height as usize)
            .sum();
        let send_full = !self.sent_base_frame
            || self.dirty_rects.len() > FULL_FRAME_RECT_LIMIT
            || dirty_area * FULL_FRAME_AREA_RATIO_DEN >= full_area * FULL_FRAME_AREA_RATIO_NUM;

        if send_full {
            let _ = output_tx.send(RemoteDesktopOutput::FrameBgra {
                width: fw,
                height: fh,
                bgra: fb.clone_bgra(),
            });
            self.sent_base_frame = true;
        } else {
            // 打包各脏矩形的 BGRA（按顺序拼接）。
            let mut bgra = Vec::with_capacity(dirty_area * 4);
            let mut rects = Vec::with_capacity(self.dirty_rects.len());
            for rect in &self.dirty_rects {
                if let Some(bytes) = fb.clone_bgra_rect(rect.x, rect.y, rect.width, rect.height) {
                    bgra.extend_from_slice(&bytes);
                    rects.push(*rect);
                }
            }
            if !rects.is_empty() {
                let _ = output_tx.send(RemoteDesktopOutput::FrameRectsBgra {
                    width: fw,
                    height: fh,
                    rects,
                    bgra,
                });
            }
        }
        self.dirty_rects.clear();
    }
}

fn to_frame_rect(rect: &DirtyRect) -> FrameRect {
    FrameRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// 合并重叠/相邻矩形为其包围盒，迭代至收敛。控制数量与总面积，降低增量包开销。
fn merge_rects(mut rects: Vec<FrameRect>) -> Vec<FrameRect> {
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                if rects_touch_or_overlap(rects[i], rects[j]) {
                    rects[i] = bounding_box(rects[i], rects[j]);
                    rects.remove(j);
                    merged = true;
                    continue 'outer;
                }
            }
        }
    }
    rects
}

/// 两矩形是否重叠或相邻（间隔 ≤1 像素，便于合并细碎矩形）。
fn rects_touch_or_overlap(a: FrameRect, b: FrameRect) -> bool {
    let a_r = a.x as u32 + a.width as u32;
    let a_b = a.y as u32 + a.height as u32;
    let b_r = b.x as u32 + b.width as u32;
    let b_b = b.y as u32 + b.height as u32;
    a.x as u32 <= b_r + 1 && b.x as u32 <= a_r + 1 && a.y as u32 <= b_b + 1 && b.y as u32 <= a_b + 1
}

fn bounding_box(a: FrameRect, b: FrameRect) -> FrameRect {
    let x = (a.x).min(b.x);
    let y = (a.y).min(b.y);
    let r = (a.x as u32 + a.width as u32).max(b.x as u32 + b.width as u32);
    let btm = (a.y as u32 + a.height as u32).max(b.y as u32 + b.height as u32);
    FrameRect {
        x,
        y,
        width: (r - x as u32) as u16,
        height: (btm - y as u32) as u16,
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

    fn rect(x: u16, y: u16, w: u16, h: u16) -> FrameRect {
        FrameRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn merge_overlapping_rects_into_bounding_box() {
        let merged = merge_rects(vec![rect(0, 0, 4, 4), rect(2, 2, 4, 4)]);
        assert_eq!(merged, vec![rect(0, 0, 6, 6)]);
    }

    #[test]
    fn merge_adjacent_rects() {
        // 水平相邻（间隔 ≤1）应合并。
        let merged = merge_rects(vec![rect(0, 0, 2, 2), rect(3, 0, 2, 2)]);
        assert_eq!(merged, vec![rect(0, 0, 5, 2)]);
    }

    #[test]
    fn keeps_disjoint_rects_separate() {
        let merged = merge_rects(vec![rect(0, 0, 2, 2), rect(10, 10, 2, 2)]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_chain_collapses_multiple() {
        let merged = merge_rects(vec![
            rect(0, 0, 2, 2),
            rect(2, 0, 2, 2),
            rect(4, 0, 2, 2),
            rect(50, 50, 1, 1),
        ]);
        assert_eq!(merged.len(), 2);
        assert!(merged.contains(&rect(0, 0, 6, 2)));
        assert!(merged.contains(&rect(50, 50, 1, 1)));
    }

    #[test]
    fn capabilities_report_local_scale_and_clipboard() {
        let caps = vnc_capabilities();
        assert_eq!(caps.resize, ResizeSupport::LocalScaleOnly);
        assert!(caps.clipboard_text);
    }
}
