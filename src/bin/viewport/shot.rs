//! 窗口自截图：自测留证用，不依赖外部脚本。
//!
//! 为什么不用 PowerShell / Python 抓屏：自测是一条**进程内**的事件链（合成点击 →
//! 载入完成 → 立刻截图），中间隔一层外部进程就会错过目标帧；而且自测要能在没有
//! 交互桌面的机器上跑，多一个依赖就多一个失败点。
//!
//! 抓法按可靠性排序，逐级降级，任何一级拿到「不是纯色」的图就收工：
//!
//! 1. `PrintWindow` + `PW_RENDERFULLCONTENT(2)`：DWM 合成后的窗口内容，最快；
//! 2. `PrintWindow` + `PW_CLIENTONLY(1)`：老式 GDI 路径，部分驱动上比 2 更灵；
//! 3. 屏幕 DC 上 `BitBlt` 窗口客户区（先 `ClientToScreen` 换算）：wgpu 的 flip-model
//!    交换链不进重定向表面时，只有这条路能拿到真实像素。
//!
//! 「纯色」判据是抽样方差：wgpu 抓不到内容时给出的是一整块同色（黑或白），
//! 与「画面真的有东西」区分得开。

use std::path::Path;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels: i32,
    y_pels: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

const SRCCOPY: u32 = 0x00CC_0020;
const DIB_RGB_COLORS: u32 = 0;
const PW_CLIENTONLY: u32 = 0x1;
const PW_RENDERFULLCONTENT: u32 = 0x2;

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: isize) -> isize;
    fn GetWindowDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, dc: isize) -> i32;
    fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn ClientToScreen(hwnd: isize, point: *mut Point) -> i32;
    fn PrintWindow(hwnd: isize, dc: isize, flags: u32) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: isize) -> isize;
    fn CreateCompatibleBitmap(dc: isize, width: i32, height: i32) -> isize;
    fn SelectObject(dc: isize, obj: isize) -> isize;
    fn DeleteObject(obj: isize) -> i32;
    fn DeleteDC(dc: isize) -> i32;
    fn BitBlt(
        dst: isize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        src: isize,
        src_x: i32,
        src_y: i32,
        rop: u32,
    ) -> i32;
    fn GetDIBits(
        dc: isize,
        bitmap: isize,
        start: u32,
        lines: u32,
        bits: *mut u8,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
}

/// 一次截图的产物。
#[derive(Debug, Clone)]
pub struct Shot {
    pub width: u32,
    pub height: u32,
    /// 实际生效的抓法（写进自测报告，便于排查「截出来是黑的」）。
    pub method: &'static str,
}

/// 抓 `hwnd` 的客户区并写成 PNG。
///
/// 返回 `Err` 的三种情况：客户区尺寸为 0（窗口还没映射）、三条路径全部拿不到像素、
/// PNG 落盘失败。三种都要在报告里说清楚，不能让「没有截图」被当成「截图成功」。
pub fn capture_client(hwnd: isize, target: &Path) -> Result<Shot, String> {
    if hwnd == 0 {
        return Err("窗口句柄无效（0）".to_string());
    }
    let mut rect = Rect::default();
    // SAFETY: 只传本进程窗口句柄与栈上结构体，RECT 生命周期覆盖调用。
    unsafe {
        if GetClientRect(hwnd, &mut rect) == 0 {
            return Err("GetClientRect 失败".to_string());
        }
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(format!("客户区尺寸无效：{width}x{height}"));
    }

    let mut last_err = String::new();
    for method in ["PrintWindow:full", "PrintWindow:client", "screen:blit"] {
        match grab(hwnd, width, height, method) {
            Ok((rgba, uniform)) => {
                if uniform {
                    last_err = format!("{method} 抓到纯色图（内容没上屏？）");
                    continue;
                }
                let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
                    .ok_or_else(|| "像素缓冲尺寸不符".to_string())?;
                image
                    .save(target)
                    .map_err(|err| format!("写 PNG {} 失败：{err}", target.display()))?;
                return Ok(Shot {
                    width: width as u32,
                    height: height as u32,
                    method,
                });
            }
            Err(err) => last_err = format!("{method}：{err}"),
        }
    }
    Err(last_err)
}

/// 按指定抓法取一张 RGBA 图；`uniform` 标记「整张同色」，供上层决定是否降级。
fn grab(hwnd: isize, width: i32, height: i32, method: &str) -> Result<(Vec<u8>, bool), String> {
    // SAFETY: 下列调用全部带成对释放：DC 用 DeleteDC/ReleaseDC，位图用 DeleteObject，
    // 且 SelectObject 的返回值（默认位图）在删除前恢复回去。
    unsafe {
        let src_dc = if method == "screen:blit" {
            GetDC(0)
        } else {
            GetWindowDC(hwnd)
        };
        if src_dc == 0 {
            return Err("取窗口 DC 失败".to_string());
        }
        let mem_dc = CreateCompatibleDC(src_dc);
        if mem_dc == 0 {
            ReleaseDC(hwnd, src_dc);
            return Err("CreateCompatibleDC 失败".to_string());
        }
        let bitmap = CreateCompatibleBitmap(src_dc, width, height);
        if bitmap == 0 {
            DeleteDC(mem_dc);
            ReleaseDC(hwnd, src_dc);
            return Err("CreateCompatibleBitmap 失败".to_string());
        }
        let old = SelectObject(mem_dc, bitmap);

        let ok = match method {
            "PrintWindow:full" => PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT) != 0,
            "PrintWindow:client" => PrintWindow(hwnd, mem_dc, PW_CLIENTONLY) != 0,
            _ => {
                // 屏幕 DC：客户区左上角换算成屏幕坐标再整块搬。
                let mut origin = Point::default();
                if ClientToScreen(hwnd, &mut origin) == 0 {
                    false
                } else {
                    BitBlt(mem_dc, 0, 0, width, height, src_dc, origin.x, origin.y, SRCCOPY) != 0
                }
            }
        };
        if !ok {
            SelectObject(mem_dc, old);
            DeleteObject(bitmap);
            DeleteDC(mem_dc);
            ReleaseDC(hwnd, src_dc);
            return Err("抓取调用返回失败".to_string());
        }

        let mut info = BitmapInfo::default();
        info.header.size = std::mem::size_of::<BitmapInfoHeader>() as u32;
        info.header.width = width;
        // 负高度 = 自上而下，省一次翻转；老驱动对负高度支持一致，这里自古用之。
        info.header.height = -height;
        info.header.planes = 1;
        info.header.bit_count = 32;
        info.header.compression = 0; // BI_RGB
        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        let lines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            rgba.as_mut_ptr(),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem_dc, old);
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(hwnd, src_dc);
        if lines == 0 {
            return Err("GetDIBits 失败".to_string());
        }

        // DIB 是 BGRA，转成 RGBA；alpha 一律写 255（窗口内容没有可信 alpha）。
        let mut uniform = true;
        let first = [rgba[0], rgba[1], rgba[2]];
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
            if uniform && (px[0] != first[0] || px[1] != first[1] || px[2] != first[2]) {
                uniform = false;
            }
        }
        Ok((rgba, uniform))
    }
}
