//! 视口里的可点击 UI：布局、命中测试、以及「GDI 画到位图再当纹理贴回去」。
//!
//! 为什么不用原生控件（`CreateWindowEx` 建按钮 / `TrackMouseEvent`）：视口是
//! DX12 flip model 的交换链，任何 Win32 子窗口都会被 swapchain 覆盖——这是本任务
//! 一开始就排除的方案。所以按钮只能是**画的**：
//!
//! 1. [`Layout::compute`] 用真实字体度量算出每个控件在物理像素里的矩形；
//! 2. [`render`] 用 GDI 把背景、文字、hover / 按下态画进一张 32 位 DIB；
//! 3. 位图拷成 RGBA（不在任何控件矩形内的像素 alpha=0）交给 `gfx` 当纹理整屏合成；
//! 4. 鼠标事件用**同一份** [`Layout`] 做 CPU 矩形命中测试。
//!
//! 所以「点哪儿」和「亮哪儿」天生一致：两者读的是同一个矩形表。中文文字走 GDI
//! 的 `DrawTextW`，字体用微软雅黑，不依赖任何字形烘焙。

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// 几何与命中
// ---------------------------------------------------------------------------

/// 一块屏幕矩形（物理像素，原点左上）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

/// 工具条上的按键。顺序即绘制顺序（从左到右）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Import,
    Play,
    Loop,
    PrevAnim,
    NextAnim,
    /// 骨骼线框开关（默认关，按下后画骨架）。
    Bones,
    Panel,
    ExportAir,
    ExportLoader,
}

impl ButtonId {
    pub const ALL: [ButtonId; 9] = [
        ButtonId::Import,
        ButtonId::Play,
        ButtonId::Loop,
        ButtonId::PrevAnim,
        ButtonId::NextAnim,
        ButtonId::Bones,
        ButtonId::Panel,
        ButtonId::ExportAir,
        ButtonId::ExportLoader,
    ];
}

/// 一次命中测试的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Button(ButtonId),
    /// 工作区面板的第 `n` 条（绝对下标，含滚动偏移）。
    Row(usize),
    /// AIR 导出模式菜单的第 `n` 项（0=bin，1=json，2=两者）。
    PopupItem(usize),
}

/// 面板里一屏能显示的内容。
#[derive(Debug, Clone, Copy)]
pub struct PanelView<'a> {
    pub open: bool,
    pub scanning: bool,
    pub truncated: bool,
    pub scroll: usize,
    /// 资产相对路径。
    pub labels: &'a [String],
}

/// 布局输入：所有会改变控件尺寸或文字的状态都从这里进。
pub struct LayoutInput<'a> {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub playing: bool,
    pub looping: bool,
    /// 骨骼线框当前是否可见：按钮文字与高亮都读它。
    pub show_bones: bool,
    pub has_asset: bool,
    pub panel: PanelView<'a>,
    pub popup: bool,
    pub status: &'a str,
    pub status_bad: bool,
    pub timeline_info: &'a str,
    pub hover: Option<Hit>,
    pub pressed: Option<Hit>,
}

/// 一屏控件的矩形表。**绘制与命中测试共用这一份**，两边不会错位。
#[derive(Debug, Clone)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
    pub font_px: f32,
    pub small_px: f32,
    pub toolbar: Rect,
    pub buttons: Vec<(ButtonId, Rect, String)>,
    pub panel: Option<Rect>,
    pub panel_header: Option<Rect>,
    pub rows: Vec<(usize, Rect, String)>,
    pub visible_rows: usize,
    pub panel_footer: Option<String>,
    pub popup: Option<Rect>,
    pub popup_items: Vec<Rect>,
    pub status: Option<Rect>,
    pub hint: Option<Rect>,
    pub timeline_info: Rect,
    pub status_text: String,
    pub status_bad: bool,
    pub hover: Option<Hit>,
    pub pressed: Option<Hit>,
}

/// 估算一行文字的宽度（物理像素）。
///
/// 用估算而不是 `GetTextExtentPoint32W`：布局要在**没有 DC** 的场合（鼠标事件里）
/// 也能重算，而绘制与命中必须读到同一个值。系数按微软雅黑的平均字宽取——CJK 是
/// 全角 1.0em，数字/大写 0.62em，其余半个字身，误差在几个像素内，按钮留的内边距
/// 足以吸收。
pub fn text_width(text: &str, px: f32) -> f32 {
    let mut width = 0.0;
    for ch in text.chars() {
        let factor = if (ch as u32) >= 0x2e80 {
            1.0
        } else if ch == ' ' {
            0.34
        } else if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            0.62
        } else {
            0.52
        };
        width += factor * px;
    }
    width
}

/// 按钮文字随状态变：播放/暂停、循环开关、面板开合。
fn label_of(id: ButtonId, input: &LayoutInput) -> &'static str {
    match id {
        ButtonId::Import => "导入资产",
        ButtonId::Play => {
            if input.playing {
                "暂停"
            } else {
                "播放"
            }
        }
        ButtonId::Loop => {
            if input.looping {
                "循环：开"
            } else {
                "循环：关"
            }
        }
        ButtonId::PrevAnim => "上一动画",
        ButtonId::NextAnim => "下一动画",
        ButtonId::Bones => {
            // 文字直接说「现在是什么状态」，不用再去猜高亮色代表开还是关。
            if input.show_bones {
                "骨骼：开"
            } else {
                "骨骼：关"
            }
        }
        ButtonId::Panel => {
            if input.panel.open {
                "目录 ▲"
            } else {
                "目录 ▼"
            }
        }
        ButtonId::ExportAir => "导出 AIR",
        ButtonId::ExportLoader => "导出 .rs",
    }
}

impl Layout {
    /// 算一屏布局。
    pub fn compute(input: &LayoutInput) -> Layout {
        let scale = input.scale.max(0.5);
        let width = input.width as f32;
        let height = input.height as f32;
        let toolbar_h = 44.0 * scale;
        let toolbar = Rect::new(0.0, 0.0, width, toolbar_h);

        // 窗口窄到放不下时按比例缩字号与内边距，而不是让按钮跑到屏幕外。
        let mut k = 1.0_f32;
        let mut font_px = 14.0 * scale;
        let mut pad = 20.0 * scale;
        let mut gap = 6.0 * scale;
        for _ in 0..8 {
            font_px = 14.0 * scale * k;
            pad = 20.0 * scale * k;
            gap = 6.0 * scale * k;
            let mut total = gap;
            for id in ButtonId::ALL {
                total += text_width(label_of(id, input), font_px) + pad + gap;
            }
            if total <= width - 10.0 * scale {
                break;
            }
            k *= 0.9;
        }

        let btn_h = (toolbar_h - 10.0 * scale).max(18.0);
        let btn_y = (toolbar_h - btn_h) * 0.5;
        let mut buttons: Vec<(ButtonId, Rect, String)> = Vec::with_capacity(ButtonId::ALL.len());
        let mut x = gap;
        for id in ButtonId::ALL {
            let text = label_of(id, input).to_string();
            let w = text_width(&text, font_px) + pad;
            buttons.push((id, Rect::new(x, btn_y, w, btn_h), text));
            x += w + gap;
        }

        let small_px = (12.0 * scale).max(9.0);

        // ---- 工作区面板 ----
        let (panel, panel_header, rows, visible_rows, panel_footer) = if input.panel.open {
            let panel_w = (420.0 * scale).min(width * 0.92);
            let panel_h = ((height - toolbar_h) * 0.66).max(80.0);
            let panel_rect = Rect::new(0.0, toolbar_h, panel_w, panel_h);
            let header_h = 30.0 * scale;
            let header_rect = Rect::new(0.0, toolbar_h, panel_w, header_h);
            let row_h = 24.0 * scale;
            let body_top = toolbar_h + header_h;
            let body_h = (panel_h - header_h).max(0.0);
            let visible = ((body_h / row_h).floor() as usize).max(0);
            let total = input.panel.labels.len();
            let start = input.panel.scroll.min(total);
            let mut rows: Vec<(usize, Rect, String)> = Vec::new();
            for slot in 0..visible {
                let index = start + slot;
                if index >= total {
                    break;
                }
                rows.push((
                    index,
                    Rect::new(0.0, body_top + slot as f32 * row_h, panel_w, row_h),
                    input.panel.labels[index].clone(),
                ));
            }
            let footer = if input.panel.scanning {
                Some("扫描中…".to_string())
            } else if total == 0 {
                Some("没有找到 .glb / .gltf / .fbx".to_string())
            } else {
                let last = (start + rows.len()).min(total);
                let mut text = format!("{}-{} / {total}", start + 1, last);
                if input.panel.truncated {
                    text.push_str("（已达上限）");
                }
                Some(text)
            };
            (
                Some(panel_rect),
                Some(header_rect),
                rows,
                visible,
                footer,
            )
        } else {
            (None, None, Vec::new(), 0, None)
        };

        // ---- 导出菜单 ----
        let (popup, popup_items) = if input.popup {
            let anchor = buttons
                .iter()
                .find(|(id, _, _)| *id == ButtonId::ExportAir)
                .map(|(_, rect, _)| *rect);
            match anchor {
                Some(anchor) => {
                    let item_h = 28.0 * scale;
                    let popup_w = (250.0 * scale).min(width - anchor.x - 2.0);
                    let top = anchor.y + anchor.h + 4.0 * scale;
                    let rect = Rect::new(anchor.x, top, popup_w, item_h * 3.0 + 4.0 * scale);
                    let mut items = Vec::with_capacity(3);
                    for i in 0..3 {
                        items.push(Rect::new(
                            rect.x + 2.0 * scale,
                            rect.y + 2.0 * scale + i as f32 * item_h,
                            popup_w - 4.0 * scale,
                            item_h,
                        ));
                    }
                    (Some(rect), items)
                }
                None => (None, Vec::new()),
            }
        } else {
            (None, Vec::new())
        };

        // ---- 状态提示 / 空场景提示 / 时间轴说明 ----
        let status = if input.status.is_empty() {
            None
        } else {
            let w = (text_width(input.status, small_px) + 22.0 * scale).min(width - 20.0 * scale);
            Some(Rect::new(
                10.0 * scale,
                height - 96.0 * scale,
                w,
                22.0 * scale,
            ))
        };
        let hint = if input.has_asset {
            None
        } else {
            let text = "拖入模型，或按 O，或点左上角「导入资产」";
            let w = (text_width(text, font_px) + 28.0 * scale).min(width * 0.9);
            Some(Rect::new(
                (width - w) * 0.5,
                toolbar_h + 24.0 * scale,
                w,
                30.0 * scale,
            ))
        };
        let timeline_info = if input.timeline_info.is_empty() {
            Rect::default()
        } else {
            let w = (text_width(input.timeline_info, small_px) + 16.0 * scale)
                .min(width - 20.0 * scale);
            Rect::new(10.0 * scale, height - 68.0 * scale, w, 18.0 * scale)
        };

        Layout {
            width,
            height,
            scale,
            font_px,
            small_px,
            toolbar,
            buttons,
            panel,
            panel_header,
            rows,
            visible_rows,
            panel_footer,
            popup,
            popup_items,
            status,
            hint,
            timeline_info,
            status_text: input.status.to_string(),
            status_bad: input.status_bad,
            hover: input.hover,
            pressed: input.pressed,
        }
    }

    /// 命中测试。顺序按「谁压在上面」来：菜单 → 按钮 → 面板条目。
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        // 视口之外一律不命中：鼠标拖出窗口再松开、或高 DPI 下的负坐标，都不该算
        // 点中了某个按钮（`CursorMoved` 会把出界坐标原样带进来）。
        if !(0.0..=self.width).contains(&x) || !(0.0..=self.height).contains(&y) {
            return None;
        }
        for (index, rect) in self.popup_items.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Hit::PopupItem(index));
            }
        }
        // 菜单展开时点在菜单矩形内但落在间隙上：不算命中按钮，免得「点菜单缝里」
        // 顺手把菜单又切一次。
        if let Some(popup) = self.popup {
            if popup.contains(x, y) {
                return None;
            }
        }
        for (id, rect, _) in &self.buttons {
            if rect.contains(x, y) {
                return Some(Hit::Button(*id));
            }
        }
        for (index, rect, _) in &self.rows {
            if rect.contains(x, y) {
                return Some(Hit::Row(*index));
            }
        }
        None
    }

    /// 面板矩形是否覆盖某点：滚轮与「点空白处收起菜单」要用。
    pub fn panel_covers(&self, x: f32, y: f32) -> bool {
        self.panel.map(|r| r.contains(x, y)).unwrap_or(false)
    }
}

/// 导出菜单的三项。第 2 项会同时写出 `.air.bin` 与 `.air.json`。
pub const POPUP_ITEMS: [&str; 3] = [
    "AIR 二进制（.air.bin）",
    "AIR JSON（.air.json）",
    "两者都导（.air.bin + .json）",
];

// ---------------------------------------------------------------------------
// 配色（GDI 的 COLORREF 是 0x00BBGGRR，不是 RGB）
// ---------------------------------------------------------------------------

const TOOLBAR_BG: u32 = 0x0028_1e1a;
const TOOLBAR_LINE: u32 = 0x0056_423a;
const BTN_BG: u32 = 0x004e_3c34;
const BTN_HOVER: u32 = 0x0070_564a;
const BTN_PRESS: u32 = 0x0038_2a24;
const BTN_ACCENT: u32 = 0x00c0_7030;
const BTN_ACCENT_HOVER: u32 = 0x00dc_8840;
const TEXT: u32 = 0x00f4_ece8;
const TEXT_DIM: u32 = 0x00b0_a096;
const PANEL_BG: u32 = 0x0018_1412;
const PANEL_LINE: u32 = 0x0054_4038;
const ROW_HOVER: u32 = 0x0048_362e;
const ROW_CUR: u32 = 0x0050_423a;
const STATUS_BG: u32 = 0x002e_221c;
const OK_COLOR: u32 = 0x0096_c88c;
const BAD_COLOR: u32 = 0x0070_70e6;
const WARN_COLOR: u32 = 0x005a_aaf0;

// ---------------------------------------------------------------------------
// GDI 绘制
// ---------------------------------------------------------------------------

#[repr(C)]
struct RectI {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

fn rect_i(rect: Rect) -> RectI {
    RectI {
        left: rect.x.round() as i32,
        top: rect.y.round() as i32,
        right: (rect.x + rect.w).round() as i32,
        bottom: (rect.y + rect.h).round() as i32,
    }
}

#[repr(C)]
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
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

const DT_LEFT: u32 = 0x0000;
const DT_CENTER: u32 = 0x0001;
const DT_VCENTER: u32 = 0x0004;
const DT_SINGLELINE: u32 = 0x0020;
const DT_NOPREFIX: u32 = 0x0800;
const DT_END_ELLIPSIS: u32 = 0x8000;

const TRANSPARENT_BK: i32 = 1;
const ANTIALIASED_QUALITY: u32 = 4;
const DEFAULT_CHARSET: u32 = 1;

/// 一块「可画的内存位图」：32 位 DIB + 兼容 DC。
///
/// DIB 用 `biHeight = -h`（自上而下）：行序与屏幕一致，回传给 GPU 时不用翻转。
struct Canvas {
    dc: isize,
    dib: isize,
    old_bitmap: isize,
    bits: *mut u8,
    width: u32,
    height: u32,
    /// 建过就欠着的 GDI 对象（字体），Drop 时统一释放。
    owned: Vec<isize>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: width as i32,
                height: -(height as i32),
                planes: 1,
                bit_count: 32,
                compression: 0, // BI_RGB
                size_image: width * height * 4,
                x_pels: 0,
                y_pels: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        unsafe {
            let dib = CreateDIBSection(0, &info, 0, &mut bits, 0, 0);
            if dib == 0 || bits.is_null() {
                return None;
            }
            let dc = CreateCompatibleDC(0);
            if dc == 0 {
                DeleteObject(dib);
                return None;
            }
            let old_bitmap = SelectObject(dc, dib);
            SetBkMode(dc, TRANSPARENT_BK);
            Some(Self {
                dc,
                dib,
                old_bitmap,
                bits: bits as *mut u8,
                width,
                height,
                owned: Vec::new(),
            })
        }
    }

    fn fill(&self, rect: Rect, color: u32) {
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        let area = rect_i(rect);
        unsafe {
            let brush = CreateSolidBrush(color);
            if brush == 0 {
                return;
            }
            FillRect(self.dc, &area, brush);
            DeleteObject(brush);
        }
    }

    fn font(&mut self, px: f32, weight: i32) -> isize {
        let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
        let font = unsafe {
            CreateFontW(
                -(px.round() as i32).max(8),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                0,
                0,
                ANTIALIASED_QUALITY,
                0,
                face.as_ptr(),
            )
        };
        if font != 0 {
            self.owned.push(font);
        }
        font
    }

    fn text(&self, font: isize, rect: Rect, text: &str, color: u32, center: bool) {
        if font == 0 || text.is_empty() || rect.w <= 2.0 || rect.h <= 2.0 {
            return;
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut area = rect_i(rect);
        let format = DT_SINGLELINE
            | DT_VCENTER
            | DT_END_ELLIPSIS
            | DT_NOPREFIX
            | if center { DT_CENTER } else { DT_LEFT };
        unsafe {
            let previous = SelectObject(self.dc, font);
            SetTextColor(self.dc, color);
            DrawTextW(self.dc, units.as_ptr(), units.len() as i32, &mut area, format);
            SelectObject(self.dc, previous);
        }
    }

    /// 转成 RGBA：GDI 写的是 BGRA，这里换字节序；`masks` 之外一律 alpha=0。
    ///
    /// 透明度不用像素值推断（「黑色即透明」会把深色工具栏一起抠掉），而是由调用方
    /// 给出「画过东西的矩形」——UI 的每一块背景都是实心矩形，这个口径是准的。
    fn finish(self, masks: &[Rect]) -> Vec<u8> {
        let count = (self.width * self.height) as usize;
        let mut out = vec![0u8; count * 4];
        let src = unsafe { std::slice::from_raw_parts(self.bits, count * 4) };
        for index in 0..count {
            out[index * 4] = src[index * 4 + 2];
            out[index * 4 + 1] = src[index * 4 + 1];
            out[index * 4 + 2] = src[index * 4];
        }
        for mask in masks {
            let x0 = (mask.x.max(0.0).round() as u32).min(self.width);
            let y0 = (mask.y.max(0.0).round() as u32).min(self.height);
            let x1 = ((mask.x + mask.w).max(0.0).round() as u32).min(self.width);
            let y1 = ((mask.y + mask.h).max(0.0).round() as u32).min(self.height);
            for y in y0..y1 {
                for x in x0..x1 {
                    let offset = ((y as usize * self.width as usize) + x as usize) * 4 + 3;
                    out[offset] = 255;
                }
            }
        }
        out
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old_bitmap);
            DeleteDC(self.dc);
            DeleteObject(self.dib);
            for object in std::mem::take(&mut self.owned) {
                DeleteObject(object);
            }
        }
    }
}

/// 把一屏 UI 画成位图，返回 `(宽, 高, RGBA)`。
///
/// 返回 `None` 只发生在 GDI 建位图失败（极小概率的显存不足）：调用方保留上一帧
/// 的纹理即可，不该因此崩进程。
pub fn render(input: &LayoutInput, layout: &Layout) -> Option<(u32, u32, Vec<u8>)> {
    let mut canvas = Canvas::new(input.width, input.height)?;
    let scale = layout.scale;
    let mut masks: Vec<Rect> = Vec::new();

    // ---- 工具条 ----
    canvas.fill(layout.toolbar, TOOLBAR_BG);
    canvas.fill(
        Rect::new(
            0.0,
            layout.toolbar.h - scale.max(1.0),
            layout.width,
            scale.max(1.0),
        ),
        TOOLBAR_LINE,
    );
    masks.push(layout.toolbar);

    let font = canvas.font(layout.font_px, 400);
    let small = canvas.font(layout.small_px, 400);

    for (id, rect, text) in &layout.buttons {
        let pressed = layout.pressed == Some(Hit::Button(*id));
        let hovered = layout.hover == Some(Hit::Button(*id));
        let accent = *id == ButtonId::Import || (*id == ButtonId::Bones && input.show_bones);
        let background = if pressed {
            BTN_PRESS
        } else if hovered {
            if accent {
                BTN_ACCENT_HOVER
            } else {
                BTN_HOVER
            }
        } else if accent {
            BTN_ACCENT
        } else {
            BTN_BG
        };
        canvas.fill(*rect, background);
        // 按下时文字整体下移 1px：不需要额外边框也能看出「按下去了」。
        let text_rect = if pressed {
            Rect::new(rect.x, rect.y + scale, rect.w, rect.h)
        } else {
            *rect
        };
        canvas.text(font, text_rect, text, TEXT, true);
        masks.push(*rect);
    }

    // ---- 工作区面板 ----
    if let (Some(panel), Some(header)) = (layout.panel, layout.panel_header) {
        canvas.fill(panel, PANEL_BG);
        canvas.fill(
            Rect::new(panel.x, panel.y + panel.h - scale.max(1.0), panel.w, scale.max(1.0)),
            PANEL_LINE,
        );
        canvas.fill(header, ROW_CUR);
        let title = if input.panel.scanning {
            "工作区资产 · 扫描中".to_string()
        } else {
            format!(
                "工作区资产 · {} 项{}",
                input.panel.labels.len(),
                if input.panel.truncated { "（已截断）" } else { "" }
            )
        };
        canvas.text(
            font,
            Rect::new(
                header.x + 10.0 * scale,
                header.y,
                header.w - 20.0 * scale,
                header.h,
            ),
            &title,
            TEXT,
            false,
        );
        for (index, rect, label) in &layout.rows {
            let hovered = layout.hover == Some(Hit::Row(*index));
            if hovered {
                canvas.fill(*rect, ROW_HOVER);
                masks.push(*rect);
            }
            canvas.text(
                small,
                Rect::new(rect.x + 10.0 * scale, rect.y, rect.w - 16.0 * scale, rect.h),
                label,
                if hovered { TEXT } else { TEXT_DIM },
                false,
            );
        }
        // 页脚贴在面板底部，给滚动位置 / 空列表 / 截断提示用。
        if let Some(footer) = &layout.panel_footer {
            let foot_rect = Rect::new(
                panel.x + 10.0 * scale,
                panel.y + panel.h - 24.0 * scale,
                panel.w - 20.0 * scale,
                18.0 * scale,
            );
            canvas.fill(
                Rect::new(panel.x, panel.y + panel.h - 26.0 * scale, panel.w, 24.0 * scale),
                PANEL_BG,
            );
            canvas.text(
                small,
                foot_rect,
                footer,
                if layout.rows.is_empty() {
                    WARN_COLOR
                } else {
                    TEXT_DIM
                },
                false,
            );
        }
        masks.push(panel);
    }

    // ---- 导出菜单 ----
    if let Some(popup) = layout.popup {
        canvas.fill(popup, PANEL_BG);
        canvas.fill(
            Rect::new(popup.x, popup.y, popup.w, scale.max(1.0)),
            PANEL_LINE,
        );
        for (index, rect) in layout.popup_items.iter().enumerate() {
            let hovered = layout.hover == Some(Hit::PopupItem(index));
            if hovered {
                canvas.fill(*rect, ROW_HOVER);
            }
            canvas.text(
                font,
                Rect::new(rect.x + 10.0 * scale, rect.y, rect.w - 14.0 * scale, rect.h),
                POPUP_ITEMS[index],
                if hovered { TEXT } else { TEXT_DIM },
                false,
            );
        }
        masks.push(popup);
    }

    // ---- 时间轴说明（时间轴本体由 gfx 用 GPU 画，这里只补一行文字）----
    if layout.timeline_info.w > 0.0 {
        canvas.fill(layout.timeline_info, STATUS_BG);
        canvas.fill(
            Rect::new(
                layout.timeline_info.x,
                layout.timeline_info.y,
                scale,
                layout.timeline_info.h,
            ),
            PANEL_LINE,
        );
        canvas.text(
            small,
            Rect::new(
                layout.timeline_info.x + 8.0 * scale,
                layout.timeline_info.y,
                layout.timeline_info.w - 10.0 * scale,
                layout.timeline_info.h,
            ),
            input.timeline_info,
            TEXT_DIM,
            false,
        );
        masks.push(layout.timeline_info);
    }

    // ---- 状态提示 ----
    if let Some(status) = layout.status {
        canvas.fill(status, STATUS_BG);
        canvas.fill(
            Rect::new(status.x, status.y, scale.max(1.0), status.h),
            if layout.status_bad { BAD_COLOR } else { OK_COLOR },
        );
        canvas.text(
            small,
            Rect::new(
                status.x + 12.0 * scale,
                status.y,
                status.w - 14.0 * scale,
                status.h,
            ),
            &layout.status_text,
            if layout.status_bad { BAD_COLOR } else { OK_COLOR },
            false,
        );
        masks.push(status);
    }

    // ---- 空场景引导 ----
    if let Some(hint) = layout.hint {
        canvas.fill(hint, STATUS_BG);
        canvas.text(
            font,
            Rect::new(hint.x + 14.0 * scale, hint.y, hint.w - 20.0 * scale, hint.h),
            "拖入模型，或按 O，或点左上角「导入资产」",
            TEXT_DIM,
            false,
        );
        masks.push(hint);
    }

    let width = canvas.width;
    let height = canvas.height;
    Some((width, height, canvas.finish(&masks)))
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: isize) -> isize;
    fn DeleteDC(dc: isize) -> i32;
    fn CreateDIBSection(
        dc: isize,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: isize,
        offset: u32,
    ) -> isize;
    fn SelectObject(dc: isize, object: isize) -> isize;
    fn DeleteObject(object: isize) -> i32;
    fn CreateSolidBrush(color: u32) -> isize;
    #[allow(clippy::too_many_arguments)]
    fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike_out: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch_and_family: u32,
        face: *const u16,
    ) -> isize;
    fn SetTextColor(dc: isize, color: u32) -> u32;
    fn SetBkMode(dc: isize, mode: i32) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn FillRect(dc: isize, rect: *const RectI, brush: isize) -> i32;
    fn DrawTextW(dc: isize, text: *const u16, len: i32, rect: *mut RectI, format: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("assets/m{i}.glb")).collect()
    }

    fn sample(labels: &[String], panel_open: bool) -> LayoutInput<'_> {
        LayoutInput {
            width: 1280,
            height: 800,
            scale: 1.0,
            playing: false,
            looping: true,
            show_bones: false,
            has_asset: true,
            panel: PanelView {
                open: panel_open,
                scanning: false,
                truncated: false,
                scroll: 0,
                labels,
            },
            popup: false,
            status: "",
            status_bad: false,
            timeline_info: "",
            hover: None,
            pressed: None,
        }
    }

    #[test]
    fn buttons_fit_inside_window_and_do_not_overlap() {
        let names = labels(0);
        let layout = Layout::compute(&sample(&names, false));
        assert_eq!(layout.buttons.len(), ButtonId::ALL.len());
        let mut previous_right = 0.0;
        for (_, rect, _) in &layout.buttons {
            assert!(rect.x >= previous_right, "按钮重叠：{rect:?}");
            assert!(rect.x + rect.w <= layout.width, "按钮越界：{rect:?}");
            assert!(rect.y + rect.h <= layout.toolbar.h);
            previous_right = rect.x + rect.w;
        }
    }

    #[test]
    fn narrow_window_shrinks_buttons_instead_of_overflowing() {
        let names = labels(0);
        // 480 才是真放不下：中文标签按全角算，八个按钮在 640 下本来就排得开，
        // 那里还缩字号反而说明测量偏小。
        let mut narrow = sample(&names, false);
        narrow.width = 480;
        let layout = Layout::compute(&narrow);
        let last = layout.buttons.last().unwrap().1;
        assert!(last.x + last.w <= 480.0, "窄窗口下按钮越界：{last:?}");
        assert!(layout.font_px < 14.0, "应当缩小字号");
    }

    #[test]
    fn hit_test_prefers_popup_then_buttons_then_rows() {
        let names = labels(40);
        let mut with_popup = sample(&names, true);
        with_popup.popup = true;
        let layout = Layout::compute(&with_popup);

        let item = layout.popup_items[1];
        assert_eq!(
            layout.hit(item.x + 5.0, item.y + 5.0),
            Some(Hit::PopupItem(1))
        );

        let import = layout.buttons[0].1;
        assert_eq!(
            layout.hit(import.x + 3.0, import.y + 3.0),
            Some(Hit::Button(ButtonId::Import))
        );

        let row = layout.rows[0].1;
        assert_eq!(layout.hit(row.x + 5.0, row.y + 5.0), Some(Hit::Row(0)));

        assert_eq!(layout.hit(layout.width - 5.0, layout.height - 5.0), None);
    }

    #[test]
    fn panel_rows_respect_scroll_offset_and_visible_count() {
        let names = labels(40);
        let mut scrolled = sample(&names, true);
        scrolled.panel.scroll = 12;
        let layout = Layout::compute(&scrolled);
        assert_eq!(layout.rows[0].0, 12);
        assert_eq!(layout.rows[0].2, "assets/m12.glb");
        assert!(layout.rows.len() < 40);
        assert_eq!(layout.rows.len(), layout.visible_rows.min(40 - 12));
        // 面板不能盖住整屏：底部时间轴还得能点。
        let panel = layout.panel.unwrap();
        assert!(panel.y + panel.h < layout.height - 40.0);
    }

    #[test]
    fn labels_switch_with_state() {
        let names = labels(0);
        let mut state = sample(&names, true);
        state.playing = true;
        state.looping = false;
        let layout = Layout::compute(&state);
        let label = |id: ButtonId| {
            layout
                .buttons
                .iter()
                .find(|(button, _, _)| *button == id)
                .map(|(_, _, text)| text.clone())
                .unwrap()
        };
        assert_eq!(label(ButtonId::Play), "暂停");
        assert_eq!(label(ButtonId::Loop), "循环：关");
        assert_eq!(label(ButtonId::Panel), "目录 ▲");
        assert_eq!(label(ButtonId::Bones), "骨骼：关");
    }

    /// 骨骼按钮：默认「关」（骨架默认不画），点一下切成「开」，且命中矩形落在
    /// 工具条内、与其它按钮不重叠。
    #[test]
    fn bones_button_starts_hidden_and_toggles_label() {
        let names = labels(0);
        let off = sample(&names, false);
        assert!(!off.show_bones, "骨架默认必须是隐藏的");
        let off_layout = Layout::compute(&off);
        let bones = off_layout
            .buttons
            .iter()
            .find(|(id, _, _)| *id == ButtonId::Bones)
            .map(|(_, rect, text)| (*rect, text.clone()))
            .expect("工具条上必须有骨骼按钮");
        assert_eq!(bones.1, "骨骼：关");
        assert!(bones.0.y + bones.0.h <= off_layout.toolbar.h);
        assert_eq!(
            off_layout.hit(bones.0.x + 3.0, bones.0.y + 3.0),
            Some(Hit::Button(ButtonId::Bones))
        );

        let mut on = sample(&names, false);
        on.show_bones = true;
        let on_layout = Layout::compute(&on);
        let label = on_layout
            .buttons
            .iter()
            .find(|(id, _, _)| *id == ButtonId::Bones)
            .map(|(_, _, text)| text.clone())
            .unwrap();
        assert_eq!(label, "骨骼：开");
        // 开关只改文字、不改宽度：文字长度相同，布局矩形必须一模一样，否则每次
        // 切换整条工具条都会横向抖一下。
        let on_rect = on_layout
            .buttons
            .iter()
            .find(|(id, _, _)| *id == ButtonId::Bones)
            .map(|(_, rect, _)| *rect)
            .unwrap();
        assert_eq!(on_rect, bones.0);
    }

    #[test]
    fn text_width_counts_cjk_as_full_em() {
        assert!((text_width("导入", 20.0) - 40.0).abs() < 0.01);
        assert!(text_width("AIR", 20.0) < 40.0);
        assert_eq!(text_width("", 20.0), 0.0);
    }
}
