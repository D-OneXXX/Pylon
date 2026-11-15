//! Pylon 构建脚本（v2.3.0）：
//!
//! Windows 下用 tauri-build 编译 tauri.conf.json 上下文
//! （generate_context! 需要它生成的代码；内嵌 ui/index.html 资产），
//! 并通过 tauri-build 内置的 tauri-winres 管线把 `assets/icon.ico`
//! 嵌进 exe 当程序图标（任务栏 / Alt+Tab / 资源管理器 / 窗口默认图标）。
//! v2.2.0 起 tauri-build 已自带 Windows 资源编译，不再单独引入 winres，
//! 避免两个资源编译器重复写 .res 造成冲突；这里只把图标路径指到 assets/。
//!
//! Linux 云端：本脚本什么都不做（GUI 全在 #[cfg(windows)] 后面），
//! cargo check / cargo test 零系统库依赖。

fn main() {
    #[cfg(windows)]
    {
        // 图标更新后强制重跑构建脚本，避免旧的 exe 资源缓存。
        println!("cargo:rerun-if-changed=assets/icon.ico");
        tauri_build::try_build(
            tauri_build::Attributes::new().windows_attributes(
                tauri_build::WindowsAttributes::new().window_icon_path("assets/icon.ico"),
            ),
        )
        .expect("tauri-build 失败");
    }
}
