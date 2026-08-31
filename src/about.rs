//! "关于"窗口:左侧展示项目图片,右侧展示项目名称、作者、GitHub 链接与版权信息。

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient};
use windows::Win32::Graphics::GdiPlus::{
    FontStyleBold, FontStyleRegular, GdipCreateFont, GdipCreateFontFamilyFromName,
    GdipCreateFromHDC, GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush,
    GdipDeleteFont, GdipDeleteFontFamily, GdipDeleteGraphics, GdipDeleteStringFormat,
    GdipDisposeImage, GdipDrawImageRect, GdipDrawString, GdipFillRectangle,
    GdipSetInterpolationMode, GdipSetSmoothingMode, GdipSetTextRenderingHint, GdiplusShutdown,
    GdiplusStartup, GdiplusStartupInput, GpBitmap, GpBrush, GpFontFamily, GpGraphics, GpImage,
    InterpolationModeHighQualityBicubic, RectF, SmoothingModeAntiAlias, TextRenderingHintAntiAlias,
    UnitPixel,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
    GetSystemMetrics, IDC_HAND, LoadCursorW, LoadIconW, MSG, PostQuitMessage, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, SetCursor, SetForegroundWindow, WM_DESTROY,
    WM_LBUTTONUP, WM_PAINT, WM_SETCURSOR, WNDCLASSW, WS_CAPTION, WS_SYSMENU, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::assets;

/// 窗口逻辑尺寸(96 DPI 基准,实际按系统 DPI 缩放)
const WINDOW_W: i32 = 480;
const WINDOW_H: i32 = 220;
const PADDING: i32 = 24;
const IMAGE_SIZE: i32 = 140;

const PROJECT_NAME: &str = "CapsLock Switcher";
const AUTHOR_LABEL: &str = "作者:Kasukabe Tsumugi";
const GITHUB_URL: &str = "https://github.com/baendlorel";
const COPYRIGHT: &str = "Copyright \u{00A9} 2026 Kasukabe Tsumugi. All rights reserved.";

static ABOUT_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// GitHub 链接文字的客户区命中区域,供点击/悬停判定
static LINK_RECT: Mutex<RECT> = Mutex::new(RECT {
    left: 0,
    top: 0,
    right: 0,
    bottom: 0,
});
/// 关于窗口左侧图片(窗口打开期间常驻,关闭时释放)
static mut ABOUT_IMAGE: *mut GpBitmap = std::ptr::null_mut();

/// 显示"关于"窗口;已打开时只置前,不重复创建。
pub fn show() {
    let p = ABOUT_HWND.load(Ordering::SeqCst);
    if !p.is_null() {
        unsafe {
            let _ = SetForegroundWindow(HWND(p));
        }
        return;
    }
    std::thread::spawn(|| unsafe { window_loop() });
}

fn dpi_scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

unsafe fn window_loop() {
    unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());

        ABOUT_IMAGE = assets::load_bitmap(assets::UMBRAL_KEYS_PNG);

        let hinst = GetModuleHandleW(None).expect("GetModuleHandleW 失败");
        // 与托盘图标同一枚嵌入资源(ID=1),保持标题栏图标一致
        let icon = LoadIconW(Some(hinst.into()), PCWSTR(1 as *const u16)).ok();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hIcon: icon.unwrap_or_default(),
            lpszClassName: w!("CapsLockAboutWnd"),
            ..Default::default()
        };
        // 窗口类可能已在上一次打开时注册过,此处忽略重复注册的失败
        let _ = RegisterClassW(&wc);

        let scale = dpi_scale();
        let win_w = (WINDOW_W as f32 * scale).round() as i32;
        let win_h = (WINDOW_H as f32 * scale).round() as i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - win_w) / 2;
        let y = (sh - win_h) / 2;

        let hwnd = CreateWindowExW(
            Default::default(),
            w!("CapsLockAboutWnd"),
            w!("关于 CapsLock Switcher"),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("创建关于窗口失败");

        ABOUT_HWND.store(hwnd.0, Ordering::SeqCst);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = DispatchMessageW(&msg);
        }

        ABOUT_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
        if !ABOUT_IMAGE.is_null() {
            let _ = GdipDisposeImage(ABOUT_IMAGE as *mut _);
            ABOUT_IMAGE = std::ptr::null_mut();
        }
        let _ = GdiplusShutdown(token);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                on_paint(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if cursor_over_link(hwnd) {
                    let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONUP => {
                if cursor_over_link(hwnd) {
                    open_github();
                }
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

fn cursor_over_link(hwnd: HWND) -> bool {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = ScreenToClient(hwnd, &mut pt);
        let rect = *LINK_RECT.lock().unwrap();
        pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
    }
}

fn open_github() {
    let url: Vec<u16> = GITHUB_URL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// 不透明 ARGB(alpha 恒 0xFF)
const fn argb_opaque(rgb: u32) -> u32 {
    0xFF00_0000 | rgb
}

unsafe fn on_paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let scale = dpi_scale();

        let mut graphics = std::ptr::null_mut();
        let _ = GdipCreateFromHDC(hdc, &mut graphics);
        let _ = GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
        let _ = GdipSetInterpolationMode(graphics, InterpolationModeHighQualityBicubic);
        let _ = GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAlias);

        // 布局必须基于客户区实际尺寸,而非 CreateWindowExW 传入的外部尺寸(后者含标题栏),
        // 否则内容会整体偏下甚至被裁切。
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let win_w = (client.right - client.left) as f32;
        let win_h = (client.bottom - client.top) as f32;
        let padding = PADDING as f32 * scale;
        let image_size = IMAGE_SIZE as f32 * scale;

        // 背景
        let mut bg_brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(argb_opaque(0xF7F8FA), &mut bg_brush);
        let _ = GdipFillRectangle(graphics, bg_brush as *mut GpBrush, 0.0, 0.0, win_w, win_h);
        let _ = GdipDeleteBrush(bg_brush as *mut GpBrush);

        // 左侧图片
        if !ABOUT_IMAGE.is_null() {
            let img_y = (win_h - image_size) / 2.0;
            let _ = GdipDrawImageRect(
                graphics,
                ABOUT_IMAGE as *mut GpImage,
                padding,
                img_y,
                image_size,
                image_size,
            );
        }

        // 右侧文字
        let text_x = padding * 2.0 + image_size;
        let text_w = win_w - padding - text_x;

        let mut family = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );

        draw_line(
            graphics,
            family,
            text_x,
            28.0 * scale,
            text_w,
            PROJECT_NAME,
            20.0 * scale,
            FontStyleBold.0,
            argb_opaque(0x212527),
        );
        draw_line(
            graphics,
            family,
            text_x,
            70.0 * scale,
            text_w,
            AUTHOR_LABEL,
            14.0 * scale,
            FontStyleRegular.0,
            argb_opaque(0x212527),
        );

        let link_top = 100.0 * scale;
        let link_h = draw_line(
            graphics,
            family,
            text_x,
            link_top,
            text_w,
            GITHUB_URL,
            14.0 * scale,
            FontStyleRegular.0,
            argb_opaque(0x0073FF),
        );
        *LINK_RECT.lock().unwrap() = RECT {
            left: text_x as i32,
            top: link_top as i32,
            right: (text_x + text_w) as i32,
            bottom: (link_top + link_h) as i32,
        };

        draw_line(
            graphics,
            family,
            text_x,
            win_h - 40.0 * scale,
            text_w,
            COPYRIGHT,
            12.0 * scale,
            FontStyleRegular.0,
            argb_opaque(0x6B7078),
        );

        let _ = GdipDeleteFontFamily(family);
        let _ = GdipDeleteGraphics(graphics);

        let _ = EndPaint(hwnd, &ps);
    }
}

/// 绘制一行左对齐文字,返回占用的行高(供命中区域计算)。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_line(
    graphics: *mut GpGraphics,
    family: *mut GpFontFamily,
    x: f32,
    y: f32,
    w: f32,
    text: &str,
    size: f32,
    style: i32,
    color: u32,
) -> f32 {
    unsafe {
        let mut font = std::ptr::null_mut();
        let _ = GdipCreateFont(family, size, style, UnitPixel, &mut font);
        let mut fmt = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        let mut brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        let line_h = size * 1.6;
        let layout = RectF {
            X: x,
            Y: y,
            Width: w,
            Height: line_h,
        };
        let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = GdipDrawString(
            graphics,
            PCWSTR(text_w.as_ptr()),
            -1,
            font,
            &layout,
            fmt,
            brush as *mut GpBrush,
        );
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
        let _ = GdipDeleteStringFormat(fmt);
        let _ = GdipDeleteFont(font);
        line_h
    }
}
