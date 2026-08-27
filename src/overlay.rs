//! 语言指示浮层:切换完成后,在屏幕中央短暂显示当前输入状态。
//!
//! 渲染:GDI+ 反锯齿绘制 32-bit 预乘 ARGB 位图,UpdateLayeredWindow 上屏,
//! 圆角与文字边缘均为逐像素 alpha,平滑无锯齿(AHK 同款效果)。
//! 淡出:每 tick 重绘带全局 alpha 的位图再提交。

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, SelectObject,
};
use windows::Win32::Graphics::GdiPlus::{
    FillModeAlternate, FontStyleBold, GdipAddPathArc, GdipClosePathFigure,
    GdipCreateBitmapFromScan0, GdipCreateFont, GdipCreateFontFamilyFromName, GdipCreatePath,
    GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont,
    GdipDeleteFontFamily, GdipDeleteGraphics, GdipDeletePath, GdipDeleteStringFormat,
    GdipDisposeImage, GdipDrawString, GdipFillPath, GdipGetImageGraphicsContext,
    GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdipSetStringFormatAlign,
    GdipSetStringFormatLineAlign, GdipSetTextRenderingHint, GdiplusStartup, GdiplusStartupInput,
    GpBrush, PixelOffsetModeHighQuality, RectF, SmoothingModeAntiAlias, Status,
    StringAlignmentCenter, TextRenderingHintAntiAlias, UnitPixel,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, KillTimer,
    MSG, PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SetTimer, SetWindowPos,
    ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_APP, WM_DESTROY, WM_QUIT, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::ime_toggle;

// ---- 可视参数 ----

/// 宽度 = 屏幕宽度的此比例
const W_RATIO: f32 = 0.06;
/// 高度 = 0.7 × 宽度
const H_RATIO: f32 = 0.7;
/// 文字高度 ≈ 此比例 × 胶囊高度
const TEXT_H_RATIO: f32 = 0.48;
/// 圆角半径
const RADIUS: f32 = 16.0;
/// 浮层中心相对屏幕中心的上移量(像素)
const LIFT_UP_PX: i32 = 120;
/// 超采样倍数:按 2x 尺寸绘制,提交前高质量下采样到 1x,
/// 边缘比单纯抗锯齿更细腻(2560x1440@100% 也看不出颗粒)。
const SSAA: i32 = 2;
/// 切换后等待输入法状态落定的延时
const READ_DELAY_MS: u32 = 10;
/// 完全显示的持续时间
const SHOW_MS: u32 = 300;
/// 淡出持续时间
const FADE_MS: u32 = 300;
/// 动画 tick 间隔
const TICK_MS: u32 = 16;

/// 全部尺寸度量:只由屏幕分辨率决定,任何机器上视觉比例一致。
struct Metrics {
    w: i32,
    h: i32,
    /// 文字 em 高度(像素)。GDI+ 字体的"高度"参数即 em size,
    /// 中日韩字形 em 高≈字高,按 TEXT_H_RATIO × h 取值即可。
    font_h: f32,
}

fn metrics() -> Metrics {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN) as f32;
        let w = (sw * W_RATIO).round() as i32;
        let h = (w as f32 * H_RATIO).round() as i32;
        let font_h = h as f32 * TEXT_H_RATIO;
        Metrics { w, h, font_h }
    }
}

// ---- 计时器 ID ----
const TIMER_READ: usize = 1;
const TIMER_ANIM: usize = 2;

/// 自定义消息:切换 IME 模式并显示浮层(全部动作在浮层线程执行)
const WM_APP_TOGGLE: u32 = WM_APP + 1;
/// 自定义消息:显示大小写状态(Alt+CapsLock 后)
const WM_APP_CAPS: u32 = WM_APP + 2;

// ---- 线程共享状态 ----

static OVERLAY_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static OVERLAY_TID: AtomicU32 = AtomicU32::new(0);
/// 动画起点(毫秒时钟);0 = 未在显示
static ANIM_START: AtomicU64 = AtomicU64::new(0);
static CUR_LANG: Mutex<LangDisplay> = Mutex::new(LANG_EN);

// ---- 状态定义 ----

/// 32-bit 颜色单通道掩码
const COLOR_CHANNEL_MASK: u32 = 0xFF;
/// GDI+ PixelFormat32bppPARGB(预乘 ARGB;crate 未导出此常量)
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0xE200B;
/// GDI+ Status::Ok
const GDIP_OK: Status = Status(0);

/// ARGB:高 8 位 alpha(绘制时恒 0xFF,淡出时整体乘系数),低 24 位 RGB
const fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (r << 16) | (g << 8) | b
}

/// "#RRGGBB" 数值字面量与 GDI+ ARGB 的 RGB 位序完全一致(R 在高位),
/// 直接使用即可——任何字节交换都会把 R/B 弄反。
const fn hex(rgb: u32) -> u32 {
    rgb
}

pub struct LangDisplay {
    pub label: &'static str,
    /// 胶囊底色(用户给的 hex)
    pub bg: u32,
    /// 文字颜色
    pub fg: u32,
}

pub const LANG_EN: LangDisplay = LangDisplay {
    label: "En",
    bg: hex(0x0073FF),
    fg: hex(0xF7F8FA),
};
pub const LANG_ZH: LangDisplay = LangDisplay {
    label: "中",
    bg: hex(0xFF1F45),
    fg: hex(0xF7F8FA),
};
pub const LANG_JA: LangDisplay = LangDisplay {
    label: "あ",
    bg: hex(0xFFBF00),
    fg: hex(0x212527),
};
pub const LANG_KO: LangDisplay = LangDisplay {
    label: "한",
    bg: hex(0x212527),
    fg: hex(0xF7F8FA),
};
pub const LANG_CAPS_ON: LangDisplay = LangDisplay {
    label: "AA",
    bg: hex(0x510068),
    fg: hex(0xF7F8FA),
};
pub const LANG_CAPS_OFF: LangDisplay = LangDisplay {
    label: "aa",
    bg: hex(0xD746FF),
    fg: hex(0xF7F8FA),
};

// ---- 对外接口 ----

pub fn spawn_thread() {
    std::thread::spawn(|| unsafe { message_loop() });
}

pub fn show_language_overlay() {
    post_show(WM_APP_TOGGLE);
}

pub fn show_caps_overlay(caps_on: bool) {
    let lparam = if caps_on { LPARAM(1) } else { LPARAM(0) };
    let p = OVERLAY_HWND.load(Ordering::SeqCst);
    if !p.is_null() {
        let _ = unsafe { PostMessageW(Some(HWND(p)), WM_APP_CAPS, WPARAM(0), lparam) };
    }
}

/// 通知浮层线程退出
pub fn post_quit() {
    let tid = OVERLAY_TID.load(Ordering::SeqCst);
    if tid != 0 {
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

fn post_show(msg: u32) {
    let p = OVERLAY_HWND.load(Ordering::SeqCst);
    if !p.is_null() {
        let _ = unsafe { PostMessageW(Some(HWND(p)), msg, WPARAM(0), LPARAM(0)) };
    }
}

// ---- 浮层线程 ----

unsafe fn message_loop() {
    OVERLAY_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

    unsafe { gdiplus_init() };

    let hinst = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW 失败");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: w!("CapsLockOverlayWnd"),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&wc) };
    debug_assert!(atom != 0, "RegisterClassW 失败");

    let hwnd = unsafe {
        CreateWindowExW(
            // 分层 + 点击穿透 + 置顶 + 不进任务栏 + 不抢焦点
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("CapsLockOverlayWnd"),
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        )
    }
    .expect("创建浮层窗口失败");

    OVERLAY_HWND.store(hwnd.0, Ordering::SeqCst);

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = DispatchMessageW(&mut msg);
        }
    }
    OVERLAY_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_TOGGLE => {
                // 先切(两级 API,失败回退 Ctrl+Space 注入),再延时检测显示。
                // 阻塞调用全在本线程执行——低级钩子回调里跑会超时摘钩。
                println!("[overlay] 收到切换请求");
                ime_toggle::toggle_or_fallback();
                on_request_show(hwnd);
                LRESULT(0)
            }
            WM_APP_CAPS => {
                let caps_on = lparam.0 != 0;
                let lang = if caps_on { LANG_CAPS_ON } else { LANG_CAPS_OFF };
                on_request_show_with(hwnd, lang);
                LRESULT(0)
            }
            WM_TIMER => {
                on_timer(hwnd, wparam.0);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 收到显示请求:延时一小段再检测显示(等 IME 落定)
unsafe fn on_request_show(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_READ);
        let _ = KillTimer(Some(hwnd), TIMER_ANIM);
        let _ = SetTimer(Some(hwnd), TIMER_READ, READ_DELAY_MS, None);
    }
}

/// 收到显示请求(内容已定,无需检测):直接显示
unsafe fn on_request_show_with(hwnd: HWND, lang: LangDisplay) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_READ);
        let _ = KillTimer(Some(hwnd), TIMER_ANIM);
        *CUR_LANG.lock().unwrap() = lang;
        layout_window(hwnd);
        render_frame(hwnd, 255);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        ANIM_START.store(GetTickCount64(), Ordering::SeqCst);
        let _ = SetTimer(Some(hwnd), TIMER_ANIM, TICK_MS, None);
    }
}

unsafe fn on_timer(hwnd: HWND, id: usize) {
    unsafe {
        match id {
            TIMER_READ => {
                let _ = KillTimer(Some(hwnd), TIMER_READ);
                // 重新检测一次(等 IME 落定后的真实状态)再上屏
                *CUR_LANG.lock().unwrap() = ime_toggle::detect_current_display();
                layout_window(hwnd);
                render_frame(hwnd, 255);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                ANIM_START.store(GetTickCount64(), Ordering::SeqCst);
                let _ = SetTimer(Some(hwnd), TIMER_ANIM, TICK_MS, None);
            }
            TIMER_ANIM => {
                let start = ANIM_START.load(Ordering::SeqCst);
                if start == 0 {
                    return;
                }
                let elapsed = (GetTickCount64() - start) as u32;
                let alpha = if elapsed < SHOW_MS {
                    255
                } else if elapsed < SHOW_MS + FADE_MS {
                    let t = elapsed - SHOW_MS;
                    (255u32 * (FADE_MS - t) / FADE_MS) as u8
                } else {
                    0
                };
                if alpha == 0 {
                    ANIM_START.store(0, Ordering::SeqCst);
                    let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else {
                    render_frame(hwnd, alpha);
                }
            }
            _ => {}
        }
    }
}

// ---- 渲染(GDI+ → 预乘 ARGB DIB → UpdateLayeredWindow) ----

/// 每帧重绘:胶囊底 + 文字,整体乘 alpha 后经 ULW 提交。
unsafe fn render_frame(hwnd: HWND, alpha: u8) {
    unsafe {
        let (label, bg_rgb, fg_rgb) = {
            let lang = CUR_LANG.lock().unwrap();
            (lang.label, lang.bg, lang.fg)
        };
        let m = metrics();
        let bg = argb(
            alpha as u32,
            (bg_rgb >> 16) & COLOR_CHANNEL_MASK,
            (bg_rgb >> 8) & COLOR_CHANNEL_MASK,
            bg_rgb & COLOR_CHANNEL_MASK,
        );
        let fg = argb(
            alpha as u32,
            (fg_rgb >> 16) & COLOR_CHANNEL_MASK,
            (fg_rgb >> 8) & COLOR_CHANNEL_MASK,
            fg_rgb & COLOR_CHANNEL_MASK,
        );
        draw_pill(hwnd, m.w, m.h, m.font_h, bg, fg, label);
    }
}

/// 已启动的 GDI+ token(进程级,浮层线程内使用)
static mut GDIPLUS_TOKEN: usize = 0;

unsafe fn gdiplus_init() {
    unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        let st = GdiplusStartup(&mut token, &input, std::ptr::null_mut());
        debug_assert!(st == GDIP_OK, "GdiplusStartup 失败: {st:?}");
        GDIPLUS_TOKEN = token;
    }
}

/// 在 32bpp DIB 上用 GDI+ 绘制圆角胶囊 + 居中文字,经 ULW 提交。
/// 内部按 SSAA 倍超采样绘制,再高质量缩到目标尺寸——
/// 圆角和文字边缘比 1x 直接抗锯齿更细腻。
unsafe fn draw_pill(hwnd: HWND, w: i32, h: i32, font_h: f32, bg: u32, fg: u32, label: &str) {
    unsafe {
        let big_w = w * SSAA;
        let big_h = h * SSAA;

        // 1. 建 32-bit 目标 DIB(预乘 ARGB),最终交给 ULW 的就是它
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut dst_bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut dst_bits, None, 0)
            .expect("CreateDIBSection 失败");
        // 全零 = 全透明起点
        std::ptr::write_bytes(dst_bits as *mut u8, 0, (w * h * 4) as usize);

        // 2. SSAA 倍大位图:GDI+ 直接画进这块内存
        let src_bits: Vec<u8> = vec![0u8; (big_w * big_h * 4) as usize];
        let mut bitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromScan0(
            big_w,
            big_h,
            big_w * 4,
            PIXEL_FORMAT_32BPP_PARGB,
            Some(src_bits.as_ptr()),
            &mut bitmap as *mut _ as *mut *mut _,
        );
        if st != GDIP_OK {
            eprintln!("[overlay] GdipCreateBitmapFromScan0 失败: {st:?}");
        }

        let mut graphics = std::ptr::null_mut();
        let _ = GdipGetImageGraphicsContext(bitmap as *mut _, &mut graphics);
        let _ = GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
        let _ = GdipSetPixelOffsetMode(graphics, PixelOffsetModeHighQuality);
        let _ = GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAlias);

        // 胶囊底(圆角矩形路径,坐标放大 SSAA 倍)
        let mut path = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut path);
        add_round_rect(
            path,
            0.0,
            0.0,
            big_w as f32,
            big_h as f32,
            RADIUS * SSAA as f32,
        );
        let mut brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(bg, &mut brush);
        let _ = GdipFillPath(graphics, brush as *mut GpBrush, path);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
        let _ = GdipDeletePath(path);

        // 居中文字(字号同样放大)
        let mut family = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );
        let mut font = std::ptr::null_mut();
        let _ = GdipCreateFont(
            family,
            font_h * SSAA as f32,
            FontStyleBold.0,
            UnitPixel,
            &mut font,
        );
        let mut fmt = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        let _ = GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
        let _ = GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
        let mut fg_brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(fg, &mut fg_brush);
        let layout = RectF {
            X: 0.0,
            Y: 0.0,
            Width: big_w as f32,
            Height: big_h as f32,
        };
        let text: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = GdipDrawString(
            graphics,
            PCWSTR(text.as_ptr()),
            -1,
            font,
            &layout,
            fmt,
            fg_brush as *mut GpBrush,
        );
        let _ = GdipDeleteBrush(fg_brush as *mut GpBrush);
        let _ = GdipDeleteStringFormat(fmt);
        let _ = GdipDeleteFont(font);
        let _ = GdipDeleteFontFamily(family);
        let _ = GdipDeleteGraphics(graphics);

        // 3. 超采样位图 → 高质量缩放到目标 DIB
        downsample_parbg(
            src_bits.as_ptr(),
            big_w,
            big_h,
            dst_bits as *mut u8,
            w,
            h,
            SSAA,
        );
        let _ = GdipDisposeImage(bitmap as *mut _);

        // 4. 目标位图已是预乘格式,选中后直接 ULW 提交
        let hdc = CreateCompatibleDC(None);
        let old = SelectObject(hdc, hbmp.into());
        let pt_src = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pt_dst = window_pos(w, h);
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&pt_dst),
            Some(&size),
            Some(hdc),
            Some(&pt_src),
            Default::default(),
            Some(&blend),
            ULW_ALPHA,
        );

        let _ = SelectObject(hdc, old);
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(hbmp.into());
    }
}

/// 盒式下采样 2x 预乘 ARGB 到 1x:每个目标像素 = 2x2 源像素平均。
/// 预乘格式下直接平均每分量即是正确的 alpha 混合。
unsafe fn downsample_parbg(
    src: *const u8,
    big_w: i32,
    _big_h: i32,
    dst: *mut u8,
    w: i32,
    h: i32,
    factor: i32,
) {
    // BGRA 小端字节序中的通道位移(B=0,G=8,R=16,A=24)
    const SHIFT_B: u32 = 0;
    const SHIFT_G: u32 = 8;
    const SHIFT_R: u32 = 16;
    const SHIFT_A: u32 = 24;

    unsafe {
        let f = factor as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let mut acc = [0u32; 4]; // 累加 [B, G, R, A]
                for dy in 0..f {
                    for dx in 0..f {
                        let sx = x * f + dx;
                        let sy = y * f + dy;
                        let s = src.offset((sy * big_w as usize + sx) as isize * 4) as *const u32;
                        let p = *s;
                        acc[0] += (p >> SHIFT_B) & COLOR_CHANNEL_MASK;
                        acc[1] += (p >> SHIFT_G) & COLOR_CHANNEL_MASK;
                        acc[2] += (p >> SHIFT_R) & COLOR_CHANNEL_MASK;
                        acc[3] += (p >> SHIFT_A) & COLOR_CHANNEL_MASK;
                    }
                }
                let n = (f * f) as u32;
                let d = dst.offset((y * w as usize + x) as isize * 4) as *mut u32;
                *d = ((acc[0] / n) << SHIFT_B)
                    | ((acc[1] / n) << SHIFT_G)
                    | ((acc[2] / n) << SHIFT_R)
                    | ((acc[3] / n) << SHIFT_A);
            }
        }
    }
}

/// 用四段圆弧拼圆角矩形路径
unsafe fn add_round_rect(
    path: *mut windows::Win32::Graphics::GdiPlus::GpPath,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
) {
    unsafe {
        let d = r * 2.0;
        // 四个角:左上、右上、右下、左下(顺时针)
        let _ = GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
        let _ = GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
        let _ = GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
        let _ = GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
        let _ = GdipClosePathFigure(path);
    }
}

/// 计算屏幕居中坐标
/// 计算窗口位置:水平居中,垂直方向在屏幕中心基础上再上移一段
fn window_pos(w: i32, h: i32) -> POINT {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        POINT {
            x: (sw - w) / 2,
            y: (sh - h) / 2 - LIFT_UP_PX,
        }
    }
}

/// 设定窗口位置与尺寸(尺寸由屏幕比例决定,与文字内容无关)
unsafe fn layout_window(hwnd: HWND) {
    unsafe {
        let m = metrics();
        let pos = window_pos(m.w, m.h);
        // SetWindowPos 触发 ULW 尺寸变化;位置每次都重设(屏幕可能变了)
        let _ = SetWindowPos(
            hwnd,
            None,
            pos.x,
            pos.y,
            m.w,
            m.h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

use windows::core::{PCWSTR, w};
