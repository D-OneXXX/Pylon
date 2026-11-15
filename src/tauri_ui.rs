//! v2.3.0 Tauri 2 GUI：无边框一体化顶栏 + 关窗即销毁，待机 ~5MB。
//!
//! - 界面是内嵌单文件 `ui/index.html`（Shadcn Zinc 体系，无 Node/npm/CDN），
//!   经 tauri 资产协议加载（tauri.conf.json 的 frontendDist 指向 ui/）。
//! - 所有数据走 `invoke('ui_request')` 桥到 `dispatch_request`
//!   （本机 loopback + X-Pylon-UI: 1），webview 不开 HTTP 端口、无 CORS。
//! - v2.2.1 线程模型：
//!   * **窗口创建只发生在 Tauri 主线程**。托盘线程（tray.rs）只调
//!     `TrayBridge::request_show()`，由它把「显示 / 聚焦 / 重建」闭包投递到
//!     `AppHandle::run_on_main_thread`，托盘线程自己绝不建窗；
//!     setup 里的首窗创建本来就在主线程，保持不变。
//!   * `ui_request` 是 async 命令：入口先克隆出 `Arc<ServerState>`，
//!     再把 `dispatch_request` 丢进 `spawn_blocking` 线程池。文件 / 子进程 /
//!     任务轮询这类阻塞活不再占 Tauri 主线程，也不堵 webview 的 IPC。
//! - **无边框一体化顶栏（v2.3.0）**：Windows 原生标题栏由
//!   `decorations(false)` 去掉，前端 32px 系统条挂
//!   `data-tauri-drag-region`（拖拽 / 双击最大化），最小化 / 最大化切换 /
//!   关闭分别走 `window_minimize` / `window_toggle_maximize` /
//!   `window_close_destroy` 命令；关闭仍是 destroy 到托盘，不退出进程。
//!   `resizable(true)` 保留边缘缩放，`shadow(true)` 保留系统阴影。
//! - **关窗即销毁**：点 X / [最小化到托盘] → `window.destroy()`，
//!   WebviewWindow 与 msedgewebview2.exe 进程彻底释放；
//!   `RunEvent::ExitRequested` 默认 `api.prevent_exit()` 驻留托盘，
//!   仅托盘「退出」/ 界面 [退出] 置位 EXIT_FLAG 后才真正退出。
//! - **v2.3.1 关窗几何恢复**：销毁前把 `outer_position` / `inner_size` /
//!   `is_maximized` 存进进程级静态 `LAST_WINDOW_GEOMETRY`，重建时恢复，
//!   不再每次重置回屏幕正中央的默认大小。
//! - 托盘双击 / 打开面板 / 新审批入队 → 窗口在则聚焦，不在则
//!   经主线程投递 `WebviewWindowBuilder` 动态重建。
//! - WebView2 缓存重定向到 `%TEMP%\pylon-webview2`，exe 旁不生成
//!   EBWebView 文件夹（单文件纯净）。
//!
//! 本文件只在 Windows 下编译（main.rs 的 `#[cfg(windows)] mod tauri_ui;`），
//! Linux 云端零系统库依赖。

use crate::bridge::{dispatch_request_internal, ServerState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// 全局 AppHandle：托盘线程（tray.rs）经它重建 / 聚焦窗口。
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
/// 退出标志：仅托盘「退出 Pylon」/ 界面 [退出] 置位；
/// ExitRequested 看到它才放行，否则 prevent_exit 驻留托盘。
static EXIT_FLAG: AtomicBool = AtomicBool::new(false);
/// v2.2.0：窗口创建互斥——首次启动的 WebView2 初始化可能耗时数秒，
/// 期间托盘双击 / 新审批触发 request_show 时不要并发再建一个同 label
/// 窗口（tauri 会报 LabelExists，且并发创建曾让首窗迟迟不出现）。
/// v2.2.1：创建已统一在主线程，这个原子量继续保护 setup 首窗与投递进来的
/// 重建任务（以及重复投递）不会重复建窗。
static CREATING: AtomicBool = AtomicBool::new(false);
/// v2.3.1：关窗前存的窗口几何（位置 / 尺寸 / 是否最大化）。
/// 点 X / [最小化到托盘] 销毁窗口前由 save_geometry 写入，
/// 下次 create_main_window 重建时恢复——不再每次重置回屏幕正中央的默认大小。
/// 最大化状态下只记 maximized=true（位置/尺寸是最大化后的全屏值，恢复时无用）。
#[derive(Clone, Copy)]
struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}
static LAST_WINDOW_GEOMETRY: std::sync::Mutex<Option<WindowGeometry>> =
    std::sync::Mutex::new(None);

/// v2.3.1：`Window` 与 `WebviewWindow` 都有 outer_position / inner_size /
/// is_maximized，但两者没有共享 trait，`WebviewWindow` 也不 Deref 到 `Window`
///（直接传 `&WebviewWindow` 给 `&Window` 参数会报 E0308），
/// 所以用一个本地小 trait 统一读几何，返回 plain tuple，不碰 dpi 类型名。
trait WindowGeometryRead {
    fn read_geometry(&self) -> (i32, i32, u32, u32, bool);
}

impl WindowGeometryRead for tauri::Window {
    fn read_geometry(&self) -> (i32, i32, u32, u32, bool) {
        let maximized = self.is_maximized().unwrap_or(false);
        let (x, y) = self.outer_position().map(|p| (p.x, p.y)).unwrap_or((0, 0));
        let (width, height) = self
            .inner_size()
            .map(|s| (s.width, s.height))
            .unwrap_or((1000, 880));
        (x, y, width, height, maximized)
    }
}

impl WindowGeometryRead for tauri::WebviewWindow {
    fn read_geometry(&self) -> (i32, i32, u32, u32, bool) {
        let maximized = self.is_maximized().unwrap_or(false);
        let (x, y) = self.outer_position().map(|p| (p.x, p.y)).unwrap_or((0, 0));
        let (width, height) = self
            .inner_size()
            .map(|s| (s.width, s.height))
            .unwrap_or((1000, 880));
        (x, y, width, height, maximized)
    }
}

/// 关窗销毁前调用：读当前窗口的 outer_position / inner_size / is_maximized
/// 存进 LAST_WINDOW_GEOMETRY。读不到就保持旧值（不覆盖）。
fn save_geometry<W: WindowGeometryRead>(window: &W) {
    let (x, y, width, height, maximized) = window.read_geometry();
    if let Ok(mut slot) = LAST_WINDOW_GEOMETRY.lock() {
        *slot = Some(WindowGeometry {
            x,
            y,
            width,
            height,
            maximized,
        });
    }
}
const WINDOW_LABEL: &str = "main";

// ---------------------------------------------------------------------------
// 托盘桥（tray.rs 的调用点签名与 Slint 时代一致）
// ---------------------------------------------------------------------------

pub struct TrayBridge;

impl TrayBridge {
    pub fn new() -> Self {
        Self
    }

    /// 托盘双击 / 打开面板 / 新审批入队：窗口在则显示并聚焦，
    /// 不在（已被 destroy）则动态重建。
    ///
    /// v2.2.1：只「请求」，不直接动手——窗口 / WebView2 的创建与显示
    /// 全部投递到 Tauri 主线程（`run_on_main_thread`）。托盘是独立的
    /// Win32 消息线程，跨线程建窗会违反 Tauri 的主线程约束。
    pub fn request_show(&self) {
        let Some(app) = APP.get().cloned() else { return };
        // 闭包要以 move 捕获 AppHandle，而 run_on_main_thread 又要借用
        // receiver，直接 move 会撞 E0505，所以再 clone 一份给主线程任务。
        let app_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app_main.get_webview_window(WINDOW_LABEL) {
                let _ = w.show();
                let _ = w.set_focus();
                return;
            }
            // 首窗还在创建中：不并发重建，等它自己出现。
            if CREATING
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return;
            }
            let result = create_main_window(&app_main);
            CREATING.store(false, Ordering::SeqCst);
            if let Ok(w) = result {
                let _ = w.show();
                let _ = w.set_focus();
            }
        });
    }

    /// 托盘「退出 Pylon」：置位退出标志 -> 销毁窗口 -> app.exit(0)。
    /// ExitRequested 看到标志放行，run_tauri 返回，main.rs 收尾托盘。
    pub fn request_quit(&self) {
        EXIT_FLAG.store(true, Ordering::SeqCst);
        if let Some(app) = APP.get() {
            if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
                let _ = w.destroy();
            }
            app.exit(0);
        } else {
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// 窗口创建（WebView2 缓存重定向 + 图标）
// ---------------------------------------------------------------------------

fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    // 窗口 / 任务栏图标用 exe 内嵌资源（tauri-build 嵌的 icons/icon.ico），
    // Windows 下 WebView 窗口自动继承，无需再解码 PNG。
    let mut builder =
        WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
            .title("Pylon")
            .inner_size(1000.0, 880.0)
            .min_inner_size(780.0, 600.0)
            // v2.3.0：去掉 Windows 原生白色标题栏，自绘一体化顶栏；
            // 保留可缩放 + 系统阴影/边缘拖拽缩放能力。
            .decorations(false)
            .resizable(true)
            .shadow(true)
            // v2.2.0：WebView2 用户数据目录重定向到 TEMP——
            // exe 同级目录不生成 EBWebView 缓存文件夹（单文件纯净）。
            .data_directory(std::env::temp_dir().join("pylon-webview2"));
    // v2.3.1：有历史几何就恢复（位置/尺寸；最大化时只恢复最大化状态）。
    if let Some(g) = LAST_WINDOW_GEOMETRY
        .lock()
        .ok()
        .and_then(|slot| *slot)
    {
        if g.maximized {
            builder = builder.maximized(true);
        } else {
            builder = builder
                .position(g.x as f64, g.y as f64)
                .inner_size(g.width as f64, g.height as f64);
        }
    }
    builder.build()
}

// ---------------------------------------------------------------------------
// Tauri 命令（webview <-> 核心服务层）
// ---------------------------------------------------------------------------

/// UI 数据桥：把 /ui/* 请求直接交给 dispatch_request_internal（Tauri 内部调用），
/// 不经 HTTP、无 CORS、不依赖监听端口。
///
/// v2.3.1：/ui/* 只允许这条内部通道——HTTP Socket（8765）收到 /ui/* 一律 403，
/// 本机第三方脚本再也不能带 X-Pylon-UI: 1 偷调管理接口。ui_request 是唯一的
/// 内部调用方（见 bridge.rs 的 dispatch_request_internal / internal 门控）。
///
/// v2.2.1：改成 async 命令 + `spawn_blocking`。`dispatch_request` 里混着
/// 文件读写 / 子进程 / 任务轮询等阻塞活，同步命令会占住 Tauri 主线程，
/// 拖慢窗口与托盘交互；现在先克隆出 `Arc<ServerState>` 再丢进线程池，
/// `tauri::State` 只是取状态的借用守卫，借一下拿到 Arc 后就不再使用，
/// 不会跟阻塞任务一起被带进 await。
///
/// 返回 `Result` 是 tauri 命令宏的硬性要求（async 命令参数里带引用时必须
/// 返回 Result）。业务失败仍以 `{"ok":false,"error":...}` 的 Ok 值交给前端，
/// 保持 `invoke('ui_request')` 的解析约定不变。
#[tauri::command]
async fn ui_request(
    state: tauri::State<'_, Arc<ServerState>>,
    method: String,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let state = Arc::clone(&state);
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let (p, query) = match path.split_once('?') {
            Some((p, q)) => (p.to_string(), crate::bridge::parse_query(q)),
            None => (path.clone(), std::collections::HashMap::new()),
        };
        let mut headers = std::collections::HashMap::new();
        headers.insert("x-pylon-ui".to_string(), "1".to_string());
        // /api/* 需要 token 鉴权：UI 桥是本机 webview 专用通道，
        // 直接注入当前 token（等价于本地点按，不经远程）。
        if p.starts_with("/api/") {
            headers.insert("x-bridge-token".to_string(), state.config().token);
        }
        let body_bytes = body.map(|v| v.to_string()).unwrap_or_default();
        let resp = dispatch_request_internal(
            &state,
            &method,
            &path,
            &p,
            &query,
            &headers,
            body_bytes.as_bytes(),
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        );
        serde_json::from_slice(&resp.body).unwrap_or_else(|_| {
            serde_json::json!({"ok": false, "error": format!("HTTP {}", resp.code)})
        })
    })
    .await;
    match joined {
        Ok(value) => Ok(value),
        // dispatch_request panic 才会走到这里：别把 invoke 变成 reject，
        // 仍回一个前端能解析的错误对象。
        Err(err) => Ok(serde_json::json!({
            "ok": false,
            "error": format!("ui_request 阻塞任务失败：{err}"),
        })),
    }
}

/// [浏览…] 工作区：独立线程跑 rfd 原生目录选择器（阻塞对话框不占主线程）。
#[tauri::command]
fn browse_folder() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let dir = rfd::FileDialog::new().pick_folder();
        let _ = tx.send(dir.map(|p| p.display().to_string()));
    });
    rx.recv().unwrap_or(None)
}

/// [浏览…] 主程序单文件：独立线程跑 rfd 原生文件选择器。
#[tauri::command]
fn browse_file() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let file = rfd::FileDialog::new().pick_file();
        let _ = tx.send(file.map(|p| p.display().to_string()));
    });
    rx.recv().unwrap_or(None)
}

/// 设置弹窗 [复制] Token。
#[tauri::command]
fn copy_text(text: String) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}

/// 界面 [退出]：置位退出标志 -> 销毁窗口 -> 退出进程。
#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
    EXIT_FLAG.store(true, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        let _ = w.destroy();
    }
    app.exit(0);
}

/// [最小化到托盘] / 点 X：destroy 窗口（WebView2 进程一并释放），
/// ExitRequested 被 prevent_exit 拦下，进程驻留托盘，待机 ~5MB。
/// v2.3.1：销毁前先存几何，重建时恢复位置/尺寸。
#[tauri::command]
fn minimize_to_tray(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        save_geometry(&w);
        let _ = w.destroy();
    }
    trim_working_set_soon();
}

// ---------------------------------------------------------------------------
// v2.3.0 无边框顶栏的窗口控制命令
// ---------------------------------------------------------------------------

/// 顶栏 `—`：系统最小化到任务栏（不销毁窗口）。
#[tauri::command]
fn window_minimize(window: tauri::Window) {
    let _ = window.minimize();
}

/// 顶栏 `□`：最大化 / 还原切换。
#[tauri::command]
fn window_toggle_maximize(window: tauri::Window) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

/// 顶栏 `×`：与点系统 X 一致——destroy 窗口、释放 WebView2、
/// 进程驻留托盘（ExitRequested 会 prevent_exit）。
/// v2.3.1：销毁前先存几何，重建时恢复位置/尺寸。
#[tauri::command]
fn window_close_destroy(window: tauri::Window) {
    save_geometry(&window);
    let _ = window.destroy();
    trim_working_set_soon();
}

/// 显式拖拽入口：给自绘标题栏 / 品牌区留一个可调用的 start_dragging；
/// 空白 drag region 本身由 Tauri 的 data-tauri-drag-region 接管。
#[tauri::command]
fn window_start_drag(window: tauri::Window) {
    let _ = window.start_dragging();
}

/// v2.2.0：关窗即销毁后的内存回落——destroy 释放 WebView2 进程后，
/// 本进程的工作集里还留着已释放页面的驻留副本；延迟 500ms 等 destroy
/// 收尾，再 EmptyWorkingSet 把可分页页面交还 OS，待机工作集回落至 ~5MB。
fn trim_working_set_soon() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            EmptyWorkingSet(GetCurrentProcess());
        }
    });
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 创建 Tauri 应用、跑事件循环。返回时窗口已真正退出（点 X 只是
/// destroy 到托盘，不返回）；main.rs 随后收尾托盘线程并退出进程。
pub fn run_tauri(state: Arc<ServerState>) {
    let app = tauri::Builder::default()
        .manage(Arc::clone(&state))
        .invoke_handler(tauri::generate_handler![
            ui_request,
            browse_folder,
            browse_file,
            copy_text,
            exit_app,
            minimize_to_tray,
            window_minimize,
            window_toggle_maximize,
            window_close_destroy,
            window_start_drag
        ])
        .setup(|app| {
            // v2.2.1：setup 本身跑在 Tauri 主线程，首窗照旧在这里创建；
            // 托盘线程的重建请求统一走 request_show 的主线程投递。
            let _ = APP.set(app.handle().clone());
            CREATING.store(true, Ordering::SeqCst);
            let result = create_main_window(app.handle());
            CREATING.store(false, Ordering::SeqCst);
            result?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Tauri 构建失败");
    app.run(|app, event| match event {
        // 关窗即销毁的核心：默认不许退出（驻留托盘），
        // 仅 EXIT_FLAG 置位（托盘「退出」/ 界面 [退出]）才放行。
        tauri::RunEvent::ExitRequested { api, .. } => {
            if !EXIT_FLAG.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
        // 点 X：拦截关闭 -> destroy（不是 hide！），WebView2 进程彻底释放。
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } => {
            api.prevent_close();
            if label == WINDOW_LABEL {
                if let Some(w) = app.get_webview_window(&label) {
                    // v2.3.1：销毁前先存几何，重建时恢复位置/尺寸。
                    save_geometry(&w);
                    let _ = w.destroy();
                }
                trim_working_set_soon();
            }
        }
        _ => {}
    });
}
