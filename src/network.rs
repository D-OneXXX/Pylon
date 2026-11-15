//! NetworkManager（v0.9 起；v2.1.3 简化为只剩 HTTP 监听）。
//!
//! v2.1.3 之前这里管四种网络模式（HTTP 监听 / cloudflared 隧道 /
//! WS 反向客户端 / relay_poll），各跑一个独立线程热切换。
//! 用户 2026-11-13 拍板只保留 Tailscale 模式：监听固定绑 host:port
//! （默认 0.0.0.0:8765），其余三种模式连同 ws.rs / relay.rs / tls.rs
//! 与两个中继脚本（cloud_ws_relay.py / cloud_relay_poll.py）一并删除。
//! 剩下的唯一热切换项是监听地址（端口改了不用重启，0.5 秒内生效）。
//! 旧线程看到 generation 变了就自己退出（监听用非阻塞 accept + 50ms 轮询），
//! 新线程随后拉起。

use crate::bridge::{handle_client, push_log, ServerState};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub struct NetworkManager {
    state: Arc<ServerState>,
    listener_gen: Arc<AtomicU64>,
    listener_alive: Arc<AtomicBool>,
    applied_bind_addr: Mutex<String>,
}

impl NetworkManager {
    pub fn new(state: Arc<ServerState>) -> Self {
        let applied_bind_addr = state.config().bind_addr();
        Self {
            state,
            listener_gen: Arc::new(AtomicU64::new(1)),
            listener_alive: Arc::new(AtomicBool::new(false)),
            applied_bind_addr: Mutex::new(applied_bind_addr),
        }
    }

    /// 首次启动：拉起监听。监听绑不上（如端口被占）时返回 Err，main 据此退出。
    pub fn start(&self) -> std::io::Result<()> {
        let cfg = self.state.config();
        self.spawn_listener(&cfg.bind_addr())
    }

    /// watch 线程入口：收到配置变更信号就 apply()。
    pub fn watch(&self, rx: mpsc::Receiver<()>) {
        while rx.recv().is_ok() {
            self.apply();
        }
    }

    /// 热切换：监听地址变了才重启监听（只改 token / 权限 / 白名单时不动监听）。
    ///
    /// v2.3.3（审查报告 P1 #3）：绑定失败必须**回滚**。
    /// 旧实现把 `*old = new_addr` 放在 `match` 外面无条件提交：`spawn_listener`
    /// 先让旧监听退出、再绑新地址，一旦新地址不可用（端口被占 / 地址写错），
    /// 旧监听已经停了，而 `applied_bind_addr` 却被改成新值——之后再保存同样的
    /// 配置时 `new_addr == old`，连重试都不会发生，服务永久失联。
    /// 现在只有在绑定成功后才提交；失败时立刻用旧地址重新拉起，
    /// 并把实际情况写进动态日志。
    pub fn apply(&self) {
        let new_addr = self.state.config().bind_addr();
        let mut old = match self.applied_bind_addr.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if new_addr == *old {
            return;
        }
        match self.spawn_listener(&new_addr) {
            Ok(()) => {
                *old = new_addr.clone();
                push_log(&self.state.logs, format!("监听已热切换到 {new_addr}"));
            }
            Err(e) => {
                let previous = old.clone();
                push_log(
                    &self.state.logs,
                    format!("监听热切换失败（{new_addr}）：{e}，正在回滚到 {previous}"),
                );
                match self.spawn_listener(&previous) {
                    Ok(()) => push_log(
                        &self.state.logs,
                        format!("已回滚：继续监听 {previous}（配置里的地址暂未生效）"),
                    ),
                    Err(e2) => push_log(
                        &self.state.logs,
                        format!("回滚失败（{previous}）：{e2}；当前没有可用监听，请检查配置"),
                    ),
                }
                // 不提交 applied_bind_addr：保持旧值，用户改回正确地址后
                // 下一次 apply 仍会尝试切换。
            }
        }
    }

    fn spawn_listener(&self, addr: &str) -> std::io::Result<()> {
        // 先让旧监听退出（非阻塞 accept 每 50ms 看一次 generation），再绑新端口。
        let my_gen = self.listener_gen.fetch_add(1, Ordering::SeqCst) + 1;
        for _ in 0..100 {
            if !self.listener_alive.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..5 {
            match TcpListener::bind(addr) {
                Ok(listener) => {
                    let state = Arc::clone(&self.state);
                    let gen = Arc::clone(&self.listener_gen);
                    let alive = Arc::clone(&self.listener_alive);
                    let addr = addr.to_string();
                    std::thread::spawn(move || {
                        listener_loop(state, listener, addr, gen, my_gen, alive)
                    });
                    // 等新循环确认活着（最多 1 秒）。
                    for _ in 0..100 {
                        if self.listener_alive.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "绑定失败")
        }))
    }
}

// ---------------------------------------------------------------------------
// HTTP 监听循环（非阻塞 accept，每 50ms 看一次 generation）
// ---------------------------------------------------------------------------

fn listener_loop(
    state: Arc<ServerState>,
    listener: TcpListener,
    addr: String,
    gen: Arc<AtomicU64>,
    my_gen: u64,
    alive: Arc<AtomicBool>,
) {
    alive.store(true, Ordering::SeqCst);
    if listener.set_nonblocking(true).is_err() {
        alive.store(false, Ordering::SeqCst);
        push_log(&state.logs, "监听启动失败：非阻塞设置失败".to_string());
        return;
    }
    push_log(
        &state.logs,
        format!("桥服务已启动：http://{addr}（Tailscale 模式）"),
    );
    loop {
        if gen.load(Ordering::SeqCst) != my_gen {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows 上 accept 会继承监听 socket 的非阻塞状态，
                // 必须显式改回阻塞，否则 handle_client 读到 WouldBlock 就直接断连。
                // （v0.9.1 回归修复：v0.9.0 把监听改成非阻塞轮询后忘了这行，
                // 大 body 分段到达时几乎必挂，小请求单包到达所以偶尔能过。）
                let _ = stream.set_nonblocking(false);
                let s = Arc::clone(&state);
                std::thread::spawn(move || handle_client(stream, s));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                push_log(&state.logs, format!("accept 失败：{e}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::ServerState;
    use crate::bridge::SharedLogs;
    use std::sync::Mutex;
    use std::time::Instant;

    fn test_state_with_port(dir: &std::path::Path, port: u16) -> Arc<ServerState> {
        let mut cfg = crate::config::BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.host = "127.0.0.1".to_string();
        cfg.port = port;
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        Arc::new(ServerState::new(
            cfg,
            dir.join("bridge-config.json"),
            logs,
        ))
    }

    fn http_get_health(port: u16) -> Option<u16> {
        http_get_health_token(port, "t")
    }

    fn http_get_health_token(port: u16, token: &str) -> Option<u16> {
        // v2.2.0：带重试的健康检查。全套测试并行跑时，git/ping 子进程
        // 创建带来的调度抖动偶尔让单次 connect/read 落空（真机实测：
        // 串行 100% 绿、并行偶发 None）；重试 5 次吸收抖动，
        // 确定性回归（如监听没起来）每次都会失败，不会被掩盖。
        for attempt in 0..5 {
            if let Some(code) = http_get_health_once(port, token) {
                return Some(code);
            }
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        None
    }

    fn http_get_health_once(port: u16, token: &str) -> Option<u16> {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(3))).ok();
        s.write_all(
            format!("GET /health HTTP/1.1\r\nHost: x\r\nX-Bridge-Token: {token}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).ok()?;
        let head = String::from_utf8_lossy(&buf);
        head.split_whitespace().nth(1).and_then(|c| c.parse().ok())
    }

    /// v2.2.0：取一个空闲临时端口（bind 0 让 OS 分配，立即释放）。
    /// 不再写死 18791-18795：WSL2 与 Windows 共享 localhost，
    /// 另一端测试残留的 TIME_WAIT 会让固定端口 bind 撞 AddrInUse；
    /// 临时端口也避免并行 / 跨环境复跑时的端口冲突。
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// v2.3.3：发一段原始 HTTP 文本，读回完整响应（含状态行与头）。
    fn raw_request(port: u16, text: &str) -> String {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("连不上");
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        s.write_all(text.as_bytes()).ok();
        let _ = s.flush();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn http_ads_path_is_not_served() {
        // P0 端到端回归：GET /api/read 用 `bridge-config.json::$DATA` 读配置，
        // 必须被挡在判定层（403/400），响应里绝不能出现 token 内容。
        let port = free_port();
        let dir = std::env::temp_dir().join("pylon-net-ads");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("bridge-config.json"), r#"{"token":"SECRET_TOKEN"}"#).unwrap();
        let state = test_state_with_port(&dir, port);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        let resp = raw_request(
            port,
            "GET /api/read?root=work&path=bridge-config.json%3A%3A%24DATA HTTP/1.1\r\nHost: x\r\nX-Bridge-Token: t\r\nConnection: close\r\n\r\n",
        );
        assert!(
            resp.contains(" 403 ") || resp.contains(" 400 "),
            "应拒绝 ADS 读取，实际：{resp}"
        );
        assert!(!resp.contains("SECRET_TOKEN"), "响应里泄露了 token：{resp}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_chunked_is_rejected_explicitly() {
        // P1 #1：不解析 chunked 就明确拒绝，不能把 chunk 帧当作 body。
        let port = free_port();
        let dir = std::env::temp_dir().join("pylon-net-chunked");
        let _ = std::fs::create_dir_all(&dir);
        let state = test_state_with_port(&dir, port);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        let resp = raw_request(
            port,
            "POST /api/write HTTP/1.1\r\nHost: x\r\nX-Bridge-Token: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        );
        assert!(resp.contains(" 411 "), "应回 411，实际：{resp}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_overlong_request_line_is_rejected() {
        // P1 #1：不含 \r\n 的超长请求行不能无限吃内存。
        let port = free_port();
        let dir = std::env::temp_dir().join("pylon-net-longline");
        let _ = std::fs::create_dir_all(&dir);
        let state = test_state_with_port(&dir, port);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        let long = "GET /".to_string() + &"a".repeat(9000) + "\r\n\r\n";
        let resp = raw_request(port, &long);
        assert!(resp.contains(" 414 "), "应回 414，实际：{resp}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_switch_listener_port() {
        // 热切换：port_a 跑起来 -> apply 改 port_b ->
        // 新端口 200、旧端口拒绝连接，且不用重启进程。
        let port_a = free_port();
        let port_b = free_port();
        let dir = std::env::temp_dir().join("pylon-net-test");
        let _ = std::fs::create_dir_all(&dir);
        let state = test_state_with_port(&dir, port_a);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        assert_eq!(http_get_health(port_a), Some(200));
        let mut cfg = state.config();
        cfg.port = port_b;
        state.update_config(cfg);
        mgr.apply();
        assert_eq!(http_get_health(port_b), Some(200));
        // v2.2.0：旧监听线程看到 generation 变化后最多 50ms 才退出，
        // 轮询等旧端口真正拒绝连接（2 秒上限），吸收退出窗口抖动。
        wait_port_dead(port_a);
        // 再切回来也行。
        let mut cfg = state.config();
        cfg.port = port_a;
        state.update_config(cfg);
        mgr.apply();
        assert_eq!(http_get_health(port_a), Some(200));
        wait_port_dead(port_b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮询直到端口拒绝连接（旧监听线程退出窗口最多 50ms + 调度抖动）。
    fn wait_port_dead(port: u16) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if http_get_health_once(port, "t").is_none() {
                return;
            }
            assert!(Instant::now() < deadline, "端口 {port} 两秒后仍在响应");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn apply_without_network_change_keeps_listener() {
        // 只改 token（不改网络项）：监听不重启，旧端口继续 200。
        let port = free_port();
        let dir = std::env::temp_dir().join("pylon-net-test2");
        let _ = std::fs::create_dir_all(&dir);
        let state = test_state_with_port(&dir, port);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        assert_eq!(http_get_health(port), Some(200));
        let mut cfg = state.config();
        cfg.token = "newtoken".to_string();
        state.update_config(cfg);
        mgr.apply();
        // 新 token 立刻生效（旧 token 会 401），监听本身没重启、端口照通。
        assert_eq!(http_get_health_token(port, "newtoken"), Some(200));
        assert_eq!(http_get_health_token(port, "t"), Some(401));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slow_headers_still_get_response() {
        // 回归测试（v0.9.1）：请求行与请求头分开发、中间隔 300ms，
        // 服务端仍要回 200，不能直接断连。
        // 背景：listener_loop 把监听 socket 设成非阻塞做 50ms 轮询；
        // Windows 上 accept 出来的 socket 会继承这个非阻塞状态，
        // 若不在 accept 后改回阻塞，handle_client 的 read_line /
        // read_exact 一撞 WouldBlock 就直接 return、连响应都不发。
        // 小请求单包到达所以偶尔能过，分段到达的大 body 几乎必挂。
        // 注意：Linux 上 accept 出来的 socket 本来就是阻塞的，
        // 这个测试在 Linux 上即使没修也能过；真正的裁决在 Windows 真机上。
        // v2.2.0：并行全套测试时子进程创建的调度抖动偶尔让连接被重置
        //（真机实测串行 100% 绿）；最多试 3 次吸收抖动——v0.9.1 的
        // 确定性回归每次都会失败，不会被重试掩盖。
        use std::io::{Read, Write};
        let port = free_port();
        let dir = std::env::temp_dir().join("pylon-net-slowhdr");
        let _ = std::fs::create_dir_all(&dir);
        let state = test_state_with_port(&dir, port);
        let mgr = NetworkManager::new(Arc::clone(&state));
        mgr.start().unwrap();
        let mut last = String::new();
        for attempt in 0..3 {
            let result = (|| -> std::io::Result<String> {
                let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
                s.set_read_timeout(Some(Duration::from_secs(5)))?;
                s.write_all(b"GET /health HTTP/1.1\r\n")?;
                std::thread::sleep(Duration::from_millis(300));
                s.write_all(b"Host: x\r\nX-Bridge-Token: t\r\nConnection: close\r\n\r\n")?;
                let mut buf = Vec::new();
                s.read_to_end(&mut buf)?;
                Ok(String::from_utf8_lossy(&buf).into_owned())
            })();
            match result {
                Ok(head) => {
                    assert!(
                        head.contains("200"),
                        "分段到达的请求头没拿到 200，响应为空或被断连：{head}"
                    );
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
                Err(e) => {
                    last = format!("{e}");
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }
            }
        }
        panic!("分段请求头 3 次都没拿到响应（最后一次错误：{last}）");
    }
}
