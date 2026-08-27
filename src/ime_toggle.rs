//! IME 模式直切:不注入按键,直接通过 API 翻转输入法的中/英模式。
//!
//! 与 overlay 的检测同构,两级通道:
//! L1: AttachThreadInput + ImmSetConversionStatus(跨进程常失败)
//! L2: 向前台默认 IME 窗口发 WM_IME_CONTROL + IMC_SETCONVERSIONMODE
//! 两级都失败时回退为注入 Ctrl+Space(由 hooks 完成)。

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd, ImmGetOpenStatus,
    ImmReleaseContext, IME_CONVERSION_MODE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_LCONTROL,
    VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
    WM_IME_CONTROL,
};

use crate::overlay::{LangDisplay, LANG_EN, LANG_JA, LANG_KO, LANG_ZH};

/// WM_IME_CONTROL:查转换模式(翻转前先读当前值)
const IMC_GETCONVERSIONMODE: u32 = 0x0001;
/// 转换模式的「本国语言输入」位(= IME_CMODE_CHINESE = IME_CMODE_HANGUL)
const IME_CMODE_NATIVE: u32 = 0x0001;

/// 标记我们用 SendInput 注入的按键,避免钩子再次拦截造成死循环。
pub(crate) const INJECTED_FLAG: usize = 0x1;

/// 切换中/英模式。
/// 实测(Win11 + 微软拼音,TSF 架构):IMM 两级通道的 SET 都会被静默忽略——
/// L1 跨进程拿不到 HIMC;L2 WM_IME_CONTROL 返回成功但 IME 不执行。
/// 因此这里不再信任 IMM 写入,直接走注入 Ctrl+Space(由 IME 自己的热键响应)。
/// 只能在浮层线程调用(含阻塞调用,严禁放进低级钩子回调)。
pub fn toggle_or_fallback() {
    println!("[toggle] 走注入 Ctrl+Space 路径");
    unsafe { send_ctrl_space() };
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
            100,
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
        let primary = ((hkl.0 as usize) & 0xFFFF) & 0x3FF;

        let (open, native) = probe_state(fg);

        match primary {
            0x04 => {
                if open && native { LANG_ZH } else { LANG_EN }
            }
            0x11 => {
                if open && native { LANG_JA } else { LANG_EN }
            }
            0x12 => {
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
        // IMC_GETOPENSTATUS = 0x0005
        let open = query(ime, 0x0005)? != 0;
        let conv = query(ime, IMC_GETCONVERSIONMODE)?;
        Some((open, (conv & IME_CMODE_NATIVE as usize) != 0))
    }
}

/// 注入一次完整的 Ctrl+Space 按下与松开(回退路径)
unsafe fn send_ctrl_space() {
    fn key(vk: u16, up: bool) -> INPUT {
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

    let inputs = [
        key(VK_LCONTROL.0, false),
        key(VK_SPACE.0, false),
        key(VK_SPACE.0, true),
        key(VK_LCONTROL.0, true),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

