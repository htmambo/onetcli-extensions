use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::protocol::FrameRect;
use crate::runtime::RemoteDesktopOutput;

pub struct OutputSender {
    shared: Arc<Shared>,
}

pub struct OutputReceiver {
    shared: Arc<Shared>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxClosed;

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

/// 累积的增量帧（合并去重，防止无界积压）。
struct PendingIncremental {
    width: u16,
    height: u16,
    rects: Vec<FrameRect>,
    bgra: Vec<u8>,
}

struct State {
    control: VecDeque<RemoteDesktopOutput>,
    latest_frame: Option<RemoteDesktopOutput>,
    pending_incremental: Option<PendingIncremental>,
    sender_count: usize,
    receiver_alive: bool,
}

pub fn output_mailbox() -> (OutputSender, OutputReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            control: VecDeque::new(),
            latest_frame: None,
            pending_incremental: None,
            sender_count: 1,
            receiver_alive: true,
        }),
        ready: Condvar::new(),
    });
    (
        OutputSender {
            shared: shared.clone(),
        },
        OutputReceiver { shared },
    )
}

impl OutputSender {
    pub fn send(&self, output: RemoteDesktopOutput) -> Result<(), MailboxClosed> {
        let mut state = lock(&self.shared);
        if !state.receiver_alive {
            return Err(MailboxClosed);
        }
        match output {
            // 整帧可去重（只保留最新）；整帧覆盖后废除已累积的增量帧（其内容已过时）。
            frame @ (RemoteDesktopOutput::Frame { .. } | RemoteDesktopOutput::FrameBgra { .. }) => {
                state.latest_frame = Some(frame);
                state.pending_incremental = None;
            }
            // 增量帧合并到单一 pending（防无界积压）。rects/bgra 按序 append，
            // 主程序按序 patch（后写覆盖先写），语义与逐帧发送等价。
            RemoteDesktopOutput::FrameRectsBgra {
                width,
                height,
                rects,
                bgra,
            } => match &mut state.pending_incremental {
                Some(p) if p.width == width && p.height == height => {
                    p.rects.extend(rects);
                    p.bgra.extend(bgra);
                    // 消费端异常缓慢时给出观测点（正常一个 tick 内取走清空）。
                    if p.bgra.len() > 32 * 1024 * 1024 {
                        tracing::warn!(
                            pending_len = p.bgra.len(),
                            "pending incremental frame too large; consumer may be stuck"
                        );
                    }
                }
                _ => {
                    state.pending_incremental = Some(PendingIncremental {
                        width,
                        height,
                        rects,
                        bgra,
                    });
                }
            },
            terminal @ (RemoteDesktopOutput::ConnectionFailure(_)
            | RemoteDesktopOutput::Terminated(_)) => {
                state.latest_frame = None;
                state.pending_incremental = None;
                state.control.push_back(terminal);
            }
            control => state.control.push_back(control),
        }
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
}

impl Clone for OutputSender {
    fn clone(&self) -> Self {
        lock(&self.shared).sender_count += 1;
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for OutputSender {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        state.sender_count = state.sender_count.saturating_sub(1);
        let closed = state.sender_count == 0;
        drop(state);
        if closed {
            self.shared.ready.notify_all();
        }
    }
}

impl OutputReceiver {
    pub fn recv(&self) -> Option<RemoteDesktopOutput> {
        let mut state = lock(&self.shared);
        loop {
            if let Some(control) = state.control.pop_front() {
                return Some(control);
            }
            if let Some(frame) = state.latest_frame.take() {
                return Some(frame);
            }
            if let Some(p) = state.pending_incremental.take() {
                return Some(RemoteDesktopOutput::FrameRectsBgra {
                    width: p.width,
                    height: p.height,
                    rects: p.rects,
                    bgra: p.bgra,
                });
            }
            if state.sender_count == 0 {
                return None;
            }
            state = self
                .shared
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        state.receiver_alive = false;
        state.control.clear();
        state.latest_frame = None;
        state.pending_incremental = None;
        drop(state);
        self.shared.ready.notify_all();
    }
}

impl fmt::Debug for OutputSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OutputSender").finish()
    }
}

impl fmt::Debug for OutputReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OutputReceiver").finish()
    }
}

impl fmt::Display for MailboxClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VNC helper output mailbox is closed")
    }
}

impl std::error::Error for MailboxClosed {}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RemoteDesktopOutput;

    #[test]
    fn keeps_only_latest_pending_frame() {
        let (tx, rx) = output_mailbox();
        tx.send(frame(1)).unwrap();
        tx.send(frame(2)).unwrap();
        tx.send(frame(3)).unwrap();

        assert_eq!(Some(frame(3)), rx.recv());
    }

    #[test]
    fn preserves_control_order_while_replacing_frames() {
        let (tx, rx) = output_mailbox();
        tx.send(RemoteDesktopOutput::Status("one".into())).unwrap();
        tx.send(frame(1)).unwrap();
        tx.send(RemoteDesktopOutput::ClipboardText { text: "two".into() })
            .unwrap();
        tx.send(frame(2)).unwrap();

        assert_eq!(Some(RemoteDesktopOutput::Status("one".into())), rx.recv());
        assert_eq!(
            Some(RemoteDesktopOutput::ClipboardText { text: "two".into() }),
            rx.recv()
        );
        assert_eq!(Some(frame(2)), rx.recv());
    }

    #[test]
    fn terminal_event_discards_pending_frame() {
        let (tx, rx) = output_mailbox();
        tx.send(frame(7)).unwrap();
        tx.send(RemoteDesktopOutput::Terminated("closed".into()))
            .unwrap();

        assert_eq!(
            Some(RemoteDesktopOutput::Terminated("closed".into())),
            rx.recv()
        );
        drop(tx);
        assert_eq!(None, rx.recv());
    }

    #[test]
    fn last_sender_drop_wakes_receiver() {
        let (tx, rx) = output_mailbox();
        let waiter = std::thread::spawn(move || rx.recv());

        drop(tx);

        assert_eq!(None, waiter.join().unwrap());
    }

    #[test]
    fn send_fails_after_receiver_is_dropped() {
        let (tx, rx) = output_mailbox();
        drop(rx);

        assert!(tx.send(frame(1)).is_err());
    }

    fn frame(value: u8) -> RemoteDesktopOutput {
        RemoteDesktopOutput::Frame {
            width: 1,
            height: 1,
            rgba: vec![value, 0, 0, 255],
        }
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> FrameRect {
        FrameRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn incr(rects: Vec<FrameRect>, bgra: Vec<u8>) -> RemoteDesktopOutput {
        RemoteDesktopOutput::FrameRectsBgra {
            width: 4,
            height: 4,
            rects,
            bgra,
        }
    }

    #[test]
    fn merges_consecutive_incremental_frames_into_one() {
        let (tx, rx) = output_mailbox();
        tx.send(incr(vec![rect(0, 0, 1, 1)], vec![1, 0, 0, 255])).unwrap();
        tx.send(incr(vec![rect(3, 3, 1, 1)], vec![2, 0, 0, 255])).unwrap();

        // 两个增量帧合并为一个（rects/bgra 按序拼接）。
        match rx.recv() {
            Some(RemoteDesktopOutput::FrameRectsBgra { rects, bgra, .. }) => {
                assert_eq!(rects, vec![rect(0, 0, 1, 1), rect(3, 3, 1, 1)]);
                assert_eq!(bgra, vec![1, 0, 0, 255, 2, 0, 0, 255]);
            }
            other => panic!("expected merged incremental frame, got {other:?}"),
        }
    }

    #[test]
    fn full_frame_discards_pending_incremental() {
        let (tx, rx) = output_mailbox();
        tx.send(incr(vec![rect(0, 0, 1, 1)], vec![1, 0, 0, 255])).unwrap();
        tx.send(frame(9)).unwrap(); // 整帧覆盖，废除 pending 增量

        assert_eq!(Some(frame(9)), rx.recv());
        drop(tx);
        assert_eq!(None, rx.recv()); // pending 增量已被废除，不会再发出
    }

    #[test]
    fn size_change_restarts_pending_incremental() {
        let (tx, rx) = output_mailbox();
        tx.send(RemoteDesktopOutput::FrameRectsBgra {
            width: 4,
            height: 4,
            rects: vec![rect(0, 0, 1, 1)],
            bgra: vec![1, 0, 0, 255],
        })
        .unwrap();
        // 尺寸不同的增量帧到来 → 旧的被替换（不混拼）。
        tx.send(RemoteDesktopOutput::FrameRectsBgra {
            width: 8,
            height: 8,
            rects: vec![rect(2, 2, 1, 1)],
            bgra: vec![2, 0, 0, 255],
        })
        .unwrap();

        match rx.recv() {
            Some(RemoteDesktopOutput::FrameRectsBgra {
                width,
                height,
                rects,
                bgra,
            }) => {
                assert_eq!((width, height), (8, 8));
                assert_eq!(rects, vec![rect(2, 2, 1, 1)]);
                assert_eq!(bgra, vec![2, 0, 0, 255]);
            }
            other => panic!("expected size-restarted incremental, got {other:?}"),
        }
    }

    #[test]
    fn control_event_between_incrementals_is_delivered_first() {
        let (tx, rx) = output_mailbox();
        tx.send(incr(vec![rect(0, 0, 1, 1)], vec![1, 0, 0, 255])).unwrap();
        tx.send(RemoteDesktopOutput::Status("s".into())).unwrap();

        assert_eq!(Some(RemoteDesktopOutput::Status("s".into())), rx.recv());
        assert!(matches!(
            rx.recv(),
            Some(RemoteDesktopOutput::FrameRectsBgra { .. })
        ));
    }
}
