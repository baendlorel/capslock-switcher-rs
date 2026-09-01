//! 共享静态资源:内嵌图片字节及 GDI+ 位图加载辅助函数。

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Graphics::GdiPlus::{GdipCreateBitmapFromStream, GpBitmap, Status};
use windows::Win32::System::Com::STREAM_SEEK_SET;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;

/// 内嵌的 umbral-keys.png 字节(启动就绪浮层 + 关于窗口左侧图 + 用前必读窗口顶部图共用)
pub static UMBRAL_KEYS_PNG: &[u8] = include_bytes!("../assets/umbral-keys.png");

/// 从内存字节加载 GDI+ 位图;调用方负责后续 GdipDisposeImage 释放。失败返回空指针。
pub unsafe fn load_bitmap(bytes: &[u8]) -> *mut GpBitmap {
    unsafe {
        let Ok(stream) = CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true) else {
            return std::ptr::null_mut();
        };
        let _ = stream.Write(bytes.as_ptr() as *const _, bytes.len() as u32, None);
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);

        let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromStream(&stream, &mut bitmap);
        if st != Status(0) {
            return std::ptr::null_mut();
        }
        bitmap
    }
}
