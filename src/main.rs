#![allow(non_snake_case)]

mod hooks;
mod ime_toggle;
mod overlay;

fn main() {
    // 按物理像素 1:1 渲染:否则在 125%/150% 缩放的屏幕上,
    // 浮层窗口会被系统位图拉伸,看起来发虚发糊。
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }

    // 低级键盘钩子要求安装线程有消息循环
    let hook_thread = std::thread::spawn(|| unsafe {
        hooks::run();
    });

    // overlay 窗口线程常驻,负责显示/隐藏语言指示浮层
    overlay::spawn_thread();

    hook_thread.join().unwrap();
    overlay::post_quit();
}
