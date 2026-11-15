//! Windows 系统托盘：百行左右的原生 Win32 消息循环（windows-sys）。
//! - 启动时在任务栏角落挂一个图标（assets/icon.ico，找不到就用默认图标）；
//! - 右键菜单：打开面板 / 暂停（恢复）云端写入 / 退出 Pylon；
//! - 左键双击 = 打开面板；
//! - 退出时删图标；运行中的命令子进程都进了 Job Object，
//!   Pylon 退出时句柄一关、系统把它们一锅端。
//! v2.2.0：托盘跑在独立线程（Tauri 占主线程），“打开面板”改为显示/聚焦
//! Tauri 窗口（窗口被 destroy 后动态重建）；“退出”走 TrayBridge 的
//! request_quit，置位退出标志后 app.exit(0)。主线程（窗口关了）调
//! shutdown_tray() 让托盘线程收尾。新审批入队时除气泡外还重建/聚焦窗口。
//! v2.2.1：托盘线程只“请求”窗口（TrayBridge::request_show），内部把
//! 显示 / 重建投递到 Tauri 主线程（run_on_main_thread）执行——Win32 窗口
//! 只在主线程创建，托盘消息循环里不做任何建窗操作。
//! （本文件只在 Windows 下编译，见 main.rs 的 `#[cfg(windows)] mod tray;`）

use crate::bridge::{push_log, ServerState};
use crate::tauri_ui::TrayBridge;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HICON, HMENU, HWND_MESSAGE,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, MF_SEPARATOR, MF_STRING,
    MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_QUIT, WM_RBUTTONUP, WM_TIMER, WM_USER, WNDCLASSEXW,
};

const WM_TRAY: u32 = WM_USER + 1;
const MENU_OPEN: u32 = 1001;
const MENU_TOGGLE: u32 = 1002;
const MENU_QUIT: u32 = 1003;

static TRAY_STATE: OnceLock<Arc<ServerState>> = OnceLock::new();
/// v2.2.0：托盘线程 ↔ Tauri 主线程的信道（main.rs 里填入）。
static TRAY_BRIDGE: OnceLock<Arc<TrayBridge>> = OnceLock::new();
// HWND 是 *mut c_void，不能进 Mutex 静态（不 Send/Sync），
// 存成 usize 走 AtomicUsize，用时再转回 HWND。
static TRAY_HWND: AtomicUsize = AtomicUsize::new(0);
/// v1.0（模块三）：上次弹过气泡的审批序号；托盘定时器靠它发现新审批。
static LAST_APPROVAL_SEQ: AtomicU64 = AtomicU64::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn set_wide<const N: usize>(buf: &mut [u16; N], text: &str) {
    let w: Vec<u16> = text.encode_utf16().take(N - 1).collect();
    buf[..w.len()].copy_from_slice(&w);
    buf[w.len()] = 0;
}

fn set_tip(buf: &mut [u16; 128], text: &str) {
    set_wide(buf, text);
}

fn load_tray_icon(hinstance: HINSTANCE) -> HICON {
    // v2.3.0：Cyber-Cat 新图标。优先 exe 旁 assets/icon.ico，
    // 再 exe 旁 icons/icon.ico（tauri-build 同款资源），最后当前目录 assets/。
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("assets").join("icon.ico"));
            candidates.push(dir.join("icons").join("icon.ico"));
        }
    }
    candidates.push(PathBuf::from("assets/icon.ico"));
    candidates.push(PathBuf::from("icons/icon.ico"));
    for p in candidates {
        if !p.exists() {
            continue;
        }
        let w = wide(&p.to_string_lossy());
        let h = unsafe {
            LoadImageW(
                std::ptr::null_mut(),
                w.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        };
        if !h.is_null() {
            return h;
        }
    }
    unsafe { LoadIconW(hinstance, IDI_APPLICATION) }
}

fn update_tip(paused: bool) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst) as HWND;
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_TIP;
        set_tip(
            &mut nid.szTip,
            if paused {
                "Pylon - 已熔断（云端写入已暂停）"
            } else {
                "Pylon - 运行中"
            },
        );
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// v1.0（模块三）：在 Windows 右下角弹原生气泡通知。
/// 点气泡会唤醒控制面板（wnd_proc 里 NIN_BALLOONUSERCLICK 分支）。
fn show_balloon(title: &str, info: &str) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst) as HWND;
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        set_wide(&mut nid.szInfo, info);
        set_wide(&mut nid.szInfoTitle, title);
        nid.dwInfoFlags = NIIF_INFO;
        // uTimeout 藏在 Anonymous 联合体里（Vista 之后实际由系统决定显示时长）。
        nid.Anonymous.uTimeout = 10000;
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// v2.2.0：托盘“退出 Pylon”——置位退出标志 + app.exit(0)，
/// ExitRequested 放行后 run_tauri 返回，main.rs 收尾。
/// Tauri 还没起来时 request_quit 内部直接退出进程。
fn quit_app() {
    if let Some(b) = TRAY_BRIDGE.get() {
        b.request_quit();
    } else {
        std::process::exit(0);
    }
}

/// v2.2.0：主线程（Tauri 窗口关了）让托盘线程收尾——往托盘窗口发 WM_QUIT，
/// 消息循环结束、删图标、线程返回，main 再 join 它。
pub fn shutdown_tray() {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe {
            PostMessageW(hwnd, WM_QUIT, 0, 0);
        }
    }
}

fn toggle_kill_switch() {
    let Some(state) = TRAY_STATE.get() else {
        return;
    };
    let paused = !state.kill_switch.load(Ordering::SeqCst);
    state.kill_switch.store(paused, Ordering::SeqCst);
    push_log(
        &state.logs,
        if paused {
            "熔断已开启：云端写入已暂停".to_string()
        } else {
            "熔断已解除：云端写入恢复".to_string()
        },
    );
    update_tip(paused);
}

unsafe fn append_menu_text(menu: HMENU, id: usize, text: &str) {
    let w = wide(text);
    AppendMenuW(menu, MF_STRING, id, w.as_ptr());
}

fn show_menu(hwnd: HWND) {
    let Some(state) = TRAY_STATE.get() else {
        return;
    };
    let paused = state.kill_switch.load(Ordering::SeqCst);
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        append_menu_text(menu, MENU_OPEN as usize, "打开面板");
        append_menu_text(
            menu,
            MENU_TOGGLE as usize,
            if paused {
                "恢复云端写入"
            } else {
                "暂停云端写入（紧急熔断）"
            },
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        append_menu_text(menu, MENU_QUIT as usize, "退出 Pylon");
        // TrackPopupMenu 之前必须先 SetForegroundWindow，否则菜单点不掉。
        SetForegroundWindow(hwnd);
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        match cmd as u32 {
            MENU_OPEN => {
                if let Some(b) = TRAY_BRIDGE.get() {
                    b.request_show();
                }
            }
            MENU_TOGGLE => toggle_kill_switch(),
            MENU_QUIT => quit_app(),
            _ => {}
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            // lparam 是托盘通知码：气泡点击是 NIN_BALLOONUSERCLICK，
            // 鼠标消息在低位（右键菜单 / 左键双击）。
            let code = lparam as u32;
            if code == NIN_BALLOONUSERCLICK {
                if let Some(b) = TRAY_BRIDGE.get() {
                    b.request_show();
                }
            } else {
                let mouse = code & 0xFFFF;
                if mouse == WM_RBUTTONUP {
                    show_menu(hwnd);
                } else if mouse == WM_LBUTTONDBLCLK {
                    if let Some(b) = TRAY_BRIDGE.get() {
                        b.request_show();
                    }
                }
            }
            0
        }
        WM_TIMER => {
            // v1.0（模块三）：每 1 秒看一眼审批序号，涨了就弹气泡。
            if let Some(s) = TRAY_STATE.get() {
                let seq = s.approval_seq.load(Ordering::SeqCst);
                let last = LAST_APPROVAL_SEQ.load(Ordering::SeqCst);
                if seq > last {
                    LAST_APPROVAL_SEQ.store(seq, Ordering::SeqCst);
                    let detail = {
                        // v2.3.3：Win32 窗口过程里不再 `unwrap()`——Mutex 中毒时
                        // 在 FFI 回调里 panic 会跨 FFI 边界 unwind（未定义行为）。
                        match s.approvals.lock() {
                            Ok(q) => q.back().map(|r| r.detail.clone()),
                            Err(poisoned) => {
                                poisoned.into_inner().back().map(|r| r.detail.clone())
                            }
                        }
                    };
                    if let Some(mut d) = detail {
                        if d.chars().count() > 200 {
                            d = d.chars().take(200).collect();
                        }
                        show_balloon("Pylon 待批准操作", &d);
                    }
                    // v2.2.0：新审批入队 -> 窗口在则聚焦，不在则重建，
                    // 用户不用手动找托盘就能直接点允许/拒绝。
                    if let Some(b) = TRAY_BRIDGE.get() {
                        b.request_show();
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam as u32) & 0xFFFF;
            match id {
                MENU_OPEN => {
                    if let Some(b) = TRAY_BRIDGE.get() {
                        b.request_show();
                    }
                }
                MENU_TOGGLE => toggle_kill_switch(),
                MENU_QUIT => quit_app(),
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 独立线程跑托盘消息循环（阻塞，直到主线程调 shutdown_tray() 发 WM_QUIT，
/// 或用户点“退出 Pylon”走 quit_app()）。
/// v2.0 第 3 步：托盘不再占主线程（主线程归 egui），改由 main.rs spawn 一个线程跑它。
pub fn run_tray(state: Arc<ServerState>, bridge: Arc<TrayBridge>) {
    let _ = TRAY_STATE.set(state);
    let _ = TRAY_BRIDGE.set(bridge);
    // v2.3.1：第二实例唤醒监听——独立线程等命名 Event `Local\PylonShowWindowEvent`。
    // 双击 exe 发现单实例锁已存在时，第二实例先 SetEvent 再退出（见 main.rs）；
    // 这里收到信号就重建 / 置顶窗口。窗口已销毁的托盘待机状态下，
    // 否则双击桌面图标毫无反应。auto-reset 事件：每次 SetEvent 只唤醒一次。
    std::thread::spawn(|| {
        use windows_sys::Win32::Foundation::{FALSE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        use windows_sys::w;
        let event = unsafe {
            CreateEventW(
                std::ptr::null(),
                FALSE,
                FALSE,
                w!("Local\\PylonShowWindowEvent"),
            )
        };
        if event.is_null() {
            return;
        }
        loop {
            // u32::MAX = INFINITE：一直等，直到进程退出。
            let r = unsafe { WaitForSingleObject(event, u32::MAX) };
            if r == WAIT_OBJECT_0 {
                if let Some(b) = TRAY_BRIDGE.get() {
                    b.request_show();
                }
            } else {
                break; // WAIT_FAILED / WAIT_ABANDONED：不再监听
            }
        }
    });
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("PylonTrayWindow");
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            eprintln!("[pylon] 注册托盘窗口类失败，托盘不启用（服务继续跑）");
            return;
        }
        // 消息专用窗口：不显示、不进任务栏。
        let title = wide("Pylon");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            eprintln!("[pylon] 创建托盘窗口失败，托盘不启用（服务继续跑）");
            return;
        }
        TRAY_HWND.store(hwnd as usize, Ordering::SeqCst);
        let icon = load_tray_icon(hinstance);
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        set_tip(&mut nid.szTip, "Pylon - 运行中");
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            eprintln!("[pylon] 托盘图标挂载失败（服务继续跑）");
        }
        // v1.0（模块三）：每 1 秒看一眼有没有新审批，有就弹托盘气泡。
        SetTimer(hwnd, 1, 1000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_icon(hwnd);
    }
}
