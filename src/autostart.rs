//! 开机启动(管理员权限):通过任务计划程序注册一个登录时以最高权限运行的任务。
//! 创建/删除任务都需要一次 UAC 提权确认(schtasks.exe 以 runas 方式启动)。

use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

/// 计划任务名称
const TASK_NAME: &str = "CapsLockSwitcherAutostart";
/// CREATE_NO_WINDOW:运行控制台程序时不弹出窗口
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 查询任务是否已注册(普通权限即可,不弹窗)
pub fn is_enabled() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 开启或关闭"开机启动(管理员)"。会触发一次 UAC 提权确认。
/// 返回是否操作成功。
pub fn set_enabled(enable: bool) -> bool {
    let params = if enable {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let exe = exe.to_string_lossy();
        format!("/Create /TN \"{TASK_NAME}\" /TR \"\\\"{exe}\\\"\" /SC ONLOGON /RL HIGHEST /F")
    } else {
        format!("/Delete /TN \"{TASK_NAME}\" /F")
    };

    run_elevated_schtasks(&params)
}

/// 以管理员身份运行 schtasks.exe 并等待完成,返回退出码是否为 0。
fn run_elevated_schtasks(params: &str) -> bool {
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = "schtasks.exe\0".encode_utf16().collect();
    let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params_w.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };

        if ShellExecuteExW(&mut info).is_err() || info.hProcess.is_invalid() {
            return false;
        }

        let _ = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let got_code = GetExitCodeProcess(info.hProcess, &mut code).is_ok();
        let _ = CloseHandle(info.hProcess);
        got_code && code == 0
    }
}
