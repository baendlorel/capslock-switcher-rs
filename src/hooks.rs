//! 低级键盘钩子:拦截 CapsLock,改键行为:
//! 1. CapsLock      -> 投递切换请求,浮层线程执行 API 直切(失败回退 Ctrl+Space)
//! 2. Alt+CapsLock  -> 透传原生大小写切换,并显示 大写/小写 浮层
//!
//! 铁律:钩子回调里只做 O(1) 的判断和 PostMessage,任何阻塞调用
//! (AttachThreadInput/SendMessageTimeout/SendInput)都会让系统超时摘钩。

use std::sync::atomic::{AtomicI16, Ordering};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CAPITAL, VK_LMENU, VK_RMENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_INJECTED, LLKHF_UP, MSG,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::overlay;

// 标记注入按键的 dwExtraInfo(注入逻辑在 ime_toggle,此处仅识别)
use crate::ime_toggle::INJECTED_FLAG;

/// GetKeyState 返回值的最低位 = toggle 状态(CapsLock 灯亮/灭)
const KEYSTATE_TOGGLED: i16 = 0x0001;

/// 缓存 Alt 键状态:0 = 松开,非 0 = 按下。
/// 用 GetAsyncKeyState 轮询在低级钩子里不可靠,直接在钩子回调里跟踪。
static ALT_DOWN: AtomicI16 = AtomicI16::new(0);

/// 安装低级键盘钩子并进入消息循环,阻塞直到收到 WM_QUIT。
pub unsafe fn run() {
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)
            .expect("安装键盘钩子失败(尝试以管理员身份运行)");

        println!("capslock-switcher 已启动:");
        println!("  CapsLock       -> API 直切输入法中/英(回退 Ctrl+Space)");
        println!("  Alt+CapsLock   -> 原生大小写切换 + 大写/小写浮层");
        println!("按 Ctrl+C 或关闭窗口退出。");

        let mut msg = MSG::default();
        // GetMessageW 在收到 WM_QUIT 前一直阻塞,-1 表示错误
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}

        let _ = UnhookWindowsHookEx(hook);
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let is_up = kb.flags.contains(LLKHF_UP);

        // 跟踪 Alt 状态:低级钩子拿到的是 VK_LMENU(0xA4)/VK_RMENU(0xA5),
        // 不是笼统的 VK_MENU(0x12),按后者匹配永远不命中。
        if kb.vkCode == VK_LMENU.0 as u32 || kb.vkCode == VK_RMENU.0 as u32 {
            ALT_DOWN.store(if is_up { 0 } else { 1 }, Ordering::SeqCst);
        }

        // 只处理按下事件;松开事件一律放行,避免按键卡住
        let msg = wparam.0 as u32;
        if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 放行我们自己注入的按键,防止无限递归
        if kb.flags.contains(LLKHF_INJECTED) && kb.dwExtraInfo == INJECTED_FLAG {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        if kb.vkCode != VK_CAPITAL.0 as u32 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // ---- 走到这里说明是真实的 CapsLock 按下 ----

        // 双保险:LLKHF_ALTDOWN 表示"此事件发生时 Alt 正被按住",
        // 再叠加手动跟踪的状态(钩子装好后 Alt 一直没松过等情况)。
        let alt_down = kb.flags.contains(LLKHF_ALTDOWN) || ALT_DOWN.load(Ordering::SeqCst) != 0;

        if alt_down {
            // Alt+CapsLock:透传给系统切换大小写;松开时 CapsLock 状态已翻转,
            // 那时读 GetKeyState 才是切换后的新状态,所以浮层放在松开事件里显示。
            if is_up {
                let caps_on = GetKeyState(VK_CAPITAL.0 as i32) & KEYSTATE_TOGGLED != 0;
                overlay::show_caps_overlay(caps_on);
            }
            CallNextHookEx(None, code, wparam, lparam)
        } else {
            // CapsLock:吞掉,向浮层线程投递「切换 + 显示」请求。
            // 切换动作(AttachThreadInput/SendMessageTimeout/SendInput)都是阻塞调用,
            // 绝不能在低级钩子回调里执行——超时几次后系统会静默摘除钩子。
            overlay::show_language_overlay();
            LRESULT(1) // 非零返回值 = 吞掉该按键,不传递给系统
        }
    }
}
