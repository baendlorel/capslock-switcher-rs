//! capslock-switcher-rs
//!
//! 1. CapsLock      -> API 直切输入法中/英模式(失败回退 Ctrl+Space)
//! 2. Alt+CapsLock  -> 原生 CapsLock(切换大小写)+ 大写/小写浮层
//! 3. 切换后在屏幕中央显示平滑的指示浮层(GDI+ 抗锯齿)

#![allow(non_snake_case)]

mod hooks;
mod ime_toggle;
mod overlay;

fn main() {
    // 低级键盘钩子要求安装线程有消息循环
    let hook_thread = std::thread::spawn(|| unsafe {
        hooks::run();
    });

    // overlay 窗口线程常驻,负责显示/隐藏语言指示浮层
    overlay::spawn_thread();

    hook_thread.join().unwrap();
    overlay::post_quit();
}
