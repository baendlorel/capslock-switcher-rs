//! IME 模式直切:不注入按键,直接通过 API 翻转输入法的中/英模式。
//!
//! 与 overlay 的检测同构,两级通道:
//! L1: AttachThreadInput + ImmSetConversionStatus(跨进程常失败)
//! L2: 向前台默认 IME 窗口发 WM_IME_CONTROL + IMC_SETCONVERSIONMODE
//! 两级都失败时回退为注入 Ctrl+Space(由 hooks 完成)。

use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::{
    IME_CONVERSION_MODE, ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd,
    ImmGetOpenStatus, ImmReleaseContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    SendInput, VK_CAPITAL, VK_LCONTROL, VK_LMENU, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    WM_IME_CONTROL,
};

use crate::overlay::{LANG_EN, LANG_JA_HIRAGANA, LANG_JA_KATAKANA, LANG_KO, LANG_ZH, LangDisplay};

// ---- IME 消息常量(windows crate 未导出,源自 imm.h) ----

/// WM_IME_CONTROL:查转换模式(其中的 native 位区分「中/英」「かな/英」等)
const IMC_GETCONVERSIONMODE: u32 = 0x0001;
/// WM_IME_CONTROL:查 IME 开启状态
const IMC_GETOPENSTATUS: u32 = 0x0005;
/// 转换模式的「本国语言输入」位(= IME_CMODE_CHINESE = IME_CMODE_HANGUL)
const IME_CMODE_NATIVE: u32 = 0x0001;
/// 向 IME 窗口查询的超时毫秒数;超时视为该通道不可用
const IME_QUERY_TIMEOUT_MS: u32 = 100;

// ---- LANGID 解析(Win32 键盘布局的语言标识) ----

/// HKL(loword)是 LANGID;LANGID 低 10 位是主语言,高 6 位是子语言
const LANGID_MASK: usize = 0xFFFF;
const PRIMARY_LANG_MASK: usize = 0x3FF;

/// 主语言标识(Windows Language ID 的低 10 位)
mod primary_lang {
    /// 中文(zh)
    pub const CHINESE: usize = 0x04;
    /// 日文(ja)
    pub const JAPANESE: usize = 0x11;
    /// 韩文(ko)
    pub const KOREAN: usize = 0x12;
}

// ---- 日语 MS-IME 转换模式位(imm.h) ----

/// 本国语言输入位(置位 = 假名;清零 = 直接入力/英文)
const IME_CMODE_NATIVE_JA: u32 = 0x0001;
/// 片假名位(与 NATIVE 同置 = 片假名;仅 NATIVE = 平假名)
const IME_CMODE_KATAKANA_JA: u32 = 0x0002;

/// 上次已知(读成功或注入后乐观推进)的日语转换模式缓存。
/// 读通道(AttachThreadInput/IME 窗口)在 TSF 应用(Electron、VS Code 等)
/// 里经常双双失败——失败返回 0 会谎报「英」,导致轮切走错分支、
/// 浮层连闪 En。失败时改用此缓存兜底;每次注入后乐观写入预期值,
/// 保证读取持续失败时轮切依然正确步进。
static LAST_JA_CONV: AtomicU32 = AtomicU32::new(0);

/// 平假名的规范转换模式位(NATIVE | FULLSHAPE)
const CONV_HIRAGANA: u32 = IME_CMODE_NATIVE_JA | 0x0008;
/// 片假名的规范转换模式位(NATIVE | KATAKANA | FULLSHAPE)
const CONV_KATAKANA: u32 = IME_CMODE_NATIVE_JA | IME_CMODE_KATAKANA_JA | 0x0008;
/// 直接入力(英)的规范转换模式位
const CONV_ALPHA: u32 = 0;

/// 日语 IME 三种输入模式
enum JaMode {
    /// 直接入力(英文)
    Alpha,
    /// 平假名(NATIVE | FULLSHAPE)
    Hiragana,
    /// 片假名(NATIVE | KATAKANA | FULLSHAPE)
    Katakana,
}

/// 从转换模式位解析日语输入模式
fn ja_mode_from_conv(conv: u32) -> JaMode {
    if conv & IME_CMODE_NATIVE_JA == 0 {
        JaMode::Alpha
    } else if conv & IME_CMODE_KATAKANA_JA != 0 {
        JaMode::Katakana
    } else {
        JaMode::Hiragana
    }
}

/// 标记我们用 SendInput 注入的按键,避免钩子再次拦截造成死循环。
pub(crate) const INJECTED_FLAG: usize = 0x1;

/// 切换入口:按前台输入法语言分派。
/// - 日文 IME:三态轮切 英 → 平假名 → 片假名 → 英
/// - 其他(中文/韩文/纯英文布局):注入 Ctrl+Space
/// 只能在浮层线程调用(含阻塞调用,严禁放进低级钩子回调)。
pub fn toggle_or_fallback() {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            println!("[toggle] 无前台窗口,忽略");
            return;
        }
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let hkl = GetKeyboardLayout(fg_tid);
        let primary = (hkl.0 as usize & LANGID_MASK) & PRIMARY_LANG_MASK;

        if primary == primary_lang::JAPANESE {
            toggle_japanese(fg);
        } else {
            println!("[toggle] 注入 Ctrl+Space");
            send_key_tap(VK_LCONTROL.0, VK_SPACE.0);
        }
    }
}

/// 日语三态轮切:英 →(Ctrl+CapsLock)→ 平假名 →(Alt+CapsLock)→ 片假名 →(半角/全角)→ 英。
/// 英→平假名、平假名→片假名两步经实机验证有效,保持 VK 注入不动;
/// 片假名→英一步改用扫描码注入(见下)。
unsafe fn toggle_japanese(fg: HWND) {
    unsafe {
        let mode = ja_mode_from_conv(current_conv_mode(fg));

        match mode {
            JaMode::Alpha => {
                println!("[toggle] 日语:英 → 平假名 (注入 Ctrl+CapsLock)");
                send_key_tap(VK_LCONTROL.0, VK_CAPITAL.0);
                LAST_JA_CONV.store(CONV_HIRAGANA, Ordering::SeqCst);
            }
            JaMode::Hiragana => {
                println!("[toggle] 日语:平假名 → 片假名 (注入 Alt+CapsLock)");
                send_key_tap(VK_LMENU.0, VK_CAPITAL.0);
                LAST_JA_CONV.store(CONV_KATAKANA, Ordering::SeqCst);
            }
            JaMode::Katakana => {
                println!("[toggle] 日语:片假名 → 英 (注入 半角/全角 sc029)");
                send_scan_tap(0x0029, 0);
                LAST_JA_CONV.store(CONV_ALPHA, Ordering::SeqCst);
            }
        }
    }
}

// ---- 扫描码注入 ----

unsafe fn send_scan_tap(scan: u32, modifier: u32) {
    fn input(scan: u32, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                    // 扫描码直接放 wScan(非扩展键);KEYEVENTF_UNICODE 时此域才是字符
                    wScan: (scan & 0xFFFF) as u16,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP | KEYEVENTF_SCANCODE
                    } else {
                        KEYEVENTF_SCANCODE
                    },
                    time: 0,
                    dwExtraInfo: INJECTED_FLAG,
                },
            },
        }
    }

    let inputs: Vec<INPUT> = if modifier == 0 {
        vec![input(scan, false), input(scan, true)]
    } else {
        vec![
            input(modifier, false),
            input(scan, false),
            input(scan, true),
            input(modifier, true),
        ]
    };
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// 读取前台窗口当前的转换模式原始值(只读,IMM 读通道可靠)。
/// 两条读通道都失败时用上次成功读到的缓存兜底(而非谎报 0=英文);
/// 从未成功读过时才返回 0。
unsafe fn current_conv_mode(fg: HWND) -> u32 {
    unsafe {
        // L1:AttachThreadInput 后直读输入上下文
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let my_tid = GetCurrentThreadId();
        let attached = fg_tid != my_tid && AttachThreadInput(my_tid, fg_tid, true).as_bool();

        let r = (|| {
            let himc = ImmGetContext(fg);
            if himc.is_invalid() {
                return None;
            }
            let mut conv = IME_CONVERSION_MODE(0);
            let got = ImmGetConversionStatus(himc, Some(&mut conv), None).as_bool();
            let _ = ImmReleaseContext(fg, himc);
            if got { Some(conv.0) } else { None }
        })();

        if attached {
            let _ = AttachThreadInput(my_tid, fg_tid, false);
        }

        match r {
            Some(v) => {
                LAST_JA_CONV.store(v, Ordering::SeqCst);
                v
            }
            // L2:问默认 IME 窗口
            None => match probe_ime_wnd_conv(fg) {
                Some(v) => {
                    LAST_JA_CONV.store(v, Ordering::SeqCst);
                    v
                }
                // 两级都失败:用缓存,别谎报
                None => {
                    let cached = LAST_JA_CONV.load(Ordering::SeqCst);
                    println!("[toggle] 读转换模式失败,用缓存兜底: {cached:#x}");
                    cached
                }
            },
        }
    }
}

/// L2 读:IME 窗口的转换模式
unsafe fn probe_ime_wnd_conv(fg: HWND) -> Option<u32> {
    unsafe {
        let ime = ImmGetDefaultIMEWnd(fg);
        if ime.0.is_null() {
            return None;
        }
        query(ime, IMC_GETCONVERSIONMODE).map(|v| v as u32)
    }
}

/// 向 IME 窗口发只读查询,超时/失败返回 None(检测层在用)
unsafe fn query(ime: HWND, imc: u32) -> Option<usize> {
    unsafe {
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            ime,
            WM_IME_CONTROL,
            WPARAM(imc as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            IME_QUERY_TIMEOUT_MS,
            Some(&mut result),
        );
        if ok.0 == 0 { None } else { Some(result) }
    }
}

// ---- 状态检测(供 overlay 显示用) ----

/// 检测前台窗口当前输入状态,返回应显示的浮层内容。
pub fn detect_current_display() -> LangDisplay {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return LANG_EN;
        }

        let fg_tid = GetWindowThreadProcessId(fg, None);
        let hkl = GetKeyboardLayout(fg_tid);
        let primary = (hkl.0 as usize & LANGID_MASK) & PRIMARY_LANG_MASK;

        match primary {
            primary_lang::CHINESE => {
                let (open, native) = probe_state(fg);
                if open && native { LANG_ZH } else { LANG_EN }
            }
            primary_lang::JAPANESE => match ja_mode_from_conv(current_conv_mode(fg)) {
                JaMode::Alpha => LANG_EN,
                JaMode::Hiragana => LANG_JA_HIRAGANA,
                JaMode::Katakana => LANG_JA_KATAKANA,
            },
            primary_lang::KOREAN => {
                let (_, native) = probe_state(fg);
                if native { LANG_KO } else { LANG_EN }
            }
            _ => LANG_EN,
        }
    }
}

/// 两级探测当前 (open, native) 状态;均失败时按 native=false(显示 En)。
unsafe fn probe_state(fg: HWND) -> (bool, bool) {
    unsafe {
        if let Some(v) = probe_context_read(fg) {
            return v;
        }
        if let Some(v) = probe_ime_wnd_read(fg) {
            return v;
        }
        (true, false)
    }
}

/// L1 读:AttachThreadInput + ImmGetContext
unsafe fn probe_context_read(fg: HWND) -> Option<(bool, bool)> {
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
            let got = ImmGetConversionStatus(himc, Some(&mut conv), None).as_bool();
            let _ = ImmReleaseContext(fg, himc);
            if !got {
                return None;
            }
            Some((open, (conv.0 & IME_CMODE_NATIVE) != 0))
        })();

        if attached {
            let _ = AttachThreadInput(my_tid, fg_tid, false);
        }
        r
    }
}

/// L2 读:问前台默认 IME 窗口
unsafe fn probe_ime_wnd_read(fg: HWND) -> Option<(bool, bool)> {
    unsafe {
        let ime = ImmGetDefaultIMEWnd(fg);
        if ime.0.is_null() {
            return None;
        }
        let open = query(ime, IMC_GETOPENSTATUS)? != 0;
        let conv = query(ime, IMC_GETCONVERSIONMODE)?;
        Some((open, (conv & IME_CMODE_NATIVE as usize) != 0))
    }
}

/// 注入一次完整的组合键按下与松开。
/// `modifier` = 0 表示无修饰键(单键敲击)。
unsafe fn send_key_tap(modifier: u16, key: u16) {
    fn input(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: INJECTED_FLAG,
                },
            },
        }
    }

    let inputs: Vec<INPUT> = if modifier == 0 {
        vec![input(key, false), input(key, true)]
    } else {
        vec![
            input(modifier, false),
            input(key, false),
            input(key, true),
            input(modifier, true),
        ]
    };
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}
