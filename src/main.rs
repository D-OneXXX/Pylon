#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]
//! Pylon 入口：读配置 -> 起 NetworkManager（热切换架构） -> 托盘驻留 -> Tauri 2 窗口。
//! v0.8 砍掉了 GPUI；v2.0 砍掉了网页面板；v2.1 砍掉了 eframe/egui 换 Slint；
//! v2.2.0 砍掉 Slint，换 Tauri 2 + 内嵌单文件 Shadcn 界面（关窗即销毁，待机 ~5MB）。
//! Linux 下没有托盘：只跑服务，主线程原地待机（云端验证就这么跑）。
//! v0.9：网络生命周期交给 NetworkManager（network.rs），保存配置后热切换，
//! 不再需要重启。v2.1.3：只保留 Tailscale 模式，
//! Cloudflare / WS 反向 / iroh P2P 三种模式及 ws.rs / relay.rs / tls.rs 已删除。
//! v1.0（附加 A）：Windows 下整个 exe 脱离控制台，只留系统托盘；
//! 子进程（cargo / git / python）统一加 CREATE_NO_WINDOW，
//! 后台跑命令时桌面不再闪黑框。

mod bridge;
mod config;
#[cfg(windows)]
mod job;
mod network;
mod tasks;
#[cfg(windows)]
mod tauri_ui;
#[cfg(windows)]
mod tray;

use bridge::{push_log, ServerState, SharedLogs, VERSION};
use config::BridgeConfig;
use network::NetworkManager;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// 配置文件查找顺序（v2.3.3，审查报告 P1 #4：CWD 不再优先）。
///
/// 1. `--config <路径>` / `--config=<路径>`：显式指定，最可信；
/// 2. `PYLON_CONFIG` 环境变量：同上（服务包装器 / 计划任务可用）；
/// 3. exe 同级目录的 `bridge-config.json`：部署形态的默认；
/// 4. **仅 debug 构建**才回退到当前工作目录（`cargo run` 的开发形态）。
///
/// 旧实现是「CWD 优先、exe 同级兜底」：快捷方式 / 计划任务 / 被其他程序 spawn
/// 时 CWD 是可控的，攻击者在 CWD 放一份 `bridge-config.json`，Pylon 就会用对方的
/// token、roots、白名单启动——等于把整台机器交出去。release 构建不再看 CWD。
fn find_config_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 0;
    while i < args.len() {
        if let Some(v) = args[i].strip_prefix("--config=") {
            if !v.trim().is_empty() {
                return PathBuf::from(v);
            }
        }
        if args[i] == "--config" {
            if let Some(v) = args.get(i + 1) {
                if !v.trim().is_empty() {
                    return PathBuf::from(v);
                }
            }
        }
        i += 1;
    }
    if let Ok(v) = std::env::var("PYLON_CONFIG") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    let exe_dir_cfg = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("bridge-config.json")));
    if let Some(c) = &exe_dir_cfg {
        if c.exists() {
            return c.clone();
        }
    }
    if cfg!(debug_assertions) {
        if let Ok(d) = std::env::current_dir() {
            let c = d.join("bridge-config.json");
            if c.exists() {
                eprintln!(
                    "[pylon] 注意：debug 构建从当前目录加载配置 {}（发布版只看 exe 同级 / --config）",
                    c.display()
                );
                return c;
            }
        }
    }
    // 都没有时：配置的保存目标收敛到 exe 同级目录
    //（工作区文件夹里不生成任何 Pylon 配置文件）。
    exe_dir_cfg.unwrap_or_else(|| PathBuf::from("bridge-config.json"))
}

fn main() {
    let config_path = find_config_path();
    // v2.1.2：bridge-config.json 不存在时，不再生成示例文件并退出，
    // 改用内存默认配置 + 自动生成的随机 Token 直接启动，
    // 不在用户的工作区文件夹内生成任何多余的配置文件。
    // Token 见设置弹窗（可复制）；工作区在设置里填，保存后写到 exe 同级目录。
    let cfg = if config_path.exists() {
        match BridgeConfig::load_or_create_example(&config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[pylon] {e}");
                std::process::exit(1);
            }
        }
    } else {
        let mut c = BridgeConfig::default();
        c.token = Uuid::new_v4().simple().to_string();
        eprintln!("[pylon] 未找到 bridge-config.json，使用内存默认配置启动（Token 已随机生成，见设置弹窗）");
        c
    };
    // v1.0（模块三）：单实例锁。已有实例在跑时，打开面板后直接退出，
    // 免得两个桥抢同一个端口 / 配置。
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, FALSE};
        use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};
        use windows_sys::w;
        let mutex =
            unsafe { CreateMutexW(std::ptr::null(), 1, w!("Global\\PylonBridgeSingleton")) };
        if !mutex.is_null() {
            // 句柄是裸指针（Copy 类型）：不需要 forget，变量活到 main 结束即可；
            // 内核互斥体只在 CloseHandle 或进程退出时释放，这里两者都不做，
            // 所以单实例保护在整个进程生命周期内都有效。
            let _ = mutex;
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                // v2.3.1：第二实例不再静默退出——先触发命名 Event 唤醒主实例再退出。
                // 主实例可能正处在“窗口已销毁”的托盘待机状态，双击 exe 否则毫无反应；
                // 主实例托盘线程监听到信号后立即重建并置顶窗口（见 tray.rs）。
                // 事件不存在（主实例是旧版）时 CreateEventW 会新建一个，
                // SetEvent 无人监听也无害，直接退出即可。
                let event = unsafe {
                    CreateEventW(
                        std::ptr::null(),
                        FALSE,
                        FALSE,
                        w!("Local\\PylonShowWindowEvent"),
                    )
                };
                if !event.is_null() {
                    unsafe { SetEvent(event) };
                }
                // v2.0：网页面板已删，重复启动不再开浏览器——窗口从托盘图标打开。
                std::process::exit(0);
            }
        }
        // CreateMutexW 返回 NULL（例如 Global 命名空间没权限）：不拦启动，
        // 只是单实例保护失效，服务照常跑。
    }
    let logs: SharedLogs = Arc::new(std::sync::Mutex::new(Vec::new()));
    // v1.0（模块五）：动态日志落盘到 exe 同目录的 pylon-activity.log，
    // 启动时把末尾 50 条读回内存动态列表。
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pylon-activity.log")))
        .unwrap_or_else(|| PathBuf::from("pylon-activity.log"));
    // v2.1：init_log_file 返回恢复条数，写进启动日志，重启后一眼能确认
    // 历史动态回读生效（之前只剩 1 条启动日志时无从判断）。
    let restored = bridge::init_log_file(&logs, log_path);
    // v1.0.2：不再生成动态 UI 密钥——/ui/* 只认本机 + 固定请求头 X-Pylon-UI: 1，
    // 重启后面板无缝恢复。
    let state = Arc::new(ServerState::new(
        cfg,
        config_path,
        Arc::clone(&logs),
    ));
    push_log(
        &logs,
        format!("Pylon v{VERSION} 启动（已恢复 {restored} 条历史动态）"),
    );
    // 网络管理器：HTTP 监听（Tailscale 模式），端口变了热切换。
    let manager = Arc::new(NetworkManager::new(Arc::clone(&state)));
    // 配置变更通知通道：/ui/save-config 发信号，watch 线程热切换。
    let (notify_tx, notify_rx) = std::sync::mpsc::channel::<()>();
    // v2.3.3：锁中毒不再 unwrap（启动路径 panic 会直接让进程死得莫名其妙）。
    match state.notify_tx.lock() {
        Ok(mut g) => *g = Some(notify_tx),
        Err(poisoned) => *poisoned.into_inner() = Some(notify_tx),
    }
    {
        let m = Arc::clone(&manager);
        std::thread::spawn(move || m.watch(notify_rx));
    }
    if let Err(e) = manager.start() {
        eprintln!("[pylon] 启动服务失败：{e}");
        std::process::exit(1);
    }
    // v2.2.0：托盘独立线程（Tauri 占主线程），窗口关了再让托盘收尾。
    // Linux：原地待机（无头服务，不开窗）。
    #[cfg(windows)]
    {
        // 关窗即销毁架构：点 X / 最小化到托盘 -> window.destroy()，
        // WebView2 进程释放，待机 ~5MB；托盘双击 / 新审批动态重建窗口。
        let tray_bridge = Arc::new(tauri_ui::TrayBridge::new());
        let tray_state = Arc::clone(&state);
        let tray_bridge_clone = Arc::clone(&tray_bridge);
        let tray_handle =
            std::thread::spawn(move || tray::run_tray(tray_state, tray_bridge_clone));
        tauri_ui::run_tauri(Arc::clone(&state));
        // 窗口真的关了：让托盘线程删图标、收尾，再退出进程。
        tray::shutdown_tray();
        let _ = tray_handle.join();
        std::process::exit(0);
    }
    #[cfg(not(windows))]
    {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}
