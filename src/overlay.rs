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
    GdipDisposeImage, GdipDrawString, GdipFillPath, GdipGetImageGraphicsContext, GdipMeasureString,
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

/// 文字字号(像素)
const FONT_SIZE_PX: f32 = 24.0;
const PAD_X: i32 = 22;
const PAD_Y: i32 = 12;
/// 圆角半径
const RADIUS: f32 = 14.0;
/// 切换后等待输入法状态落定的延时
const READ_DELAY_MS: u32 = 20;
/// 完全显示的持续时间
const SHOW_MS: u32 = 300;
/// 淡出持续时间
const FADE_MS: u32 = 300;
/// 动画 tick 间隔
const TICK_MS: u32 = 16;

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

/// ARGB:高 8 位 alpha(绘制时恒 0xFF,淡出时整体乘系数),低 24 位 RGB
const fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (r << 16) | (g << 8) | b
}

const fn hex(rgb: u32) -> u32 {
    // "#RRGGBB" → GDI+ 的 ARGB 字节序(R 高位)
    ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
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
    label: "大写",
    bg: hex(0x510068),
    fg: hex(0xF7F8FA),
};
pub const LANG_CAPS_OFF: LangDisplay = LangDisplay {
    label: "小写",
    bg: hex(0xD746FF),
    fg: hex(0xF7F8FA),
};

// ---- 对外接口 ----

/// 启动浮层窗口线程(常驻,负责显示/淡出/隐藏)
pub fn spawn_thread() {
    std::thread::spawn(|| unsafe { message_loop() });
}

/// 请求切换 IME 模式并显示浮层。可在钩子线程调用:仅投递一条消息,立即返回。
/// 切换(含 AttachThreadInput/SendMessageTimeout 等阻塞调用)在浮层线程执行,
/// 低级钩子回调里绝不能跑这些——超时会被系统静默摘除钩子。
pub fn show_language_overlay() {
    post_show(WM_APP_TOGGLE);
}

/// 请求显示大小写浮层(Alt+CapsLock 切换后)。
/// `caps_on`:当前是否处于大写锁定。
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
/// 注意:先取出 (label,bg,fg) 再放锁,内部不再触碰 CUR_LANG,
/// 否则 current_size() 二次加锁会自死锁(浮层线程直接冻住)。
unsafe fn render_frame(hwnd: HWND, alpha: u8) {
    unsafe {
        let (label, bg_rgb, fg_rgb, w, h) = {
            let lang = CUR_LANG.lock().unwrap();
            let (w, h) = measure_cached(lang.label);
            (lang.label, lang.bg, lang.fg, w, h)
        };
        let bg = argb(alpha as u32, (bg_rgb >> 16) & 0xFF, (bg_rgb >> 8) & 0xFF, bg_rgb & 0xFF);
        let fg = argb(alpha as u32, (fg_rgb >> 16) & 0xFF, (fg_rgb >> 8) & 0xFF, fg_rgb & 0xFF);
        draw_pill(hwnd, w, h, bg, fg, label);
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
        debug_assert!(st == Status(0), "GdiplusStartup 失败: {st:?}");
        GDIPLUS_TOKEN = token;
    }
}

/// 在 32bpp DIB 上用 GDI+ 绘制圆角胶囊 + 居中文字,经 ULW 提交。
unsafe fn draw_pill(hwnd: HWND, w: i32, h: i32, bg: u32, fg: u32, label: &str) {
    unsafe {
        // 1. 建 32-bit DIB(预乘 ARGB)
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
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection 失败");
        // 全零 = 全透明起点
        std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);

        let hdc = CreateCompatibleDC(None);
        let old = SelectObject(hdc, hbmp.into());

        // 2. GDI+ 绘制 —— 关键:scan0 必须指向 DIB 的内存,
        //    否则 GDI+ 画进自己的内部缓冲,DIB 保持全零(全透明),
        //    ULW 提交的就是一张看不见的空图。
        let mut bitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            0xE200B, /* PixelFormat32bppPARGB */
            Some(bits as *const u8),
            &mut bitmap as *mut _ as *mut *mut _,
        );
        if st != Status(0) {
            eprintln!("[overlay] GdipCreateBitmapFromScan0 失败: {st:?}");
        }

        let mut graphics = std::ptr::null_mut();
        let _ = GdipGetImageGraphicsContext(bitmap as *mut _, &mut graphics);
        let _ = GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
        let _ = GdipSetPixelOffsetMode(graphics, PixelOffsetModeHighQuality);
        let _ = GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAlias);

        // 胶囊底(圆角矩形路径)
        let mut path = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut path);
        add_round_rect(path, 0.0, 0.0, w as f32, h as f32, RADIUS);
        let mut brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(bg, &mut brush);
        let _ = GdipFillPath(graphics, brush as *mut GpBrush, path);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
        let _ = GdipDeletePath(path);

        // 居中文字
        let mut family = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );
        let mut font = std::ptr::null_mut();
        let _ = GdipCreateFont(family, FONT_SIZE_PX, FontStyleBold.0, UnitPixel, &mut font);
        let mut fmt = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        let _ = GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
        let _ = GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
        let mut fg_brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(fg, &mut fg_brush);
        let layout = RectF {
            X: 0.0,
            Y: 0.0,
            Width: w as f32,
            Height: h as f32,
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
        let _ = GdipDisposeImage(bitmap as *mut _);

        // 3. 位图已是预乘格式,直接 ULW 提交
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
fn window_pos(w: i32, h: i32) -> POINT {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        POINT {
            x: (sw - w) / 2,
            y: (sh - h) / 2,
        }
    }
}

// ---- 尺寸测量(带缓存,避免每帧建 GDI+ 对象) ----

static SIZE_CACHE: Mutex<Vec<(&'static str, i32, i32)>> = Mutex::new(Vec::new());

fn measure_cached(label: &str) -> (i32, i32) {
    // 标签集合极小(En/中/あ/한/大写/小写),直接线性查
    let mut cache = SIZE_CACHE.lock().unwrap();
    if let Some((_, w, h)) = cache.iter().find(|(l, _, _)| *l == label) {
        return (*w, *h);
    }
    let (w, h) = unsafe { measure_gdiplus(label) };
    cache.push((leak_label(label), w, h));
    (w, h)
}

/// 测量结果缓存键必须是 'static;调用方传入的都来自 LangDisplay::label,
/// 本身即 &'static str,leak 只是类型层面的安全转换。
fn leak_label(label: &str) -> &'static str {
    unsafe { std::mem::transmute::<&str, &'static str>(label) }
}

unsafe fn measure_gdiplus(label: &str) -> (i32, i32) {
    unsafe {
        let mut family = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );
        let mut font = std::ptr::null_mut();
        let _ = GdipCreateFont(family, FONT_SIZE_PX, FontStyleBold.0, UnitPixel, &mut font);
        let mut fmt = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        let _ = GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
        let _ = GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);

        // 给一个大布局矩形,让 GDI+ 报出自然尺寸
        let big = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 1000.0,
            Height: 200.0,
        };
        let mut bbox = RectF::default();
        let text: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
        // MeasureString 需要 graphics;用一个 1x1 内存位图的上下文即可
        let mut bitmap = std::ptr::null_mut();
        let _ =
            GdipCreateBitmapFromScan0(1, 1, 4, 0xE200B, None, &mut bitmap as *mut _ as *mut *mut _);
        let mut graphics = std::ptr::null_mut();
        let _ = GdipGetImageGraphicsContext(bitmap as *mut _, &mut graphics);
        let _ = GdipMeasureString(
            graphics,
            PCWSTR(text.as_ptr()),
            -1,
            font,
            &big,
            fmt,
            &mut bbox,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let _ = GdipDeleteGraphics(graphics);
        let _ = GdipDisposeImage(bitmap as *mut _);
        let _ = GdipDeleteStringFormat(fmt);
        let _ = GdipDeleteFont(font);
        let _ = GdipDeleteFontFamily(family);
        (
            (bbox.Width.ceil() as i32) + PAD_X * 2,
            (bbox.Height.ceil() as i32) + PAD_Y * 2,
        )
    }
}

/// 按文字测量尺寸并设定窗口位置(尺寸不变则不动)
unsafe fn layout_window(hwnd: HWND) {
    unsafe {
        let (w, h) = {
            let lang = CUR_LANG.lock().unwrap();
            measure_cached(lang.label)
        };
        let pos = window_pos(w, h);
        // SetWindowPos 触发 ULW 尺寸变化;位置每次都重设(屏幕可能变了)
        let _ = SetWindowPos(
            hwnd,
            None,
            pos.x,
            pos.y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

use windows::core::{PCWSTR, w};
