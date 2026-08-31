//! 系统托盘:图标 + 右键菜单(启用/停止、退出)。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetMessageW, LoadIconW, MF_CHECKED, MF_STRING, MF_UNCHECKED, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_RIGHTALIGN, TrackPopupMenu, WM_COMMAND, WM_DESTROY, WM_USER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use windows::core::{PCWSTR, w};

/// 是否启用(供 hooks 检查)
static ENABLED: AtomicBool = AtomicBool::new(true);

static TRAY_HWND: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// 托盘自定义消息:图标事件(右键等)
const WM_TRAY_ICON: u32 = WM_USER + 1;
/// 菜单命令 ID
const IDM_TOGGLE: u32 = 1;
const IDM_AUTOSTART: u32 = 2;
const IDM_EXIT: u32 = 3;

pub fn spawn_thread() {
    std::thread::spawn(|| unsafe { tray_loop() });
}

#[allow(dead_code)]
pub fn post_quit() {
    let p = TRAY_HWND.load(Ordering::SeqCst);
    if !p.is_null() {
        let _ = unsafe { PostMessageW(Some(HWND(p)), WM_DESTROY, WPARAM(0), LPARAM(0)) };
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

unsafe fn tray_loop() {
    unsafe {
        let hinst = GetModuleHandleW(None).expect("GetModuleHandleW 失败");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: w!("CapsLockTrayWnd"),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        debug_assert!(atom != 0, "RegisterClassW 失败");

        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("CapsLockTrayWnd"),
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
        .expect("创建托盘窗口失败");

        TRAY_HWND.store(hwnd.0, Ordering::SeqCst);

        // 加载图标(嵌入在 exe 资源中, ID=1)
        let icon = LoadIconW(Some(hinst.into()), PCWSTR(1 as *const u16))
            .expect("加载图标失败(资源 ID=1)");

        // 注册托盘图标
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_ICON;
        nid.hIcon = icon;
        let tip = "CapsLock Switcher";
        let tip_utf16: Vec<u16> = tip.encode_utf16().collect();
        let copy_len = tip_utf16.len().min(nid.szTip.len() - 1);
        nid.szTip[..copy_len].copy_from_slice(&tip_utf16[..copy_len]);
        nid.szTip[copy_len] = 0;

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);

        // 消息循环
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = DispatchMessageW(&msg);
        }

        // 清理托盘图标
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        TRAY_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY_ICON => {
                // lparam 低 16 位 = 实际消息类型(WM_RBUTTONUP 等)
                let actual_msg = (lparam.0 as u32) & 0xFFFF;
                // WM_RBUTTONUP = 0x0205
                if actual_msg == 0x0205 {
                    show_context_menu(hwnd);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as u32;
                match id {
                    IDM_TOGGLE => {
                        let was_enabled = ENABLED.fetch_xor(true, Ordering::SeqCst);
                        let now_enabled = !was_enabled;
                        update_tray_tip(now_enabled);
                        println!(
                            "[tray] {}",
                            if now_enabled {
                                "已启用"
                            } else {
                                "已停止"
                            }
                        );
                    }
                    IDM_AUTOSTART => {
                        let target = !crate::autostart::is_enabled();
                        let ok = crate::autostart::set_enabled(target);
                        println!(
                            "[tray] 开机启动(管理员) {}",
                            if !ok {
                                "操作失败"
                            } else if target {
                                "已开启"
                            } else {
                                "已关闭"
                            }
                        );
                    }
                    IDM_EXIT => {
                        // 通知 hooks 线程退出
                        crate::hooks::post_quit();
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
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

unsafe fn show_context_menu(hwnd: HWND) {
    unsafe {
        let enabled = is_enabled();
        let menu = CreatePopupMenu().expect("CreatePopupMenu 失败");

        let autostart_enabled = crate::autostart::is_enabled();

        let toggle_text: Vec<u16> = (if enabled { "停止" } else { "启用" })
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let autostart_text: Vec<u16> = "开机启动(管理员)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let exit_text: Vec<u16> = "退出".encode_utf16().chain(std::iter::once(0)).collect();

        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_TOGGLE as usize,
            PCWSTR(toggle_text.as_ptr()),
        );
        let autostart_flag = MF_STRING
            | if autostart_enabled {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let _ = AppendMenuW(
            menu,
            autostart_flag,
            IDM_AUTOSTART as usize,
            PCWSTR(autostart_text.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_EXIT as usize,
            PCWSTR(exit_text.as_ptr()),
        );

        // TrackPopupMenu 需要 SetForegroundWindow 才能在点击外部时关闭
        let _ = SetForegroundWindow(hwnd);

        // 获取鼠标位置
        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);

        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

fn update_tray_tip(enabled: bool) {
    unsafe {
        let p = TRAY_HWND.load(Ordering::SeqCst);
        if p.is_null() {
            return;
        }
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = HWND(p);
        nid.uID = 1;
        nid.uFlags = NIF_TIP;
        let tip = if enabled {
            "CapsLock Switcher (已启用)"
        } else {
            "CapsLock Switcher (已停止)"
        };
        let tip_utf16: Vec<u16> = tip.encode_utf16().collect();
        let copy_len = tip_utf16.len().min(nid.szTip.len() - 1);
        nid.szTip[..copy_len].copy_from_slice(&tip_utf16[..copy_len]);
        nid.szTip[copy_len] = 0;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}
