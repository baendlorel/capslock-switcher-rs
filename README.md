# CapsLock Switcher RS 输入法切换工具

CapsLock Switcher RS由Rust开发，旨在安全稳定，解决AHK版的闪退问题。这是一个轻量级的输入法切换工具，允许你使用 CapsLock 键在中英文输入法之间切换，类似于 macOS 上的行为。消除了使用 Shift 键切换输入模式的不便。

<p align="center">
   <img src="./assets/yukari-smugshrug.png" width="240" />
   <p align="center">没有英文版Readme哦</p>
</p>

**此脚本的作用是将`CapsLock`映射到`Ctrl + Space`，因此需要先在设置中将输入法切换快捷键改为`Ctrl + Space`。**

## 功能

- **CapsLock 切换**: 按下 CapsLock 键在中英文输入模式之间切换
- **Alt+CapsLock**: 按下 Alt+CapsLock 可触发原本的锁定大写功能
- **日语**: 在英文模式、片假名、平假名模式中切换
- **系统托盘集成**: 右键点击系统托盘图标进行快速控制
- **开机启动（管理员）**: 可设置为系统登录后以管理员权限静默启动
- **暂停/恢复**: 通过托盘菜单临时禁用/启用脚本
- **跨应用兼容性**: 适用于大多数 Windows 应用程序和输入法

## 安装方法

1. 从 [发布页面](#) 下载最新版本 (或自己构建)
2. 运行 `capslock-switcher-rs-vX.X.X.exe`

## 许可证

MIT
