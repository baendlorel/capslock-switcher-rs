//! 语言指示浮层:输入法切换完成后,在屏幕中央短暂显示当前输入语言。
//!
//! EN 蓝色 / 中 红色 / 日 黄色 / 韩 黑色,显示 0.3s 后再用 0.3s 淡出消失。
//! 检测方式:前台窗口所在线程的键盘布局(HKL)定语种,
//! 再用 WM_IME_CONTROL 查 IME 的开/关与转换模式区分「本国语言/英文」输入模式。

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU32, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BeginPaint, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject, EndPaint, FF_DONTCARE, FW_SEMIBOLD, FillRect,
    GetDC, GetStockObject, GetTextExtentPoint32W, HFONT, NULL_PEN, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, HWND_TOPMOST, KillTimer, LWA_ALPHA,
    LWA_COLORKEY, MSG, MoveWindow, PostMessageW, PostQuitMessage, PostThreadMessageW,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SMTO_ABORTIFHUNG, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageTimeoutW, SetLayeredWindowAttributes,
    SetTimer, SetWindowPos, ShowWindow, WM_APP, WM_DESTROY, WM_IME_CONTROL, WM_PAINT, WM_QUIT,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

// windows crate 未导出的 IME 消息常量(imm.h)
/// 查询 IME 开启状态(中文/日文 IME 的「允许 native 输入」开关)
const IMC_GETOPENSTATUS: u32 = 0x0005;
/// 查询当前转换模式(韩文 IME 用它的 native 位区分韩/英)
const IMC_GETCONVERSIONMODE: u32 = 0x0001;
/// 转换模式中的「本国语言输入」位
const IME_CMODE_NATIVE: usize = 0x0001;

// ---- 可视参数 ----

/// 文字字号(像素)
const FONT_SIZE_PX: i32 = 24;
const PAD_X: i32 = 20;
const PAD_Y: i32 = 12;
/// 注入 Ctrl+Space 后等待输入法状态落定的延时
const READ_DELAY_MS: u32 = 120;
/// 完全显示的持续时间
const SHOW_MS: u32 = 300;
/// 淡出持续时间
const FADE_MS: u32 = 300;
/// 动画 tick 间隔
const TICK_MS: u32 = 16;
/// 显示期间的整窗透明度(255 = 不透明)
const BASE_ALPHA: u8 = 235;
/// 色键(品红)= 完全透明;COLORREF 布局为 0x00BBGGRR
const COLOR_KEY: COLORREF = COLORREF(0x00FF_00FF);
/// 胶囊底色(近白)
const PILL_COLOR: COLORREF = COLORREF(0x00FA_FA_FA);

// ---- 计时器 ID ----
const TIMER_READ: usize = 1;
const TIMER_ANIM: usize = 2;

/// 自定义消息:请求显示语言浮层
const WM_APP_SHOW: u32 = WM_APP + 1;

// ---- 线程共享状态 ----

static OVERLAY_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static OVERLAY_TID: AtomicU32 = AtomicU32::new(0);
/// 动画起点(毫秒时钟,GetTickCount64);0 = 未在显示
static ANIM_START: AtomicU64 = AtomicU64::new(0);
static CUR_LANG: Mutex<LangDisplay> = Mutex::new(LANG_EN);

// ---- 语言定义 ----

/// COLORREF 参数顺序是 R,G,B,内部布局 0x00BBGGRR
const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(r | (g << 8) | (b << 16))
}

struct LangDisplay {
    label: &'static str,
    color: COLORREF,
}

const LANG_EN: LangDisplay = LangDisplay {
    label: "EN",
    color: rgb(0x1E, 0x70, 0xEB), // 蓝
};
const LANG_ZH: LangDisplay = LangDisplay {
    label: "中",
    color: rgb(0xE5, 0x39, 0x35), // 红
};
const LANG_JA: LangDisplay = LangDisplay {
    label: "日",
    color: rgb(0xD4, 0x8E, 0x00), // 黄(加深以保证白底可读)
};
const LANG_KO: LangDisplay = LangDisplay {
    label: "韩",
    color: rgb(0x21, 0x21, 0x21), // 黑
};

// ---- 对外接口 ----

/// 启动浮层窗口线程(常驻,负责显示/淡出/隐藏)
pub fn spawn_thread() {
    std::thread::spawn(|| unsafe { message_loop() });
}

/// 请求显示语言浮层。可在钩子线程调用:仅投递一条消息,立即返回。
pub fn show_language_overlay() {
    let p = OVERLAY_HWND.load(Ordering::SeqCst);
    if !p.is_null() {
        let _ = unsafe { PostMessageW(Some(HWND(p)), WM_APP_SHOW, WPARAM(0), LPARAM(0)) };
    }
}

/// 通知浮层线程退出
pub fn post_quit() {
    let tid = OVERLAY_TID.load(Ordering::SeqCst);
    if tid != 0 {
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

// ---- 浮层线程 ----

unsafe fn message_loop() {
    OVERLAY_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

    let hinst = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW 失败");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
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

    // 色键 + 整窗透明度;品红像素完全透明,其余按 alpha 混合
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLOR_KEY, BASE_ALPHA, LWA_COLORKEY | LWA_ALPHA);
    }

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
            WM_APP_SHOW => {
                on_request_show(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                on_timer(hwnd, wparam.0);
                LRESULT(0)
            }
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

/// 收到显示请求:重置状态,延时一小段再读输入法(等 IME 落定)
unsafe fn on_request_show(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_READ);
        let _ = KillTimer(Some(hwnd), TIMER_ANIM);
        // 恢复不透明(若上次停在淡出中途)
        let _ = SetLayeredWindowAttributes(hwnd, COLOR_KEY, BASE_ALPHA, LWA_COLORKEY | LWA_ALPHA);
        let _ = SetTimer(Some(hwnd), TIMER_READ, READ_DELAY_MS, None);
    }
}

/// 单个动画 tick:按「从显示起经过的绝对时间」直接算 alpha,不依赖链式计时器。
/// t < SHOW_MS        -> BASE_ALPHA
/// SHOW_MS..=SHOW+FADE -> 线性淡到 0
/// 之后               -> 强制隐藏
unsafe fn on_timer(hwnd: HWND, id: usize) {
    unsafe {
        match id {
            TIMER_READ => {
                // 读取当前输入语言,测量居中并显示,启动动画
                let _ = KillTimer(Some(hwnd), TIMER_READ);
                *CUR_LANG.lock().unwrap() = detect_language();
                layout_window(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let _ = SetLayeredWindowAttributes(
                    hwnd,
                    COLOR_KEY,
                    BASE_ALPHA,
                    LWA_COLORKEY | LWA_ALPHA,
                );
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
                    BASE_ALPHA
                } else if elapsed < SHOW_MS + FADE_MS {
                    let t = elapsed - SHOW_MS;
                    // 线性淡出(整除);到时后的下一 tick 由下面的超时分支强制归零
                    BASE_ALPHA.saturating_sub((BASE_ALPHA as u32 * t / FADE_MS) as u8)
                } else {
                    0
                };
                if alpha == 0 {
                    // 动画结束:停表、强制完全透明、隐藏窗口
                    ANIM_START.store(0, Ordering::SeqCst);
                    let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                    let _ = SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY | LWA_ALPHA);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else {
                    let _ = SetLayeredWindowAttributes(
                        hwnd,
                        COLOR_KEY,
                        alpha,
                        LWA_COLORKEY | LWA_ALPHA,
                    );
                }
            }
            _ => {}
        }
    }
}

/// 按当前文字测量尺寸并把窗口移到屏幕正中
unsafe fn layout_window(hwnd: HWND) {
    let label = CUR_LANG.lock().unwrap().label;
    let (tw, th) = unsafe { measure_text(hwnd, label) };
    let width = tw + PAD_X * 2;
    let height = th + PAD_Y * 2;
    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    unsafe {
        let _ = MoveWindow(
            hwnd,
            (sw - width) / 2,
            (sh - height) / 2,
            width,
            height,
            false,
        );
    }
}

unsafe fn on_paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let rc: RECT = ps.rcPaint;

        // 1. 整面填色键(品红)= 完全透明
        let key_brush = CreateSolidBrush(COLOR_KEY);
        FillRect(hdc, &rc, key_brush);
        let _ = DeleteObject(key_brush.into());

        // 2. 近白色胶囊底(NULL_PEN 避免描边)
        let pill = CreateSolidBrush(PILL_COLOR);
        let old_brush = SelectObject(hdc, pill.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let r = (rc.bottom - rc.top) / 2;
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, r, r);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_brush);
        let _ = DeleteObject(pill.into());

        // 3. 居中绘制语言文字
        {
            let lang = CUR_LANG.lock().unwrap();
            let font = create_font();
            let old_font = SelectObject(hdc, font.into());
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, lang.color);
            let wide: Vec<u16> = lang.label.encode_utf16().collect();
            let (tw, th) = text_size(hdc, &wide);
            let x = rc.left + ((rc.right - rc.left) - tw) / 2;
            let y = rc.top + ((rc.bottom - rc.top) - th) / 2;
            let _ = TextOutW(hdc, x, y, &wide);
            let _ = SelectObject(hdc, old_font);
            let _ = DeleteObject(font.into());
        }

        let _ = EndPaint(hwnd, &mut ps);
    }
}

unsafe fn create_font() -> HFONT {
    unsafe {
        CreateFontW(
            -FONT_SIZE_PX, // 高度为负 = 字号按像素计
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Microsoft YaHei UI"),
        )
    }
}

unsafe fn measure_text(hwnd: HWND, text: &str) -> (i32, i32) {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        let font = create_font();
        let old = SelectObject(hdc, font.into());
        let wide: Vec<u16> = text.encode_utf16().collect();
        let size = text_size(hdc, &wide);
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        size
    }
}

unsafe fn text_size(hdc: windows::Win32::Graphics::Gdi::HDC, wide: &[u16]) -> (i32, i32) {
    let mut sz = SIZE::default();
    if unsafe { GetTextExtentPoint32W(hdc, wide, &mut sz) }.as_bool() {
        (sz.cx, sz.cy)
    } else {
        (0, 0)
    }
}

// ---- 语言检测 ----

fn detect_language() -> LangDisplay {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return LANG_EN;
        }

        let tid = GetWindowThreadProcessId(fg, None);
        let hkl = GetKeyboardLayout(tid);
        // HKL 低 16 位是 LANGID,其低 10 位是主语言标识
        let langid = (hkl.0 as usize) & 0xFFFF;
        let primary = langid & 0x3FF;

        match primary {
            0x04 => {
                // 中文:IME 开 = 中文模式,关 = 英文模式
                if ime_native_mode(fg, true) {
                    LANG_ZH
                } else {
                    LANG_EN
                }
            }
            0x11 => {
                // 日文:IME 开 = 假名模式,关 = 直接输入(英文)
                if ime_native_mode(fg, true) {
                    LANG_JA
                } else {
                    LANG_EN
                }
            }
            0x12 => {
                // 韩文:看转换模式的 native 位(开/关状态对韩文 IME 不适用)
                if ime_native_mode(fg, false) {
                    LANG_KO
                } else {
                    LANG_EN
                }
            }
            _ => LANG_EN,
        }
    }
}

/// 查询前台窗口 IME 是否处于「本国语言输入」模式。
/// `open_based`:中文/日文用开/关状态;韩文用转换模式的 IME_CMODE_NATIVE 位。
/// 查询失败时按本国语言处理(宁可显示 中/日/韩 也不误报 EN)。
unsafe fn ime_native_mode(hwnd: HWND, open_based: bool) -> bool {
    unsafe {
        let ime = ImmGetDefaultIMEWnd(hwnd);
        if ime.0.is_null() {
            return true;
        }
        let imc = if open_based {
            IMC_GETOPENSTATUS
        } else {
            IMC_GETCONVERSIONMODE
        };
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            ime,
            WM_IME_CONTROL,
            WPARAM(imc as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            80,
            Some(&mut result),
        );
        if ok.0 == 0 {
            return true;
        }
        if open_based {
            result != 0
        } else {
            (result & IME_CMODE_NATIVE) != 0
        }
    }
}
