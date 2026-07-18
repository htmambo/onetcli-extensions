//! libvncclient 的安全封装层。
//!
//! 把对 `_rfbClient` 的 unsafe FFI 收敛到本模块，对上暴露安全 API。
//! 内存约定（关键，否则会 double-free / UB）：
//! - `GetCredential` 返回的 `rfbCredential` 及其 `username`/`password` 全部由
//!   libvncserver 通过 `FreeUserCredential` 用 `free()` 释放，因此回调内必须用
//!   `libc::malloc` 分配（结构体与两个字符串各自独立 malloc），不能给 Rust 指针。
//! - 会话上下文 `Ctx` 用 `Box::into_raw` 挂到 client data，在 `VncClient::drop`
//!   （`rfbClientCleanup` 之后）`Box::from_raw` 回收。
//! - `rfbInitClient` 失败时会自行 `rfbClientCleanup`，失败后 client 指针不可再用。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr::{self, NonNull};

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]
mod rfb {
    include!(concat!(env!("OUT_DIR"), "/rfb.rs"));
}

/// 一次脏矩形更新。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// 挂在 client data 上的会话上下文，由 C 回调写入、由事件循环读取。
pub(crate) struct Ctx {
    username: CString,
    password: CString,
    /// 自上次取走后累积的脏矩形。
    dirty: Vec<DirtyRect>,
    /// 服务端推送的剪贴板文本（UTF-8）。
    clipboard: Vec<String>,
    /// 是否已收到首帧（SetResolution 等价事件）。
    saw_framebuffer: bool,
}

impl Ctx {
    fn new(username: &str, password: &str) -> Self {
        Self {
            username: CString::new(username).unwrap_or_default(),
            password: CString::new(password).unwrap_or_default(),
            dirty: Vec::new(),
            clipboard: Vec::new(),
            saw_framebuffer: false,
        }
    }
}

/// 用 `libc::malloc` 复制一个 C 字符串（供 libvncserver 之后 `free`）。
unsafe fn malloc_cstr(src: &CString) -> *mut c_char {
    unsafe {
        let bytes = src.as_bytes_with_nul();
        let ptr = libc::malloc(bytes.len()) as *mut c_char;
        if !ptr.is_null() {
            ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, ptr, bytes.len());
        }
        ptr
    }
}

unsafe extern "C" fn get_credential(
    client: *mut rfb::rfbClient,
    _credential_type: c_int,
) -> *mut rfb::rfbCredential {
    unsafe {
        let ctx = rfb::rfbClientGetClientData(client, ptr::null_mut()) as *mut Ctx;
        let Some(ctx) = ctx.as_ref() else {
            return ptr::null_mut();
        };
        let cred =
            libc::malloc(std::mem::size_of::<rfb::rfbCredential>()) as *mut rfb::rfbCredential;
        if cred.is_null() {
            return ptr::null_mut();
        }
        (*cred).userCredential.username = malloc_cstr(&ctx.username);
        (*cred).userCredential.password = malloc_cstr(&ctx.password);
        cred
    }
}

unsafe extern "C" fn got_fb_update(
    client: *mut rfb::rfbClient,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
) {
    unsafe {
        let ctx = rfb::rfbClientGetClientData(client, ptr::null_mut()) as *mut Ctx;
        if let Some(ctx) = ctx.as_mut() {
            ctx.saw_framebuffer = true;
            ctx.dirty.push(DirtyRect {
                x: x.max(0) as u16,
                y: y.max(0) as u16,
                width: w.max(0) as u16,
                height: h.max(0) as u16,
            });
        }
    }
}

unsafe extern "C" fn got_xcut_text(
    client: *mut rfb::rfbClient,
    text: *const c_char,
    textlen: c_int,
) {
    unsafe {
        let ctx = rfb::rfbClientGetClientData(client, ptr::null_mut()) as *mut Ctx;
        let Some(ctx) = ctx.as_mut() else {
            return;
        };
        if text.is_null() || textlen <= 0 {
            return;
        }
        let bytes = std::slice::from_raw_parts(text as *const u8, textlen as usize);
        ctx.clipboard.push(String::from_utf8_lossy(bytes).into_owned());
    }
}

/// libvncclient 连接的安全封装。非 `Send`/`Sync`——必须只在创建它的线程使用。
pub struct VncClient {
    client: NonNull<rfb::rfbClient>,
    ctx: *mut Ctx,
}

impl VncClient {
    /// 连接并完成 RFB 握手与安全协商（支持 ARD/VncAuth/TLS 等，由服务端选择）。
    pub fn connect(
        destination: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> anyhow::Result<Self> {
        let (host, port) = parse_destination(destination)?;
        let host_c = CString::new(host.as_str()).map_err(|_| anyhow::anyhow!("非法主机名"))?;

        // ctx 必须 into_raw 交出所有权——否则局部 Box 在 connect 返回时被 drop，
        // ctx_ptr 立即悬垂，后续 C 回调 / take_dirty 访问即 UB（会导致卡死/崩溃）。
        let ctx_ptr = Box::into_raw(Box::new(Ctx::new(
            username.unwrap_or_default(),
            password.unwrap_or_default(),
        )));

        unsafe {
            let client = rfb::rfbGetClient(8, 3, 4); // 32bpp RGBA
            if client.is_null() {
                anyhow::bail!("rfbGetClient 分配失败");
            }
            // serverHost 必须由 libvncserver 可 free 的内存持有——rfbClientCleanup 会
            // free(serverHost)。用 libc::strdup 分配并把所有权交给它，不能给 Rust 指针。
            (*client).serverHost = libc::strdup(host_c.as_ptr());
            if (*client).serverHost.is_null() {
                libc::free(client as *mut c_void);
                drop(Box::from_raw(ctx_ptr));
                anyhow::bail!("serverHost 分配失败");
            }
            (*client).serverPort = port as c_int;
            (*client).GetCredential = Some(get_credential);
            (*client).GotFrameBufferUpdate = Some(got_fb_update);
            // 用标准 GotXCutText 而非 GotXCutTextUTF8：后者会让 SetFormatAndEncodings
            // 附加 ExtendedClipboard 伪编码，ARD 收到后不再推送普通帧（实测稳定复现
            // 收不到首帧）。标准 rfbServerCutText 回调不影响编码协商，剪贴板正常。
            (*client).GotXCutText = Some(got_xcut_text);
            // 编码优先级：ZRLE/Tight 压缩优先，降低服务端→客户端的网络与解码开销。
            // 注意：libvncclient 按【空格】分隔解析 encodingsString（strchr ' '），
            // 用逗号会被当成单个编码名导致 "Unknown encoding"、编码协商失败黑屏。
            // 用 'static 字面量，生命周期贯穿整个进程，libvncclient 只读不接管。
            (*client).appData.encodingsString = c"zrle tight copyrect hextile raw".as_ptr();
            rfb::rfbClientSetClientData(client, ptr::null_mut(), ctx_ptr as *mut c_void);

            let ok = rfb::rfbInitClient(client, ptr::null_mut(), ptr::null_mut());
            if ok == 0 {
                // rfbInitClient 失败时已自行 rfbClientCleanup（含 free serverHost），
                // 直接回收 ctx，不要再触碰 client 指针。
                drop(Box::from_raw(ctx_ptr));
                anyhow::bail!("VNC 握手或认证失败（{destination}）");
            }

            let client = NonNull::new(client).unwrap();
            Ok(Self {
                client,
                ctx: ctx_ptr,
            })
        }
    }

    /// 桌面尺寸（宽, 高）。
    pub fn size(&self) -> (u16, u16) {
        unsafe {
            let c = self.client.as_ref();
            (c.width.max(0) as u16, c.height.max(0) as u16)
        }
    }

    /// 桌面名称（若服务端提供）。预留给状态展示，暂未消费。
    #[allow(dead_code)]
    pub fn desktop_name(&self) -> Option<String> {
        unsafe {
            let name = self.client.as_ref().desktopName;
            if name.is_null() {
                None
            } else {
                Some(CStr::from_ptr(name).to_string_lossy().into_owned())
            }
        }
    }

    /// 当前帧缓冲（RGBA，长度 = width*height*4）。未分配时为空切片。
    pub fn framebuffer(&self) -> &[u8] {
        unsafe {
            let c = self.client.as_ref();
            if c.frameBuffer.is_null() || c.width <= 0 || c.height <= 0 {
                return &[];
            }
            let len = (c.width as usize) * (c.height as usize) * 4;
            std::slice::from_raw_parts(c.frameBuffer, len)
        }
    }

    /// 等待服务端消息。返回 <0 表示连接已断开。
    pub fn wait_for_message(&mut self, usecs: u32) -> i32 {
        unsafe { rfb::WaitForMessage(self.client.as_ptr(), usecs) }
    }

    /// 处理一条服务端消息（会同步触发帧/剪贴板回调）。false 表示断开。
    pub fn handle_message(&mut self) -> bool {
        unsafe { rfb::HandleRFBServerMessage(self.client.as_ptr()) != 0 }
    }

    /// 请求全屏刷新（incremental=false 强制全量）。预留给手动刷新/重连场景，暂未消费。
    #[allow(dead_code)]
    pub fn request_refresh(&mut self, incremental: bool) {
        let (w, h) = self.size();
        unsafe {
            rfb::SendFramebufferUpdateRequest(
                self.client.as_ptr(),
                0,
                0,
                w as c_int,
                h as c_int,
                incremental as rfb::rfbBool,
            );
        }
    }

    pub fn send_key(&mut self, keysym: u32, down: bool) {
        unsafe {
            rfb::SendKeyEvent(self.client.as_ptr(), keysym, down as rfb::rfbBool);
        }
    }

    pub fn send_pointer(&mut self, x: u16, y: u16, button_mask: u8) {
        unsafe {
            rfb::SendPointerEvent(
                self.client.as_ptr(),
                x as c_int,
                y as c_int,
                button_mask as c_int,
            );
        }
    }

    pub fn send_cut_text(&mut self, text: &str) {
        let Ok(text) = CString::new(text) else {
            return;
        };
        let len = text.as_bytes().len() as c_int;
        unsafe {
            rfb::SendClientCutTextUTF8(self.client.as_ptr(), text.as_ptr() as *mut c_char, len);
        }
    }

    /// 取走自上次以来累积的脏矩形。
    pub(crate) fn take_dirty(&mut self) -> Vec<DirtyRect> {
        unsafe { std::mem::take(&mut (*self.ctx).dirty) }
    }

    /// 取走服务端推送的剪贴板文本。
    pub(crate) fn take_clipboard(&mut self) -> Vec<String> {
        unsafe { std::mem::take(&mut (*self.ctx).clipboard) }
    }

    /// 是否已收到首帧。
    pub(crate) fn saw_framebuffer(&self) -> bool {
        unsafe { (*self.ctx).saw_framebuffer }
    }
}

impl Drop for VncClient {
    fn drop(&mut self) {
        // 连接成功才构造 Self，这里总是 cleanup。
        // rfbClientCleanup 会 free serverHost / frameBuffer / client 等。
        unsafe { rfb::rfbClientCleanup(self.client.as_ptr()) };
        unsafe { drop(Box::from_raw(self.ctx)) };
    }
}

/// 解析 `host[:port]`，缺省端口 5900。
fn parse_destination(destination: &str) -> anyhow::Result<(String, u16)> {
    let destination = destination.trim();
    anyhow::ensure!(!destination.is_empty(), "目标地址为空");
    if let Some((host, port)) = destination.rsplit_once(':') {
        // IPv6 形如 [::1]:5900
        if let Ok(port) = port.parse::<u16>() {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            return Ok((host.to_string(), port));
        }
    }
    Ok((destination.to_string(), 5900))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_destination_defaults_port_5900() {
        assert_eq!(
            parse_destination("192.168.50.205").unwrap(),
            ("192.168.50.205".to_string(), 5900)
        );
    }

    #[test]
    fn parse_destination_reads_explicit_port() {
        assert_eq!(
            parse_destination("192.168.50.205:5901").unwrap(),
            ("192.168.50.205".to_string(), 5901)
        );
    }

    #[test]
    fn parse_destination_strips_ipv6_brackets() {
        assert_eq!(
            parse_destination("[::1]:5900").unwrap(),
            ("::1".to_string(), 5900)
        );
    }

    #[test]
    fn parse_destination_rejects_empty() {
        assert!(parse_destination("  ").is_err());
    }
}
