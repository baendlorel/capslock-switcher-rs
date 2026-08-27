//! capslock-switcher-rs
//!
//! 1. CapsLock           -> Ctrl+Space
//! 2. Alt+CapsLock       -> 原生 CapsLock(切换大小写)

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicI16, Ordering};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_CAPITAL, VK_LCONTROL,
    VK_LMENU, VK_RMENU, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_INJECTED, LLKHF_UP, MSG,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// 标记我们用 SendInput 注入的按键,避免钩子再次拦截造成死循环。
const INJECTED_FLAG: usize = 0x1;

/// 缓存 Alt 键状态:0 = 松开,非 0 = 按下。
/// 用 GetAsyncKeyState 轮询在低级钩子里不可靠,直接在钩子回调里跟踪。
static ALT_DOWN: AtomicI16 = AtomicI16::new(0);

fn main() {
    // 低级键盘钩子要求安装线程有消息循环
    let hook_thread = std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)
            .expect("安装键盘钩子失败(尝试以管理员身份运行)");

        println!("capslock-switcher 已启动:");
        println!("  CapsLock       -> Ctrl+Space");
        println!("  Alt+CapsLock   -> 原生 CapsLock(切换大小写)");
        println!("按 Ctrl+C 或关闭窗口退出。");

        let mut msg = MSG::default();
        // GetMessageW 在收到 WM_QUIT 前一直阻塞,-1 表示错误
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}

        let _ = UnhookWindowsHookEx(hook);
    });

    hook_thread.join().unwrap();
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
        let alt_down =
            kb.flags.contains(LLKHF_ALTDOWN) || ALT_DOWN.load(Ordering::SeqCst) != 0;

        if alt_down {
            // Alt+CapsLock:透传原生 CapsLock 行为(切换大小写)
            CallNextHookEx(None, code, wparam, lparam)
        } else {
            // CapsLock:吞掉,注入 Ctrl+Space
            send_ctrl_space();
            LRESULT(1) // 非零返回值 = 吞掉该按键,不传递给系统
        }
    }
}

/// 注入一次完整的 Ctrl+Space 按下与松开
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
