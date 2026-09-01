//! "用前必读"窗口:上方居中展示说明图片,下方为使用说明文字。

use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::Graphics::GdiPlus::{
    FontStyleBold, FontStyleRegular, GdipCreateBitmapFromScan0, GdipCreateFont,
    GdipCreateFontFamilyFromName, GdipCreateFromHDC, GdipCreateSolidFill,
    GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
    GdipDeleteGraphics, GdipDeleteStringFormat, GdipDisposeImage, GdipDrawImageRect,
    GdipDrawString, GdipFillRectangle, GdipGetImageGraphicsContext, GdipGetImageHeight,
    GdipGetImageWidth, GdipMeasureString, GdipSetInterpolationMode, GdipSetSmoothingMode,
    GdipSetTextRenderingHint, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpBitmap,
    GpBrush, GpFontFamily, GpGraphics, GpImage, InterpolationModeHighQualityBicubic,
    RectF, SmoothingModeAntiAlias, Status, TextRenderingHintAntiAlias,
    UnitPixel,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
    GetSystemMetrics, GetMessageW, LoadIconW, MSG, PostQuitMessage, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SetForegroundWindow, WM_DESTROY, WM_PAINT, WNDCLASSW, WS_CAPTION, WS_SYSMENU,
    WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::assets;

/// 客户区宽度(96 DPI 基准,实际按系统 DPI 缩放)
const WINDOW_W: i32 = 440;
const PADDING: i32 = 24;
/// 图片显示宽度(高度按原始宽高比换算)
const IMAGE_W: i32 = 180;
/// 图片与文字之间的间距
const IMG_TEXT_GAP: i32 = 16;
/// 正文字号
const FONT_SIZE: f32 = 14.0;
/// 32 位预乘 ARGB(GDI+ PixelFormat 枚举值,测量用临时位图与 overlay 同口径)
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0xE200B;

/// 一段说明文字
struct Paragraph {
    text: &'static str,
    bold: bool,
    /// 段后间距(逻辑像素)
    space_after: i32,
}

const PARAGRAPHS: [Paragraph; 4] = [
    Paragraph {
        text: "使用前请先将中英文输入法切换键设为 Ctrl+Space。",
        bold: true,
        space_after: 16,
    },
    Paragraph {
        text: "快捷键：",
        bold: true,
        space_after: 6,
    },
    Paragraph {
        text: "CapsLock：切换中英文；日语输入法下切换平假名/片假名/英文。",
        bold: false,
        space_after: 4,
    },
    Paragraph {
        text: "Alt+CapsLock：原生大小写锁定。",
        bold: false,
        space_after: 0,
    },
];

static README_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// 窗口上方图片(窗口打开期间常驻,关闭时释放)
static mut README_IMAGE: *mut GpBitmap = std::ptr::null_mut();

/// 显示"用前必读"窗口;已打开时只置前,不重复创建。
pub fn show() {
    let p = README_HWND.load(Ordering::SeqCst);
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

/// 图片按 IMAGE_W 显示时的尺寸(宽, 高),高度按原始宽高比换算。
unsafe fn image_size(scale: f32) -> (f32, f32) {
    unsafe {
        let w = IMAGE_W as f32 * scale;
        let mut iw: u32 = 0;
        let mut ih: u32 = 0;
        let _ = GdipGetImageWidth(README_IMAGE as *mut GpImage, &mut iw);
        let _ = GdipGetImageHeight(README_IMAGE as *mut GpImage, &mut ih);
        let aspect = if iw > 0 { ih as f32 / iw as f32 } else { 1.0 };
        (w, w * aspect)
    }
}

/// 在临时位图上测量各段文字在 text_w 宽度下的实际渲染高度(含自动换行)。
/// 创建窗口前用它确定客户区高度,绘制时用它保证换行结果一致。
unsafe fn measure_heights(scale: f32, text_w: f32, out: &mut [f32]) {
    unsafe {
        let mut bmp: *mut GpBitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromScan0(8, 8, 0, PIXEL_FORMAT_32BPP_PARGB, None, &mut bmp);
        if st != Status(0) || bmp.is_null() {
            return;
        }
        let mut graphics: *mut GpGraphics = std::ptr::null_mut();
        let _ = GdipGetImageGraphicsContext(bmp as *mut GpImage, &mut graphics);
        let mut family: *mut GpFontFamily = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );

        for (i, p) in PARAGRAPHS.iter().enumerate() {
            let style = if p.bold {
                FontStyleBold.0
            } else {
                FontStyleRegular.0
            };
            let mut font = std::ptr::null_mut();
            let _ = GdipCreateFont(family, FONT_SIZE * scale, style, UnitPixel, &mut font);
            let mut fmt = std::ptr::null_mut();
            let _ = GdipCreateStringFormat(0, 0, &mut fmt);
            let layout = RectF {
                X: 0.0,
                Y: 0.0,
                Width: text_w,
                Height: 1e4,
            };
            let mut bound = RectF::default();
            let mut fitted: i32 = 0;
            let mut lines: i32 = 0;
            let text16: Vec<u16> = p.text.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = GdipMeasureString(
                graphics,
                PCWSTR(text16.as_ptr()),
                -1,
                font,
                &layout,
                fmt,
                &mut bound,
                &mut fitted,
                &mut lines,
            );
            out[i] = bound.Height.max(FONT_SIZE * scale);
            let _ = GdipDeleteStringFormat(fmt);
            let _ = GdipDeleteFont(font);
        }

        let _ = GdipDeleteFontFamily(family);
        let _ = GdipDeleteGraphics(graphics);
        let _ = GdipDisposeImage(bmp as *mut GpImage);
    }
}

unsafe fn window_loop() {
    unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());

        README_IMAGE = assets::load_bitmap(assets::UMBRAL_KEYS_PNG);

        let scale = dpi_scale();
        let padding = PADDING as f32 * scale;
        let client_w = WINDOW_W as f32 * scale;
        let text_w = client_w - padding * 2.0;
        let (_img_w, img_h) = image_size(scale);

        let mut heights = [0.0f32; PARAGRAPHS.len()];
        measure_heights(scale, text_w, &mut heights);
        let text_h: f32 = PARAGRAPHS
            .iter()
            .zip(heights.iter())
            .map(|(p, h)| h + p.space_after as f32 * scale)
            .sum();

        let client_h = padding + img_h + IMG_TEXT_GAP as f32 * scale + text_h + padding;

        // 由期望的客户区尺寸换算窗口外部尺寸(含标题栏/边框),
        // 保证内容在任何主题下都完整可见。
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: client_w.round() as i32,
            bottom: client_h.round() as i32,
        };
        let _ = AdjustWindowRect(&mut rc, WS_CAPTION | WS_SYSMENU, false);
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;

        let hinst = GetModuleHandleW(None).expect("GetModuleHandleW 失败");
        // 与托盘图标同一枚嵌入资源(ID=1),保持标题栏图标一致
        let icon = LoadIconW(Some(hinst.into()), PCWSTR(1 as *const u16)).ok();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hIcon: icon.unwrap_or_default(),
            lpszClassName: w!("CapsLockReadmeWnd"),
            ..Default::default()
        };
        // 窗口类可能已在上一次打开时注册过,此处忽略重复注册的失败
        let _ = RegisterClassW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - win_w) / 2;
        let y = (sh - win_h) / 2;

        let hwnd = CreateWindowExW(
            Default::default(),
            w!("CapsLockReadmeWnd"),
            w!("用前必读"),
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
        .expect("创建用前必读窗口失败");

        README_HWND.store(hwnd.0, Ordering::SeqCst);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = DispatchMessageW(&msg);
        }

        README_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
        if !README_IMAGE.is_null() {
            let _ = GdipDisposeImage(README_IMAGE as *mut _);
            README_IMAGE = std::ptr::null_mut();
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
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
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

        // 布局基于客户区实际尺寸
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let win_w = (client.right - client.left) as f32;

        // 背景
        let mut bg_brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(0xFF_F7_F8_FA, &mut bg_brush);
        let _ = GdipFillRectangle(graphics, bg_brush as *mut GpBrush, 0.0, 0.0, win_w, (client.bottom - client.top) as f32);
        let _ = GdipDeleteBrush(bg_brush as *mut GpBrush);

        let padding = PADDING as f32 * scale;
        let text_w = win_w - padding * 2.0;
        let (img_w, img_h) = image_size(scale);

        // 上方居中图片
        if !README_IMAGE.is_null() {
            let img_x = (win_w - img_w) / 2.0;
            let _ = GdipDrawImageRect(
                graphics,
                README_IMAGE as *mut GpImage,
                img_x,
                padding,
                img_w,
                img_h,
            );
        }

        // 下方文字:先测量再绘制,与建窗时的高度计算保持同一换行结果
        let mut heights = [0.0f32; PARAGRAPHS.len()];
        measure_heights(scale, text_w, &mut heights);

        let mut family = std::ptr::null_mut();
        let _ = GdipCreateFontFamilyFromName(
            w!("Microsoft YaHei UI"),
            std::ptr::null_mut(),
            &mut family,
        );

        let mut y = padding + img_h + IMG_TEXT_GAP as f32 * scale;
        for (p, h) in PARAGRAPHS.iter().zip(heights.iter()) {
            let style = if p.bold {
                FontStyleBold.0
            } else {
                FontStyleRegular.0
            };
            draw_paragraph(
                graphics,
                family,
                padding,
                y,
                text_w,
                *h,
                p.text,
                FONT_SIZE * scale,
                style,
                0xFF_21_25_27,
            );
            y += h + p.space_after as f32 * scale;
        }

        let _ = GdipDeleteFontFamily(family);
        let _ = GdipDeleteGraphics(graphics);

        let _ = EndPaint(hwnd, &ps);
    }
}

/// 在 (x, y) 处以宽度 w 绘制一段可自动换行的文字,布局高度取测量值。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_paragraph(
    graphics: *mut GpGraphics,
    family: *mut GpFontFamily,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    text: &str,
    size: f32,
    style: i32,
    color: u32,
) {
    unsafe {
        let mut font = std::ptr::null_mut();
        let _ = GdipCreateFont(family, size, style, UnitPixel, &mut font);
        let mut fmt = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        let mut brush = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        let layout = RectF {
            X: x,
            Y: y,
            Width: w,
            Height: h,
        };
        let text16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = GdipDrawString(
            graphics,
            PCWSTR(text16.as_ptr()),
            -1,
            font,
            &layout,
            fmt,
            brush as *mut GpBrush,
        );
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
        let _ = GdipDeleteStringFormat(fmt);
        let _ = GdipDeleteFont(font);
    }
}
