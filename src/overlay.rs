//! 语言指示浮层:输入法切换完成后,在屏幕中央短暂显示当前输入语言。
//!
//! EN 蓝色 / 中 红色 / 日 黄色 / 韩 黑色,显示 0.3s 后再用 0.3s 淡出消失。
//! 检测方式:前台窗口所在线程的键盘布局(HKL)定语种,
//! 再经 AttachThreadInput 后用 ImmGetContext + ImmGetConversionStatus
//! 读取前台输入上下文的真实转换状态,区分本国语言模式与英文模式。

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU32, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BeginPaint, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject, EndPaint, FF_DONTCARE, FW_SEMIBOLD, FillRect,
    GetDC, GetStockObject, GetTextExtentPoint32W, HFONT, InvalidateRect, NULL_PEN,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT, TextOutW, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd, ImmGetOpenStatus,
    IME_CONVERSION_MODE,
};
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

// windows crate 未导出的 IME 常量(imm.h)
/// 转换模式中的「本国语言输入」位:置位 = 中文/假名/韩文模式
const IME_CMODE_NATIVE: u32 = 0x0001;
/// WM_IME_CONTROL:查 IME 开启状态
const IMC_GETOPENSTATUS: u32 = 0x0005;
/// WM_IME_CONTROL:查转换模式
const IMC_GETCONVERSIONMODE: u32 = 0x0001;

// ---- 可视参数 ----

/// 文字字号(像素)
const FONT_SIZE_PX: i32 = 24;
const PAD_X: i32 = 20;
const PAD_Y: i32 = 12;
/// 注入 Ctrl+Space 后等待输入法状态落定的延时
const READ_DELAY_MS: u32 = 20;
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

/// 按当前文字测量尺寸并把窗口移到屏幕正中。
/// 尺寸变化后必须整窗失效:分层窗口复用旧位图,不失效的话
/// 旧内容的边缘会残留(表现为胶囊两侧像被切掉一条)。
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
        // NULL = 整个客户区失效,强制下次完整重绘
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);
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

        let fg_tid = GetWindowThreadProcessId(fg, None);
        let hkl = GetKeyboardLayout(fg_tid);
        // HKL 低 16 位是 LANGID,其低 10 位是主语言标识
        let langid = (hkl.0 as usize) & 0xFFFF;
        let primary = langid & 0x3FF;

        let lang_name = match primary {
            0x04 => "zh",
            0x11 => "ja",
            0x12 => "ko",
            _ => "other",
        };

        // 两级探测,任一级给出结果即用
        let (open, native) = match probe_via_context(fg) {
            Some(v) => {
                println!("[detect] {lang_name}: ImmGetContext 层 open={} conv_native={}", v.0, v.1);
                v
            }
            None => match probe_via_ime_wnd(fg) {
                Some(v) => {
                    println!("[detect] {lang_name}: WM_IME_CONTROL 层 open={} conv_native={}", v.0, v.1);
                    v
                }
                None => {
                    println!("[detect] {lang_name}: 两级探测均失败,按键盘布局回退");
                    // 查不到模式,按布局显示语种(不误报 EN)
                    return match primary {
                        0x04 => LANG_ZH,
                        0x11 => LANG_JA,
                        0x12 => LANG_KO,
                        _ => LANG_EN,
                    };
                }
            },
        };

        match primary {
            0x04 => {
                // 中文输入法:开 + native(中文)模式 = 中;英文模式 = EN
                if open && native {
                    LANG_ZH
                } else {
                    LANG_EN
                }
            }
            0x11 => {
                // 日文输入法:开 + 假名(native)模式 = 日;直接入力/英数 = EN
                if open && native {
                    LANG_JA
                } else {
                    LANG_EN
                }
            }
            0x12 => {
                // 韩文输入法:转换模式的 native 位区分 한/A
                if native {
                    LANG_KO
                } else {
                    LANG_EN
                }
            }
            _ => LANG_EN,
        }
    }
}

/// 第一级:AttachThreadInput 后 ImmGetContext 直接读前台输入上下文。
/// Windows 8+ 的 HIMC 是进程私有的,跨进程常拿不到 → 返回 None 换下一级。
unsafe fn probe_via_context(fg: HWND) -> Option<(bool, bool)> {
    unsafe {
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let my_tid = GetCurrentThreadId();
        let attached = fg_tid != my_tid && AttachThreadInput(my_tid, fg_tid, true).as_bool();

        let r = (|| {
            let himc = ImmGetContext(fg);
            if himc.is_invalid() {
                return None;
            }
            let open = ImmGetOpenStatus(himc).as_bool();
            let mut conv = IME_CONVERSION_MODE(0);
            let got_conv = ImmGetConversionStatus(himc, Some(&mut conv), None).as_bool();
            let native = got_conv && (conv.0 & IME_CMODE_NATIVE) != 0;
            Some((open, native))
        })();

        if attached {
            let _ = AttachThreadInput(my_tid, fg_tid, false);
        }
        r
    }
}

/// 第二级:问前台默认 IME 窗口(AutoHotkey 的 IME 用户常用手法)。
unsafe fn probe_via_ime_wnd(fg: HWND) -> Option<(bool, bool)> {
    unsafe {
        let ime = ImmGetDefaultIMEWnd(fg);
        if ime.0.is_null() {
            return None;
        }
        let open = query_ime_wnd(ime, IMC_GETOPENSTATUS)? != 0;
        let conv = query_ime_wnd(ime, IMC_GETCONVERSIONMODE)?;
        Some((open, (conv & IME_CMODE_NATIVE as usize) != 0))
    }
}

/// 向 IME 窗口发查询,超时/失败返回 None
unsafe fn query_ime_wnd(ime: HWND, imc: u32) -> Option<usize> {
    unsafe {
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            ime,
            WM_IME_CONTROL,
            WPARAM(imc as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            100,
            Some(&mut result),
        );
        if ok.0 == 0 { None } else { Some(result) }
    }
}
