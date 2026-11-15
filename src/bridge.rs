//! Pylon 桥服务本体：只用 std 做 HTTP，不引 axum / tokio。
//!
//! 接口（JSON 风格一致）：
//!   GET  /health
//!   GET  /api/list?root=<别名>&path=<相对路径>
//!   GET  /api/read?root=<别名>&path=<相对文件>
//!   POST /api/write   {"root","path","content"}
//!   POST /api/mkdir   {"root","path"}
//!   POST /api/run     {"root","path","argv","timeout_secs"}  // v0.5 白名单命令
//!   POST /api/patch   {"root","path","patches":[{"search","replace"}]}  // v0.8
//!   GET  /api/search?root=&path=&query=&is_regex=&case_sensitive=  // v0.8
//!   POST /api/trash   {"root","path"}  // v1.0：软删除进 .pylon_trash/
//!   POST /api/rename  {"root","from","to"}  // v1.0：重命名与移动
//!   GET  /api/read_base64?root=&path=  // v1.0：二进制读（上限 10MB）
//!   POST /api/write_base64 {"root","path","content_base64"}  // v1.0：二进制写
//!   GET  /            // v0.8：内置 Web 面板（仅 127.0.0.1）
//!   GET  /ui/state  /ui/approvals  /ui/activity  /ui/browse  // v0.8：面板接口（仅本机 + 密钥）
//!   POST /ui/approve  /ui/run  /ui/save-config  /ui/kill-switch
//!   GET  /ui/snapshots  POST /ui/revert {"id"}  // v1.0：快照列表与一键回滚
//!
//! v0.8 架构：`dispatch_request` 是统一分发核心——HTTP Listener 调它，
//! v2.1.3：只保留 Tailscale 模式，WS 反向 / iroh P2P 已删除。
//! GPUI 已砍掉（v0.8）；网页面板已删（v2.0）；eframe/egui 已砍掉（v2.1），
//! 界面是 Slint 声明式窗口（Win11 Fluent）+ 系统托盘。

use crate::config::{BridgeConfig, PermissionMode, MIN_TOKEN_LEN};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// v1.0（模块四）：回收站目录名（root 下）；二进制传输上限 10MB。
pub const TRASH_DIR: &str = ".pylon_trash";
pub const MAX_BASE64_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_LIST_ENTRIES: usize = 500;
/// /api/run 单条流的输出上限（stdout / stderr 各算），超了截断并标记。
pub const MAX_RUN_OUTPUT_BYTES: usize = 512 * 1024;
/// /api/run 默认超时 300 秒；请求里 timeout_secs 可改，上限 1800 秒。
pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;
pub const MAX_RUN_TIMEOUT_SECS: u64 = 1800;
/// v0.8：人工审批默认等 60 秒，超时按拒绝处理。
pub const APPROVAL_TIMEOUT_SECS: u64 = 60;
/// v2.3.3（审查报告 P1 #2）：挂起审批的队列上限。
/// 旧实现队列无界、每挂起一项独占一条 HTTP 线程 60 秒——持 token 者并发
/// 发起命中 `run_asklist` 的命令就能把线程与内存一起耗光（放大 P1 #1）。
/// 超过上限直接 429，不再往队列里塞。
pub const MAX_PENDING_APPROVALS: usize = 16;
/// v0.8：HMAC 防重放允许的时间差（±60 秒）。
pub const HMAC_MAX_SKEW_SECS: i64 = 60;
/// v2.3.3：签名去重表上限（60 秒窗口内正常调用远达不到）。
const MAX_SEEN_SIGNATURES: usize = 4096;
/// v2.3.3：认证失败限流——窗口内失败 N 次后锁定一段时间。
pub const AUTH_MAX_FAILURES: u32 = 10;
pub const AUTH_FAILURE_WINDOW_SECS: u64 = 60;
pub const AUTH_LOCKOUT_SECS: u64 = 60;
/// v0.8：/api/search 最多返回条数。
pub const MAX_SEARCH_RESULTS: usize = 100;
/// v2.3.3（审查报告 P1 #1）：HTTP 层的硬上限。
/// 旧实现没有读超时、请求行 / 请求头没有长度上限、每连接一个线程也没有并发上限
/// ——一个声明了 `Content-Length` 却不发 body 的连接、或一条不含 `\r\n` 的超长
/// 请求行，就能永久占住一条线程 / 无限吃内存。
pub const HTTP_READ_TIMEOUT_SECS: u64 = 20;
pub const HTTP_WRITE_TIMEOUT_SECS: u64 = 30;
/// 单条请求行（含方法、目标、版本）上限。
pub const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
/// 单个请求头上限与全部请求头合计上限。
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
pub const MAX_HEADER_TOTAL_BYTES: usize = 32 * 1024;
pub const MAX_HEADER_COUNT: usize = 100;
/// 同时处理的连接数上限（每连接一条线程）。
pub const MAX_CONCURRENT_CONNECTIONS: usize = 64;
/// 读完整份请求（请求行 + 头 + body）的总时限，防慢速滴灌。
/// 取 60 秒：2MB body 在 ~35KB/s 的链路上也传得完，
/// 又足以把「每 19 秒滴一个字节」这类慢速攻击挡在 64 条连接的上限之内。
pub const HTTP_REQUEST_DEADLINE_SECS: u64 = 60;

pub const VERSION: &str = "2.3.3";

/// v2.1.3：base64 编解码（/api/read_base64 / /api/write_base64 用）。
/// 之前在已删除的 ws.rs 里，改走 base64 crate。
pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败：{e}"))
}


/// v2.0：网页面板已删（egui 窗口取代），GET / 改回小 JSON 状态。
/// /ui/* 仍保留（仅本机 + 固定请求头 `X-Pylon-UI: 1`），供本地工具与测试用。

/// v1.0：动态记录的一条。`snapshot_id` 有值时，面板「动态」卡
/// 在这条右侧显示 [↶ 回滚] 按钮，点一下就把文件还原到快照时的样子。
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// v1.0.3：日期（YYYY-MM-DD），热力图按天统计靠它；旧行没有日期时为空。
    pub date: String,
    pub time: String,
    pub msg: String,
    pub snapshot_id: Option<String>,
}

pub type SharedLogs = Arc<Mutex<Vec<LogEntry>>>;

/// v2.2.0：日志单行化——\r\n / \r / \n 统一替换为可视符号 " ↵ "，
/// 连续空白折叠为单空格，首尾修剪。保证每条日志严格单行：
/// 前端列表行不再纵向溢出，pylon-activity.log 按行解析不再被多行片段劈裂。
pub fn sanitize_log_msg(msg: &str) -> String {
    let s = msg
        .replace("\r\n", " ↵ ")
        .replace('\r', " ↵ ")
        .replace('\n', " ↵ ");
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// v2.2.0：行首是否为完整新格式时间戳 "YYYY-MM-DD HH:MM:SS"。
/// restore_log_file 与 count_heatmap_days 共用：历史日志里残留的多行
/// 片段（不以时间戳开头）一律跳过，不再污染恢复列表与热力图统计。
fn log_timestamp_prefix(line: &str) -> bool {
    let b = line.as_bytes();
    line.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[17..19].iter().all(u8::is_ascii_digit)
}

/// 所有动态记录都走这里：前面带本地日期+时间（YYYY-MM-DD HH:MM:SS），
/// 界面「动态」卡靠它定位“什么时候改了什么文件 / 跑了什么命令”，
/// 热力图（/ui/heatmap）按日期统计也靠它。
pub fn push_log(logs: &SharedLogs, msg: String) {
    push_log_snapshot(logs, msg, None);
}

/// 带快照 id 的版本（写 / 修补 / 回收站等动作在落盘成功后调它，
/// 动态里这条就能一键回滚）。
/// v2.2.0：入口统一单行化——多行命令（python -c "...\n..."）的原始换行
/// 在这里被替换成可视符号，LogEntry.msg 与落盘行严格单行，
/// 前端列表不再纵向溢出覆盖下方条目，按行解析的日志不再劈裂。
pub fn push_log_snapshot(logs: &SharedLogs, msg: String, snapshot_id: Option<String>) {
    let now = chrono::Local::now();
    let line = LogEntry {
        date: now.format("%Y-%m-%d").to_string(),
        time: now.format("%H:%M:%S").to_string(),
        msg: sanitize_log_msg(&msg),
        snapshot_id,
    };
    if let Ok(mut guard) = logs.lock() {
        guard.push(line.clone());
        if guard.len() > 200 {
            let excess = guard.len() - 200;
            guard.drain(..excess);
        }
    }
    append_log_file(&line);
}

/// v1.0（模块五）：动态日志落盘。
/// - 位置：exe 同目录的 pylon-activity.log；
/// - 超 5MB 自动轮转为 pylon-activity.log.old；
/// - 启动时读末尾 50 条恢复进内存列表（见 init_log_file）。
pub const LOG_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;
static LOG_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 设置落盘路径（main.rs 启动时调一次；单测可临时改向）。
pub fn set_log_file(path: Option<PathBuf>) {
    if let Ok(mut g) = LOG_FILE.lock() {
        *g = path;
    }
}

/// 启动时：定下落盘路径，并把日志文件末尾 50 行恢复进内存动态列表。
/// 返回恢复的条数（main.rs 写进启动日志，方便确认回读生效）。
/// v2.1：与热力图保持一致——主文件 + `.old` 轮转文件都读（旧文件在前、
/// 主文件在后，合起来取末尾 50 条），轮转过的历史也不丢。
pub fn init_log_file(logs: &SharedLogs, path: PathBuf) -> usize {
    set_log_file(Some(path.clone()));
    restore_log_file(logs, &path)
}

/// 把日志文件末尾 50 行恢复进内存动态列表（init_log_file 的纯逻辑部分，
/// 单测直接调它，不碰全局落盘路径）。
/// 行格式就是 "YYYY-MM-DD HH:MM:SS 消息"（v1.0.3 起带日期）；旧格式
/// "HH:MM:SS 消息"也认，日期按今天算——重启跨版本不丢行。
/// v2.1：主文件 + `.old` 都读（`.old` 在前、主文件在后，合起来取末尾 50 条）。
/// 根因复核（2026-11-13）：restore 本身没错——拿真实 184 行日志实测，
/// 末尾 50 条全能恢复；但旧版只读主文件，轮转后历史只剩 `.old` 里有，
/// 热力图（读主 + `.old`）能数出来、动态列表却看不到，对不上。
/// 这次改成两边都读，和热力图同一口径。
pub fn restore_log_file(logs: &SharedLogs, path: &Path) -> usize {
    let old_path = PathBuf::from(format!("{}.old", path.display()));
    let mut all_lines: Vec<String> = Vec::new();
    for p in [&old_path, path] {
        if let Ok(content) = std::fs::read_to_string(p) {
            all_lines.extend(content.lines().map(|l| l.to_string()));
        }
    }
    let tail: &[String] = if all_lines.len() > 50 {
        &all_lines[all_lines.len() - 50..]
    } else {
        &all_lines[..]
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut restored = 0usize;
    if let Ok(mut guard) = logs.lock() {
        for line in tail {
            // v2.2.0：只认带完整时间戳前缀的行；历史多行命令溢出的脏片段
            //（不以 YYYY-MM-DD HH:MM:SS 或旧版 HH:MM:SS 开头）直接跳过，
            // 不再当消息恢复进动态列表。
            let (date, time, msg) = if log_timestamp_prefix(line)
                && line.len() >= 20
                && line.as_bytes()[19] == b' '
            {
                // 新格式：YYYY-MM-DD HH:MM:SS 消息
                (
                    line[..10].to_string(),
                    line[11..19].to_string(),
                    line[20..].to_string(),
                )
            } else if line.len() > 9
                && line.as_bytes().get(2) == Some(&b':')
                && line.as_bytes().get(5) == Some(&b':')
                && line.as_bytes().get(8) == Some(&b' ')
            {
                // 旧格式（v1.0.2 及以前）：HH:MM:SS 消息，日期按今天算。
                (today.clone(), line[..8].to_string(), line[9..].to_string())
            } else {
                continue;
            };
            guard.push(LogEntry {
                date,
                time,
                msg,
                snapshot_id: None,
            });
            restored += 1;
        }
        if guard.len() > 200 {
            let excess = guard.len() - 200;
            guard.drain(..excess);
        }
    }
    restored
}

/// push_log 的落盘部分：有路径就追加一行；超 5MB 先轮转。
fn append_log_file(entry: &LogEntry) {
    let path = match LOG_FILE.lock() {
        Ok(g) => match g.clone() {
            Some(p) => p,
            None => return,
        },
        Err(_) => return,
    };
    append_log_file_to(&path, entry);
}

/// 给指定路径追加一行日志；超 5MB 先把旧文件轮转为 `.old`
///（append_log_file 的纯逻辑部分，单测直接调它，不碰全局落盘路径）。
/// v1.0.1：追加全程持一把静态写锁——writeln! 底层是多次 write，
/// 多线程并发追加会把两行字串在一起（真机实测：行首行尾互相咬合）。
/// 锁住整个 open + 写回，行与行之间不再交错。
static LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn append_log_file_to(path: &Path, entry: &LogEntry) {
    let _guard = match LOG_WRITE_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > LOG_FILE_MAX_BYTES {
            let old = PathBuf::from(format!("{}.old", path.display()));
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(path, &old);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // v1.0.3：行首带日期，供热力图按天统计；date 为空（旧行/单测手写）
        // 时退回旧格式，不写空日期占位。
        if entry.date.is_empty() {
            let _ = writeln!(f, "{} {}", entry.time, entry.msg);
        } else {
            let _ = writeln!(f, "{} {} {}", entry.date, entry.time, entry.msg);
        }
    }
}

/// v0.8：一项挂起的人工审批。
pub struct ApprovalRequest {
    pub id: String,
    /// "run" | "write" | "patch"。
    pub kind: String,
    /// 人话详情，如 `git push`、`写入 build.rs`、`修补 src/main.rs`。
    pub detail: String,
    /// v1.0（模块三）：write / patch 审批的 Unified Diff 预览（截断 4000 字符）；
    /// run / mkdir 等没有文件内容对比的，None。
    pub diff_preview: Option<String>,
    pub created_at: u64,
    pub responder: mpsc::Sender<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    TimedOut,
    /// v2.3.3：审批队列已满（见 MAX_PENDING_APPROVALS），本次没有挂起。
    QueueFull,
}

/// v1.0（模块二）：一次写前快照。修改已存在文件前，把原文件原样记下来；
/// 新建文件记 existed=false（回滚 = 删掉这个新建文件）。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub root: String,
    pub rel: String,
    pub action: String,
    pub created_at: u64,
    pub existed: bool,
    /// 原文件字节（existed 且 <= 2MB 时才有）。
    pub content: Option<Vec<u8>>,
    /// 移动类动作的落点：rename 的新路径 / trash 的回收站内相对路径
    /// （如 ".pylon_trash/1723..._x.txt"），回滚时从这里移回来。
    pub moved_to: Option<String>,
    /// v2.3.3：已被回滚过（防重复点击把同一张快照反复写回）。
    pub reverted: bool,
}

/// 快照环形队列上限（任务书模块二）。
pub const MAX_SNAPSHOTS: usize = 30;
/// v2.3.3：快照总字节上限（超过就丢最旧的）。
pub const MAX_SNAPSHOT_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// v2.2.0：最近一次运行状态摘要（GET /api/program 的 last_run 字段）。
/// 同步 /api/run、/api/program 改后即跑、异步任务都会更新它。
#[derive(Debug, Clone)]
pub struct LastRun {
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub at_ms: u64,
    pub task_id: Option<String>,
    pub async_run: bool,
}

impl LastRun {
    pub fn to_json(&self) -> Value {
        json!({
            "argv": self.argv,
            "exit_code": self.exit_code,
            "timed_out": self.timed_out,
            "at_ms": self.at_ms,
            "task_id": self.task_id,
            "async": self.async_run,
        })
    }
}

/// v2.3.3：认证失败计数（按客户端 IP）——审查报告 P1 #6「认证无任何失败计数 /
/// 限流 / 退避」。超过阈值后短时间内直接 429，不再做昂贵的验签 / 比对。
pub struct AuthFailure {
    pub count: u32,
    pub first_at: Instant,
    pub locked_until: Option<Instant>,
}

/// v0.9：服务全局状态。HTTP 线程、托盘线程、隧道子进程、网络管理器共享同一份。
pub struct ServerState {
    /// v0.9：实时配置（RwLock）。面板保存后立刻换掉这份，
    /// 每次请求都现读，权限级别 / token / 白名单改完无需重启就生效。
    /// 网络监听地址与隧道子进程由 NetworkManager 另行热切换（见 network.rs）。
    pub cfg: Arc<RwLock<BridgeConfig>>,
    /// bridge-config.json 的实际路径（/ui/save-config 写回用）。
    pub config_path: PathBuf,
    pub logs: SharedLogs,
    pub approvals: Arc<Mutex<VecDeque<ApprovalRequest>>>,
    /// v1.0（模块二）：写前快照环形队列（上限 30 条）。
    pub snapshots: Arc<Mutex<VecDeque<Snapshot>>>,
    /// v1.0（模块三）：审批序号单调递增；托盘线程靠它发现新审批（比数队列长度可靠）。
    pub approval_seq: Arc<std::sync::atomic::AtomicU64>,
    /// 紧急熔断：true = 暂停云端写入（write / mkdir / run / patch 一律 403）。
    /// 托盘菜单与 Web 面板都能开关；只在内存里，重启即恢复 false。
    pub kill_switch: Arc<AtomicBool>,
    /// 配置变更通知：/ui/save-config 写完新配置后往这里发一个信号，
    /// NetworkManager 的监听线程收到就热切换监听 / 隧道 / 反向连接。
    /// 没有接收方时（单测）直接忽略。
    pub notify_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    /// v2.2.0：异步后台长任务引擎（/api/tasks/*）。
    pub tasks: Arc<crate::tasks::TaskManager>,
    /// v2.2.0：最近一次运行摘要（GET /api/program 的 last_run）。
    pub last_run: Arc<Mutex<Option<LastRun>>>,
    /// v2.3.3（P1 #6）：认证失败计数，按客户端地址隔离。
    pub auth_failures: Arc<Mutex<HashMap<String, AuthFailure>>>,
    /// v2.3.3（P1 #6）：HMAC 已用签名去重表（签名 -> 过期时间戳秒）。
    /// 时间窗内同一签名第二次出现即判重放。
    pub seen_signatures: Arc<Mutex<HashMap<String, u64>>>,
    /// v2.3.3（P1 #13）：主程序文件的读-校验-写临界区。
    /// 把「读旧哈希 -> 比对 expected_sha256 -> rename」串起来，
    /// 并发两个 POST /api/program 不再双双以旧哈希通过后互相覆盖。
    pub program_write_lock: Arc<Mutex<()>>,
    pub started_at: Instant,
}

impl ServerState {
    pub fn new(cfg: BridgeConfig, config_path: PathBuf, logs: SharedLogs) -> Self {
        Self {
            cfg: Arc::new(RwLock::new(cfg)),
            config_path,
            logs,
            approvals: Arc::new(Mutex::new(VecDeque::new())),
            snapshots: Arc::new(Mutex::new(VecDeque::new())),
            approval_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            kill_switch: Arc::new(AtomicBool::new(false)),
            notify_tx: Arc::new(Mutex::new(None)),
            tasks: Arc::new(crate::tasks::TaskManager::new()),
            last_run: Arc::new(Mutex::new(None)),
            auth_failures: Arc::new(Mutex::new(HashMap::new())),
            seen_signatures: Arc::new(Mutex::new(HashMap::new())),
            program_write_lock: Arc::new(Mutex::new(())),
            started_at: Instant::now(),
        }
    }

    /// 读一份当前配置快照（每次请求现读，热重载靠它）。
    ///
    /// v2.3.3（审查报告 P2）：`RwLock` 中毒不再 `unwrap`。
    /// 只要有一个线程持写锁时 panic，旧实现的 `unwrap()` 会让**之后每个请求**
    /// 在鉴权之前就 panic → 整个服务不可用。配置是纯数据，中毒后直接取回内部值
    /// 继续用即可（最坏是拿到一份半更新的配置，仍远好过全局宕机）。
    pub fn config(&self) -> BridgeConfig {
        match self.cfg.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 换掉实时配置（/ui/save-config 用），并通知 NetworkManager 热切换。
    pub fn update_config(&self, cfg: BridgeConfig) {
        match self.cfg.write() {
            Ok(mut g) => *g = cfg,
            Err(poisoned) => *poisoned.into_inner() = cfg,
        }
        self.notify_config_changed();
    }

    /// 通知 NetworkManager：配置变了，该热切换了。没有接收方时静默跳过。
    pub fn notify_config_changed(&self) {
        if let Ok(g) = self.notify_tx.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(());
            }
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// v0.8：统一的 HTTP 响应。JSON 与 HTML 都走它，
/// `dispatch_request` 只产这个，`handle_client` 只负责发。
pub struct HttpResponse {
    pub code: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(code: u16, reason: &'static str, v: Value) -> Self {
        Self {
            code,
            reason,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn err(code: u16, reason: &'static str, msg: &str) -> Self {
        Self::json(code, reason, json!({"ok": false, "error": msg}))
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// v2.2.0：SHA-256 小写 hex（Program Capsule 的 expected_sha256 乐观锁用）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(bytes))
}

/// 定长比较（防时序侧信道，HMAC 验签用）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Windows 提示音（审批挂起时响一声）。Linux 下什么都不做。
pub fn beep() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(
            windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK,
        );
    }
}

// ---------------------------------------------------------------------------
// v0.8 漏洞 4：可选防重放 HMAC
// ---------------------------------------------------------------------------

/// HMAC-SHA256(key=token, msg = timestamp + method + full_path + body)。
/// full_path 是请求行里的完整目标（含 query）；timestamp 是 header 原样字符串。
/// 签名是小写 hex。调用方（测试 / 以后的客户端）必须用同一套拼接方式。
pub fn hmac_sha256_hex(key: &[u8], timestamp: &str, method: &str, full_path: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key 长度合法");
    mac.update(timestamp.as_bytes());
    mac.update(method.as_bytes());
    mac.update(full_path.as_bytes());
    mac.update(body);
    hex_encode(&mac.finalize().into_bytes())
}

/// 验签 + 时间差检查 + 重放去重。返回 Ok 表示通过，Err 是人话原因。
///
/// v2.3.3（审查报告 P1 #6）：
/// - 时间窗内**同一签名只允许用一次**（`ServerState::seen_signatures`）。
///   旧实现只验时间差，60 秒窗口内同一签名可无限重放，与 README 的「防重放」
///   承诺不符；即使开了 `require_hmac` 也照样成立。
/// - 时间差用 i128 计算：`ts = i64::MIN` 时 `(now - ts).abs()` 在 debug 构建会
///   溢出 panic（报告 P2），i128 下不会有这个问题。
/// - 错误文案不再回显服务端时间戳（旧版把 `now` 直接写给调用方，
///   等于免费送一个校时 / 判断时钟偏移的探针）。
fn verify_hmac(
    state: &ServerState,
    headers: &HashMap<String, String>,
    method: &str,
    full_path: &str,
    body: &[u8],
) -> Result<(), String> {
    let ts = headers
        .get("x-bridge-timestamp")
        .ok_or_else(|| "缺 X-Bridge-Timestamp 请求头".to_string())?;
    let sig = headers
        .get("x-bridge-signature")
        .ok_or_else(|| "缺 X-Bridge-Signature 请求头".to_string())?;
    let ts_secs: i64 = ts
        .trim()
        .parse()
        .map_err(|_| "X-Bridge-Timestamp 不是合法的 Unix 秒数".to_string())?;
    let now = now_unix_secs() as i64;
    let skew = (now as i128) - (ts_secs as i128);
    if skew.abs() > HMAC_MAX_SKEW_SECS as i128 {
        return Err(format!(
            "X-Bridge-Timestamp 与服务端时间偏差超过 ±{HMAC_MAX_SKEW_SECS} 秒"
        ));
    }
    let expected = hmac_sha256_hex(
        state.config().token.as_bytes(),
        ts.trim(),
        method,
        full_path,
        body,
    );
    if !constant_time_eq(expected.as_bytes(), sig.trim().as_bytes()) {
        return Err("签名不对（HMAC-SHA256 校验失败）".to_string());
    }
    // v2.3.3：窗口内重放去重。签名是 hex、大小写不敏感，统一小写做键。
    let sig_key = sig.trim().to_ascii_lowercase();
    let expire_at = now_unix_secs().saturating_add(HMAC_MAX_SKEW_SECS as u64 + 1);
    let mut seen = match state.seen_signatures.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now_secs = now_unix_secs();
    seen.retain(|_, exp| *exp >= now_secs);
    if seen.contains_key(&sig_key) {
        return Err("该签名已在有效时间窗内使用过（疑似重放）".to_string());
    }
    if seen.len() >= MAX_SEEN_SIGNATURES {
        // 表满：正常量级（60 秒窗口）远达不到，清空只为防止内存被灌爆。
        seen.clear();
    }
    seen.insert(sig_key, expire_at);
    Ok(())
}

/// v2.3.3：认证失败计数 / 锁定的客户端键。拿不到对端地址时归到一个桶里。
fn auth_client_key(peer_ip: Option<IpAddr>) -> String {
    peer_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "internal".to_string())
}

/// 当前是否处于锁定期；返回还需等待的秒数。
fn auth_locked_secs(state: &ServerState, key: &str) -> Option<u64> {
    let g = match state.auth_failures.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let f = g.get(key)?;
    let until = f.locked_until?;
    let now = Instant::now();
    if until > now {
        Some(until.duration_since(now).as_secs().max(1))
    } else {
        None
    }
}

/// 记一次失败；窗口内累计超阈值就把该客户端锁一段时间。
fn record_auth_failure(state: &ServerState, key: &str) {
    let mut g = match state.auth_failures.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    // 顺手清掉过期条目，避免被大量随机源地址灌满内存。
    if g.len() > 1024 {
        g.retain(|_, f| {
            now.duration_since(f.first_at).as_secs() < AUTH_FAILURE_WINDOW_SECS
                && f.locked_until.map(|u| u > now).unwrap_or(false)
        });
    }
    let entry = g.entry(key.to_string()).or_insert(AuthFailure {
        count: 0,
        first_at: now,
        locked_until: None,
    });
    if now.duration_since(entry.first_at).as_secs() >= AUTH_FAILURE_WINDOW_SECS {
        entry.count = 0;
        entry.first_at = now;
        entry.locked_until = None;
    }
    entry.count = entry.count.saturating_add(1);
    if entry.count >= AUTH_MAX_FAILURES {
        entry.locked_until = Some(now + Duration::from_secs(AUTH_LOCKOUT_SECS));
        entry.count = 0;
        entry.first_at = now;
    }
}

fn clear_auth_failures(state: &ServerState, key: &str) {
    if let Ok(mut g) = state.auth_failures.lock() {
        g.remove(key);
    }
}

/// 远程接口鉴权（/health 与 /api/*）。
/// - require_hmac=false（默认）：只认 X-Bridge-Token 请求头（兼容老调用）；
/// - require_hmac=true：改认时间戳 + 签名，token 本人不用再传（它是 HMAC 的 key）。
///
/// v2.3.3：
/// - token 比较改常量时间（旧版 `t == &cfg.token` 是短路比较，报告 P2）；
/// - 失败按客户端计数，超阈值 429 + 冷却期（P1 #6：认证零限流）；
/// - 成功即清零，正常客户端不会被误锁。
fn check_remote_auth(
    state: &ServerState,
    headers: &HashMap<String, String>,
    method: &str,
    full_path: &str,
    body: &[u8],
    peer_ip: Option<IpAddr>,
) -> Result<(), HttpResponse> {
    let key = auth_client_key(peer_ip);
    if let Some(wait) = auth_locked_secs(state, &key) {
        return Err(HttpResponse::err(
            429,
            "Too Many Requests",
            &format!("认证失败次数过多，请 {wait} 秒后再试"),
        ));
    }
    let cfg = state.config();
    let outcome = if cfg.require_hmac {
        verify_hmac(state, headers, method, full_path, body)
    } else {
        let ok = headers
            .get("x-bridge-token")
            .map(|t| {
                !t.is_empty()
                    && !cfg.token.is_empty()
                    && constant_time_eq(t.as_bytes(), cfg.token.as_bytes())
            })
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err("unauthorized：请带正确的 X-Bridge-Token 请求头".to_string())
        }
    };
    match outcome {
        Ok(()) => {
            clear_auth_failures(state, &key);
            Ok(())
        }
        Err(msg) => {
            record_auth_failure(state, &key);
            Err(HttpResponse::err(401, "Unauthorized", &msg))
        }
    }
}

// ---------------------------------------------------------------------------
// v0.8 漏洞 1：敏感路径写保护
// ---------------------------------------------------------------------------

/// 写保护分级。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteProtection {
    /// 可以直接写。
    Allowed,
    /// 必须走人工审批（用户在面板看清路径点“允许”后才落盘）。
    NeedsApproval,
    /// 硬性禁止远程修改，直接 403（不给审批机会）。
    Forbidden,
}

/// v0.8：按相对路径（root 内）判定写保护。
/// Windows 文件系统不区分大小写，且会静默剥掉每段路径末尾的 '.' 与 ' '，
/// 所以比对前必须把每段归一化：先剥尾部 '.'/' ' 再转小写，否则
/// `BUILD.RS.`、`start.BAT␣`、`.GIT./config`、`bridge-config.json.` 这类写法
/// 在 NTFS 上与 build.rs / start.bat / .git/config / bridge-config.json 是
/// 同一个文件，却会绕过拦截。
///
/// v2.3.3（审查报告 P0）：再加一维——NTFS 备用数据流分隔符 `:`。
/// `bridge-config.json::$DATA` 归一化后是 `bridge-config.json::$data`，
/// 旧实现不切 `:` 就匹配不上黑名单，而 `std::fs` 在 verbatim 路径下依然
/// 把它解析到主数据流（实机已验证能读出明文 token / 写回配置）。
/// 现在统一用 `normalize_rel_segments`：先切掉第一个 `:` 及其后内容，再剥尾点/尾空格、
/// 转小写。`resolve_inside` 另外直接拒绝含 `:` 的 path，两层都拦。
///
/// v2.1：敏感文件「读保护」——远程读接口（read / search / read_base64）
/// 严禁读出 Pylon 自身的配置文件与活动日志。
/// 拦的是文件名本身（归一化后比对），所以 `bridge-config.json`、
/// `BRIDGE-CONFIG.JSON.`、`pylon-activity.log`、`.old` 轮转文件全拦；
/// 路径里任何一层是 `.git` 也拦。
/// 注意：这只是远程接口的保护——本机窗口读自己的日志与配置不走这条路。
pub fn check_read_protected(rel: &str) -> bool {
    let parts = normalize_rel_segments(rel);
    if parts.iter().any(|p| p == ".git") {
        return true;
    }
    match parts.last() {
        Some(f) => is_sensitive_file_name(f),
        None => false,
    }
}

/// v2.3.3：单段归一化——切掉 NTFS 流分隔符之后的内容，剥尾部 '.'/' '，转小写。
fn normalize_segment(seg: &str) -> String {
    let head = seg.split(':').next().unwrap_or(seg);
    head.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

/// v2.3.3：把相对路径切成归一化后的段（去掉空段与 `.`）。
fn normalize_rel_segments(rel: &str) -> Vec<String> {
    rel.replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(normalize_segment)
        .filter(|s| !s.is_empty() && s != ".")
        .collect()
}

/// v2.3.3：文件名本身是否属于「敏感」（配置文件 / 活动日志）。
/// 入参应当是已经归一化的段（小写、无尾点、无流后缀）；内部再归一化一次，
/// 方便调用方直接传原始文件名。
pub fn is_sensitive_file_name(name: &str) -> bool {
    let f = normalize_segment(name);
    f == "bridge-config.json" || f == "pylon-activity.log" || f == "pylon-activity.log.old"
}

/// v2.3.3（审查报告 P0 第二层）：对**解析后的真实路径**再做一次判定。
/// 判定字符串（`rel`）与使用对象（`PathBuf`）分离是 P0 的根因：
/// 解析结果可能是 `\\?\C:\...\bridge-config.json`，也可能被 junction
/// 换成别的目录。落盘 / 读盘前用真实文件名复检，`::$DATA`、尾点、
/// 大小写在 canonicalize 之后都不再是变体。
pub fn is_sensitive_target(path: &Path) -> bool {
    if path.components().any(|c| match c {
        std::path::Component::Normal(s) => normalize_segment(&s.to_string_lossy()) == ".git",
        _ => false,
    }) {
        return true;
    }
    match path.file_name() {
        Some(n) => is_sensitive_file_name(&n.to_string_lossy()),
        None => false,
    }
}

pub fn check_write_protected(rel: &str) -> WriteProtection {
    let parts = normalize_rel_segments(rel);
    // 硬禁：任何一层是 .git、文件名是 bridge-config.json。
    if parts.iter().any(|p| p == ".git") {
        return WriteProtection::Forbidden;
    }
    if let Some(file) = parts.last() {
        if file == "bridge-config.json" {
            return WriteProtection::Forbidden;
        }
    }
    // 审批：回收站内、任何一层是 .cargo、文件名是 build.rs、危险后缀。
    if parts.first().map(|p| p == TRASH_DIR).unwrap_or(false) {
        return WriteProtection::NeedsApproval;
    }
    if parts.iter().any(|p| p == ".cargo") {
        return WriteProtection::NeedsApproval;
    }
    if let Some(file) = parts.last() {
        if file == "build.rs" {
            return WriteProtection::NeedsApproval;
        }
        for ext in [".bat", ".cmd", ".ps1", ".exe", ".dll"] {
            if file.ends_with(ext) {
                return WriteProtection::NeedsApproval;
            }
        }
    }
    WriteProtection::Allowed
}

// ---------------------------------------------------------------------------
// v0.8 漏洞 2：命令参数级消毒
// ---------------------------------------------------------------------------

/// v0.8：前缀匹配通过后，再过一遍参数级消毒。
/// - python：禁 `-c`、`-m`（除非整条恰好是白名单里的 `python -m pip --version`），
///   第一个非 flag 参数必须是白名单内已存在的 `.py` 文件；
/// - git：禁 --upload-pack / --receive-pack / --exec / --output / -c，
///   外加 -C / --git-dir / --work-tree（这三个能把 git 的工作目录指到白名单外，
///   是任务书之外顺手补的加固）；
/// - 其他命令：不额外限制。
///
/// v2.3.3（审查报告 P1 #9）：匹配前先把 `argv[0]` 归一化（`normalize_program_name`）。
/// 旧实现对字面量精确匹配，`["python.exe","-c",...]` / `["PY.EXE","-c",...]`
/// 落进 `_ => Ok(())`，于是 `ask_always` 档的「自动 403 门」退化成靠人眼看审批弹窗，
/// 而审批详情只是 `argv.join(" ")`，变体并不显眼。
pub fn validate_argv_safety(
    cfg: &BridgeConfig,
    root: &str,
    rel: &str,
    argv: &[String],
) -> Result<(), String> {
    if argv.is_empty() {
        return Err("argv 为空".to_string());
    }
    let prog = normalize_program_name(&argv[0]);
    match prog.as_str() {
        // `py` 是 Windows 的 Python 启动器，`py -c` 同样能执行任意代码。
        "python" | "python3" | "py" => {
            // 精确放行 python -m pip --version（白名单里本来就有这一条）。
            if argv.len() == 4 && argv[1] == "-m" && argv[2] == "pip" && argv[3] == "--version" {
                return Ok(());
            }
            for a in &argv[1..] {
                if a == "-c" || a == "-m" {
                    return Err(format!("python 不允许 {a}（-c / -m 会绕过脚本文件检查）"));
                }
            }
            if let Some(script) = argv[1..].iter().find(|a| !a.starts_with('-')) {
                if !script.to_lowercase().ends_with(".py") {
                    return Err(format!("python 只能跑 .py 脚本：{script}"));
                }
                // 脚本路径是相对运行目录（rel）的，先拼到 rel 再进白名单校验。
                let joined = if rel.is_empty() || rel == "." {
                    script.clone()
                } else {
                    format!("{}/{}", rel.trim_end_matches('/'), script)
                };
                let target = cfg.resolve_inside(root, &joined)?;
                if !target.is_file() {
                    return Err(format!("python 脚本不存在：{script}"));
                }
            }
            Ok(())
        }
        "git" => {
            for a in &argv[1..] {
                let bad = a == "--upload-pack"
                    || a.starts_with("--upload-pack=")
                    || a == "--receive-pack"
                    || a.starts_with("--receive-pack=")
                    || a == "--exec"
                    || a.starts_with("--exec=")
                    || a.starts_with("--output")
                    || a == "-c"
                    || a.starts_with("-c")
                    || a == "-C"
                    || a.starts_with("-C")
                    || a == "--git-dir"
                    || a.starts_with("--git-dir=")
                    || a == "--work-tree"
                    || a.starts_with("--work-tree=");
                if bad {
                    return Err(format!("git 不允许危险参数：{a}"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// v0.8 原语 1：行级 / 块级热修补（纯函数，可单测）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PatchError {
    /// 某个 search 一次都没出现。
    NotFound,
    /// 某个 search 出现 >= 2 次。
    Ambiguous,
    /// 其他问题（search 为空等）。
    Invalid(String),
}

/// v0.8：内存里按序应用全部 patches，返回新内容与应用处数。
/// - CRLF / LF 无感：先统一归一化为 \n 做精确匹配定位；
/// - 每个 search 必须恰好出现 1 次（0 次 -> NotFound，>=2 次 -> Ambiguous）。
///
/// v2.3.3（审查报告 P1 #14，数据损坏）：换行风格按「每个换行各自的原风格」还原，
/// 不再用 `original.contains("\r\n")` 一刀切。旧实现只要文件里出现过 CRLF
/// （比如混合换行的文件），写回时就把**所有** `\n` 转成 `\r\n` —— 混合文件被
/// 整体改成 CRLF，产生全文件 diff、Windows 工具链行为变化。
/// 现在的规则：
/// - 全文只有 LF -> 替换进来的换行是 LF；
/// - 全文只有 CRLF -> 是 CRLF；
/// - 混合 -> 取匹配位置附近那个换行原本的风格（区间内第一个换行，
///   没有就看本行行尾）。
pub fn apply_patches(
    original: &str,
    patches: &[(String, String)],
) -> Result<(String, usize), PatchError> {
    if patches.is_empty() {
        return Err(PatchError::Invalid("patches 为空".to_string()));
    }
    // 归一化 + 每个字节记一份「如果这是 \n，还原成哪种风格」。
    // 0 = LF，1 = CRLF。style 数组与 text 的字节一一对应。
    let mut text = String::with_capacity(original.len());
    let mut styles: Vec<u8> = Vec::with_capacity(original.len());
    let mut it = original.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\r' && it.peek() == Some(&'\n') {
            it.next();
            text.push('\n');
            styles.push(1);
        } else {
            text.push(c);
            // 每个 UTF-8 字节一个 style 槽（非换行一律记 0）。
            styles.resize(styles.len() + c.len_utf8(), 0);
        }
    }
    let mut applied = 0;
    for (search, replace) in patches {
        if search.is_empty() {
            return Err(PatchError::Invalid("search 不能为空".to_string()));
        }
        let s = normalize_newlines(search);
        let r = normalize_newlines(replace);
        let mut hits = text.match_indices(&s);
        let first = match hits.next() {
            Some((i, _)) => i,
            None => return Err(PatchError::NotFound),
        };
        if hits.next().is_some() {
            return Err(PatchError::Ambiguous);
        }
        let style = pick_newline_style(&text, &styles, first, s.len());
        let end = first + s.len();
        // 替换文本里的每个 '\n' 按选定风格还原；非换行字节的 style 记 0。
        text.replace_range(first..end, &r);
        let new_styles: Vec<u8> = r
            .bytes()
            .map(|b| if b == b'\n' { style } else { 0 })
            .collect();
        styles.splice(first..end, new_styles);
        applied += 1;
    }
    // 按 style 还原换行。
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    for (idx, ch) in text.char_indices() {
        if ch == '\n' && styles.get(idx).copied().unwrap_or(0) == 1 {
            out.push_str("\r\n");
        } else {
            out.push(ch);
        }
    }
    Ok((out, applied))
}

/// v2.3.3：把任意换行风格归一成 LF（用于 search / replace 的匹配）。
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// v2.3.3：匹配区间内用哪种换行风格。
/// 优先区间内第一个换行；区间里没有换行时用**本行行尾**（匹配之后最近的换行）
/// 的风格——这样「改这一行」不会把隔壁行的风格带过来；只有改到最后一行
/// （行尾没有换行）才回退到前一个换行。
fn pick_newline_style(text: &str, styles: &[u8], pos: usize, len: usize) -> u8 {
    if let Some(off) = text[pos..pos + len].find('\n') {
        return styles.get(pos + off).copied().unwrap_or(0);
    }
    if let Some(next) = text[pos + len..].find('\n') {
        return styles.get(pos + len + next).copied().unwrap_or(0);
    }
    if let Some(prev) = text[..pos].rfind('\n') {
        return styles.get(prev).copied().unwrap_or(0);
    }
    match text.find('\n') {
        Some(first) => styles.get(first).copied().unwrap_or(0),
        None => 0,
    }
}

/// v1.0（模块三）：行级 Unified Diff（纯函数，可单测）。
/// - 输出格式：`--- <path> (原)` / `+++ <path> (新)` 头 + `@@ -a,b +c,d @@` 块头，
///   `-` 行是删掉的、`+` 行是新增的、空格开头是上下文（前后各 3 行）；
/// - 两边行数都超过 1000 行时不跑 LCS（O(n*m) 太贵），改出摘要；
/// - 整体截断到 4000 字符（任务书要求）；内容无变化时返回空串。
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len() > 1000 || new_lines.len() > 1000 {
        return format!(
            "--- {path} (原)\n+++ {path} (新)\n@@ 文件较大（原 {} 行 -> 新 {} 行），Diff 预览已省略 @@\n",
            old_lines.len(),
            new_lines.len()
        );
    }
    // 行级 LCS 动态规划。
    let n = old_lines.len();
    let m = new_lines.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Eq,
        Del,
        Add,
    }
    let mut ops: Vec<(Op, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push((Op::Eq, old_lines[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push((Op::Del, old_lines[i]));
            i += 1;
        } else {
            ops.push((Op::Add, new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push((Op::Del, old_lines[i]));
        i += 1;
    }
    while j < m {
        ops.push((Op::Add, new_lines[j]));
        j += 1;
    }
    // 有变化的操作下标；没有就返回空串。
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Eq)
        .map(|(k, _)| k)
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    // 按 3 行上下文切 hunk，相邻（上下文重叠）的合并。
    const CTX: usize = 3;
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &k in &changed {
        let start = k.saturating_sub(CTX);
        let end = (k + CTX + 1).min(ops.len());
        if let Some(last) = hunks.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        hunks.push((start, end));
    }
    let mut out = format!("--- {path} (原)\n+++ {path} (新)\n");
    for (start, end) in hunks {
        let (mut old_n, mut new_n) = (0usize, 0usize);
        for (op, _) in &ops[..start] {
            match op {
                Op::Eq | Op::Del => old_n += 1,
                Op::Add => {}
            }
            match op {
                Op::Eq | Op::Add => new_n += 1,
                Op::Del => {}
            }
        }
        let (mut old_len, mut new_len) = (0usize, 0usize);
        for (op, _) in &ops[start..end] {
            match op {
                Op::Eq | Op::Del => old_len += 1,
                Op::Add => {}
            }
            match op {
                Op::Eq | Op::Add => new_len += 1,
                Op::Del => {}
            }
        }
        let old_start = if old_len == 0 { old_n } else { old_n + 1 };
        let new_start = if new_len == 0 { new_n } else { new_n + 1 };
        out.push_str(&format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"));
        for (op, line) in &ops[start..end] {
            let mark = match op {
                Op::Eq => ' ',
                Op::Del => '-',
                Op::Add => '+',
            };
            out.push(mark);
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut s: String = out.chars().take(4000).collect();
    if out.chars().count() > 4000 {
        s.push_str("\n…（已截断 4000 字符）");
    }
    s
}

// ---------------------------------------------------------------------------
// v0.8 原语 2：代码检索（纯函数，可单测）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// v2.3.3：搜索 / 变更扫描共用的跳过目录判定。
/// 归一化后比对（NTFS 语义），`TARGET.` / `NODE_MODULES` 这类写法同样跳过。
fn is_skip_dir_name(name: &str) -> bool {
    matches!(
        normalize_segment(name).as_str(),
        ".git" | "target" | "node_modules" | ".pylon_trash"
    )
}

/// v0.8：在 base 目录下递归搜 UTF-8 文本文件。
/// 跳过 .git / target / node_modules、二进制（非 UTF-8）与超过 2MB 的文件。
/// 返回 (命中列表, 是否因超 100 条被截断)。结果按文件路径排序，行号 1-based，
/// 匹配行截断到 300 字符（超了末尾加 …）。
///
/// v2.3.3（审查报告 P1 #5）：递归改用 `DirEntry::file_type()`，**不跟随符号链接**
/// / junction。旧实现用 `Path::is_dir()` / `is_file()`（底层 `metadata`，会跟随），
/// root 内放一个指向外部的 junction，递归搜索就能穿过它列出并读取白名单外的文件
/// —— 敏感文件过滤只看 `rel` 文件名，挡不住链接目标。
/// 对照 `handle_changes` 的写法（本来就是 `file_type()`）统一过来。
pub fn search_files(
    base: &Path,
    query: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<(Vec<SearchHit>, bool), String> {
    let re = if is_regex {
        let mut b = regex::RegexBuilder::new(query);
        b.case_insensitive(!case_sensitive);
        // v2.3.3：限制编译后体积与回溯规模，避免恶意正则把 CPU / 内存打满
        //（regex crate 本身不回溯，但大 `{n,m}` 仍会撑爆内存）。
        b.size_limit(1024 * 1024);
        b.dfa_size_limit(1024 * 1024);
        Some(
            b.build()
                .map_err(|e| format!("正则写错了：{e}"))?,
        )
    } else {
        None
    };
    let needle = if !is_regex && !case_sensitive {
        query.to_lowercase()
    } else {
        query.to_string()
    };

    // 先收集文件（排序后结果稳定），再逐个读。
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            // 不跟随链接：链接本身既不当目录递归，也不当文件读取。
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if is_skip_dir_name(&name) {
                    continue;
                }
                stack.push(p);
            } else if ft.is_file() {
                // v2.1：敏感文件不参与搜索——配置文件（含明文 token）与
                // 活动日志的内容不能经搜索接口读出。
                if is_sensitive_file_name(&name) {
                    continue;
                }
                let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy();
                if check_read_protected(&rel) {
                    continue;
                }
                files.push(p);
            }
        }
    }
    files.sort();

    let mut out: Vec<SearchHit> = Vec::new();
    let mut truncated = false;
    'outer: for p in files {
        if out.len() >= MAX_SEARCH_RESULTS {
            truncated = true;
            break;
        }
        if p.metadata().map(|m| m.len()).unwrap_or(0) > MAX_TEXT_BYTES as u64 {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue, // 二进制 / 非 UTF-8 跳过
        };
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        for (i, line) in content.lines().enumerate() {
            if out.len() >= MAX_SEARCH_RESULTS {
                truncated = true;
                break 'outer;
            }
            let matched = match &re {
                Some(r) => r.is_match(line),
                None => {
                    if case_sensitive {
                        line.contains(query)
                    } else {
                        line.to_lowercase().contains(&needle)
                    }
                }
            };
            if matched {
                let mut t: String = line.chars().take(300).collect();
                if line.chars().count() > 300 {
                    t.push('…');
                }
                out.push(SearchHit {
                    file: rel.clone(),
                    line: i + 1,
                    text: t,
                });
            }
        }
    }
    Ok((out, truncated))
}

// ---------------------------------------------------------------------------
// v0.8 原语 3：高危动作本地拦截审批
// ---------------------------------------------------------------------------

/// 带 Diff 预览的版本（v1.0 模块三：write / patch 审批带 Unified Diff）。
pub fn request_approval_with_diff(
    state: &ServerState,
    kind: &str,
    detail: &str,
    diff_preview: Option<String>,
) -> ApprovalOutcome {
    request_approval_timeout_with_diff(state, kind, detail, diff_preview, APPROVAL_TIMEOUT_SECS)
}

/// 带超时 + Diff 预览的完整版本。
pub fn request_approval_timeout_with_diff(
    state: &ServerState,
    kind: &str,
    detail: &str,
    diff_preview: Option<String>,
    timeout_secs: u64,
) -> ApprovalOutcome {
    let (tx, rx) = mpsc::channel::<bool>();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let req = ApprovalRequest {
        id: id.clone(),
        kind: kind.to_string(),
        detail: detail.to_string(),
        diff_preview,
        created_at: now_unix_secs(),
        responder: tx,
    };
    {
        // v2.3.3：队列上限。满了立刻返回 QueueFull（调用方映射 429），
        // 不占线程、不占内存，也不会让用户在面板里看到无穷无尽的待批项。
        let mut q = match state.approvals.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if q.len() >= MAX_PENDING_APPROVALS {
            drop(q);
            push_log(
                &state.logs,
                format!("审批队列已满（上限 {MAX_PENDING_APPROVALS}），拒绝新请求 [{kind}] {detail}"),
            );
            return ApprovalOutcome::QueueFull;
        }
        q.push_back(req);
    }
    // v1.0（模块三）：审批序号 +1，托盘线程靠它发现新审批、弹气泡。
    state
        .approval_seq
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    push_log(
        &state.logs,
        format!("审批挂起 [{kind}] {detail}（{timeout_secs} 秒内需在面板点允许）"),
    );
    beep();
    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(true) => {
            push_log(&state.logs, format!("审批通过 [{kind}] {detail}"));
            ApprovalOutcome::Approved
        }
        Ok(false) => {
            push_log(&state.logs, format!("审批拒绝 [{kind}] {detail}"));
            ApprovalOutcome::Denied
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let mut q = match state.approvals.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            q.retain(|r| r.id != id);
            push_log(&state.logs, format!("审批超时 [{kind}] {detail}"));
            ApprovalOutcome::TimedOut
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => ApprovalOutcome::Denied,
    }
}

/// 列出当前挂起的审批（Web 面板轮询用）。
pub fn list_approvals(state: &ServerState) -> Vec<Value> {
    let now = now_unix_secs();
    let q = match state.approvals.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    q.iter()
        .map(|r| {
            json!({
                "id": r.id,
                "kind": r.kind,
                "detail": r.detail,
                "diff_preview": r.diff_preview,
                "created_at": r.created_at,
                "age_secs": now.saturating_sub(r.created_at),
            })
        })
        .collect()
}

/// 用户在面板点允许 / 拒绝。返回 false 表示该 id 已不在队列（已超时或已被处理）。
pub fn resolve_approval(state: &ServerState, id: &str, allow: bool) -> bool {
    let mut q = match state.approvals.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(pos) = q.iter().position(|r| r.id == id) {
        let req = q.remove(pos).unwrap();
        let _ = req.responder.send(allow);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// v1.0（模块二）：写前快照与一键回滚
// ---------------------------------------------------------------------------

/// 给 root/rel 拍一张快照，放进环形队列（上限 30 条），返回快照 id。
/// 写成功后把 id 放进响应 JSON，动态记录靠它挂回滚按钮。
/// - 目标是文件且 <= 2MB：存原字节；
/// - 目标是目录：只记 is_dir（内容靠 moved_to 回滚）；
/// - 目标不存在：existed=false。
pub fn take_snapshot(
    state: &ServerState,
    root: &str,
    rel: &str,
    action: &str,
    moved_to: Option<String>,
) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let cfg = state.config();
    let (existed, content) = match cfg.resolve_inside(root, rel) {
        Ok(target) => {
            if target.is_file() {
                let size = target.metadata().map(|m| m.len()).unwrap_or(0);
                let content = if (size as usize) <= MAX_TEXT_BYTES {
                    std::fs::read(&target).ok()
                } else {
                    None
                };
                (true, content)
            } else if target.is_dir() {
                (true, None)
            } else {
                (false, None)
            }
        }
        Err(_) => (false, None),
    };
    let snap = Snapshot {
        id: id.clone(),
        root: root.to_string(),
        rel: rel.to_string(),
        action: action.to_string(),
        created_at: now_unix_secs(),
        existed,
        content,
        moved_to,
        reverted: false,
    };
    if let Ok(mut q) = state.snapshots.lock() {
        q.push_back(snap);
        while q.len() > MAX_SNAPSHOTS {
            q.pop_front();
        }
        // v2.3.3：除条数上限外再加一道总字节上限——30 张 2MB 快照最坏 60MB 常驻，
        // 长时间跑下来（尤其反复整文件覆写）不该无限吃内存。
        while q.len() > 1 {
            let total: usize = q
                .iter()
                .map(|s| s.content.as_ref().map(|c| c.len()).unwrap_or(0))
                .sum();
            if total <= MAX_SNAPSHOT_TOTAL_BYTES {
                break;
            }
            q.pop_front();
        }
    }
    id
}

/// v2.3.3：把一张快照标记为「已回滚」（幂等，找不到就算了）。
fn mark_snapshot_reverted(state: &ServerState, id: &str) {
    if let Ok(mut q) = state.snapshots.lock() {
        if let Some(s) = q.iter_mut().find(|s| s.id == id) {
            s.reverted = true;
        }
    }
}

/// 列出快照（新在前，供 GET /ui/snapshots；不带原字节，免得一次拉几十 MB）。
pub fn list_snapshots(state: &ServerState) -> Vec<Value> {
    let q = match state.snapshots.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    q.iter()
        .rev()
        .map(|s| {
            json!({
                "id": s.id,
                "root": s.root,
                "path": s.rel,
                "action": s.action,
                "created_at": s.created_at,
                "existed": s.existed,
                // v2.3.3：已回滚过的快照，界面可以置灰按钮。
                "reverted": s.reverted,
            })
        })
        .collect()
}

/// 按 id 回滚一张快照。返回 Ok(相对路径) / Err(人话原因)。
/// - 移动类（moved_to 有值）：把回收站 / 新路径里的东西移回原位；
/// - 有原字节：写回原文件（父目录自动建）；
/// - 新建文件（existed=false）：删掉它；
/// - 原目标是目录且无 moved_to：没东西可还原，报错。
pub fn revert_snapshot(state: &ServerState, id: &str) -> Result<String, String> {
    let snap = {
        let q = state
            .snapshots
            .lock()
            .map_err(|_| "快照队列被锁住了".to_string())?;
        q.iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| "快照不存在（可能已被 30 条上限挤掉）".to_string())?
    };
    // v2.3.3（审查报告 P2）：同一张快照只允许回滚一次。
    // 旧实现里连点「回滚」会把同一份旧内容反复写回，并每次都塞一张保护快照，
    // 30 条环形队列被瞬间填满、真正有用的历史被挤掉。
    if snap.reverted {
        return Err("这张快照已经回滚过；如需再次还原，请用回滚时生成的那张保护快照".to_string());
    }
    let cfg = state.config();
    let target = cfg.resolve_inside(&snap.root, &snap.rel)?;
    if let Some(moved) = &snap.moved_to {
        let from = cfg.resolve_inside(&snap.root, moved)?;
        if !from.exists() {
            return Err(format!(
                "回滚失败：回收站 / 新路径里已找不到它（{moved}）"
            ));
        }
        if target.exists() {
            return Err(format!(
                "回滚失败：{} 已存在同名文件/目录，请先处理再回滚",
                snap.rel
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建父目录失败：{e}"))?;
        }
        std::fs::rename(&from, &target).map_err(|e| format!("移回失败：{e}"))?;
        mark_snapshot_reverted(state, &snap.id);
        push_log(
            &state.logs,
            format!("已回滚 {}（从 {moved} 移回）", snap.rel),
        );
        return Ok(snap.rel.clone());
    }
    // 回滚前保护：目标文件当前存在时（moved_to 为空的写回/删除类回滚），
    // 先为它拍一张保护快照存入队列，防止手滑误点历史动态的「回滚」把
    // 最新修改不可逆覆盖/删除。保护快照本身也可以回滚（二次反悔）。
    // moved_to 类回滚在目标已存在时直接报错、不写文件，不需要保护。
    let protection_id = if target.is_file() {
        Some(take_snapshot(
            state,
            &snap.root,
            &snap.rel,
            "回滚前保护",
            None,
        ))
    } else {
        None
    };
    if snap.existed {
        if let Some(bytes) = &snap.content {
            if target.is_dir() {
                return Err("回滚失败：原目标现在是目录".to_string());
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("建父目录失败：{e}"))?;
            }
            std::fs::write(&target, bytes).map_err(|e| format!("写回失败：{e}"))?;
            mark_snapshot_reverted(state, &snap.id);
            let msg = if protection_id.is_some() {
                format!("已回滚 {}（已为当前文件拍保护快照，可再次回滚）", snap.rel)
            } else {
                format!("已回滚 {}", snap.rel)
            };
            push_log_snapshot(&state.logs, msg, protection_id);
            return Ok(snap.rel.clone());
        }
        return Err(
            "回滚失败：这张快照没存原内容（文件超过 2MB 或原目标是目录）".to_string(),
        );
    }
    // 新建文件：回滚 = 删掉它。
    if target.is_file() {
        std::fs::remove_file(&target).map_err(|e| format!("删除失败：{e}"))?;
        mark_snapshot_reverted(state, &snap.id);
        let msg = if protection_id.is_some() {
            format!(
                "已回滚 {}（已删除新建文件；已为当前文件拍保护快照，可再次回滚）",
                snap.rel
            )
        } else {
            format!("已回滚 {}（已删除新建文件）", snap.rel)
        };
        push_log_snapshot(&state.logs, msg, protection_id);
        return Ok(snap.rel.clone());
    }
    if target.exists() {
        return Err("回滚失败：原目标现在是目录，删不掉".to_string());
    }
    Ok(snap.rel.clone())
}

/// argv 是否以某条前缀逐 token 精确开头（空前缀不算匹配）。
fn argv_starts_with(argv: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty()
        && argv.len() >= prefix.len()
        && argv.iter().zip(prefix.iter()).all(|(a, p)| a == p)
}

fn parse_body(body: &[u8]) -> Result<Value, HttpResponse> {
    serde_json::from_slice(body)
        .map_err(|e| HttpResponse::err(400, "Bad Request", &format!("body 不是合法 JSON：{e}")))
}

// ---------------------------------------------------------------------------
// /api/* 处理函数
// ---------------------------------------------------------------------------

fn handle_list(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let cfg = state.config();
    let root = query.get("root").cloned().unwrap_or_default();
    let rel = query
        .get("path")
        .cloned()
        .unwrap_or_else(|| ".".to_string());
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if !target.exists() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{rel}"));
    }
    if !target.is_dir() {
        return HttpResponse::err(400, "Bad Request", &format!("不是目录：{rel}"));
    }
    let mut entries: Vec<Value> = Vec::new();
    let mut items: Vec<_> = match std::fs::read_dir(&target) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => return HttpResponse::err(403, "Forbidden", &format!("无权限：{e}")),
    };
    items.sort_by_key(|e| {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        (!is_dir, e.file_name().to_string_lossy().to_lowercase())
    });
    for entry in items.into_iter().take(MAX_LIST_ENTRIES) {
        let name = entry.file_name().to_string_lossy().into_owned();
        // v1.0（模块四）：回收站不出现在列表里（显式 path 进去看是另一回事）。
        // v2.3.3（审查报告 P2）：敏感文件同样不列——旧实现会把
        // bridge-config.json / pylon-activity.log 的文件名与修改时间
        // 直接摊给任何持 token 的调用方。
        let seg = normalize_segment(&name);
        if seg == TRASH_DIR || is_sensitive_file_name(&name) {
            continue;
        }
        // v2.3.3：用 DirEntry::file_type()（不跟随链接）；符号链接单独标出来，
        // 免得调用方以为它是普通目录 / 文件。
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let is_dir = ft.is_dir();
        let is_symlink = ft.is_symlink();
        let size = if is_dir {
            None
        } else {
            entry.metadata().ok().map(|m| m.len())
        };
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push(json!({"name": name, "type": if is_dir {"dir"} else {"file"}, "size": size, "mtime": mtime, "symlink": is_symlink}));
    }
    HttpResponse::json(
        200,
        "OK",
        json!({"ok": true, "root": root, "path": rel, "entries": entries}),
    )
}

fn handle_read(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let cfg = state.config();
    let root = query.get("root").cloned().unwrap_or_default();
    let rel = match query.get("path") {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return HttpResponse::err(400, "Bad Request", "缺少 path 参数"),
    };
    // v2.1：敏感文件读保护——配置文件（含明文 token）与活动日志禁读。
    if check_read_protected(&rel) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读取");
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    // v2.3.3（P0 第二层）：判定字符串与真正要读的 PathBuf 不是同一个东西，
    // 解析后再用真实文件名复检一次。
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读取");
    }
    if !target.exists() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{rel}"));
    }
    if !target.is_file() {
        return HttpResponse::err(400, "Bad Request", &format!("不是文件：{rel}"));
    }
    let size = match target.metadata() {
        Ok(m) => m.len(),
        Err(e) => return HttpResponse::err(403, "Forbidden", &format!("读不到文件状态：{e}")),
    };
    if size as usize > MAX_TEXT_BYTES {
        return HttpResponse::err(
            413,
            "Payload Too Large",
            "文件超过 2 MB，v0.1 不读（大文件 / 二进制后续再加）",
        );
    }
    match std::fs::read_to_string(&target) {
        Ok(content) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "root": root, "path": rel, "size": size, "content": content}),
        ),
        Err(_) => HttpResponse::err(
            415,
            "Unsupported Media Type",
            "不是 UTF-8 文本文件，v0.1 不读（二进制后续再加）",
        ),
    }
}

/// v0.9：人工审批的统一关口：挂起等用户点允许，拒绝/超时都变 403。
fn approval_gate(state: &ServerState, kind: &str, detail: &str) -> Result<(), HttpResponse> {
    approval_gate_with_diff(state, kind, detail, None)
}

/// 带 Diff 预览的版本（v1.0 模块三：write / patch 审批带 Unified Diff）。
fn approval_gate_with_diff(
    state: &ServerState,
    kind: &str,
    detail: &str,
    diff: Option<String>,
) -> Result<(), HttpResponse> {
    match request_approval_with_diff(state, kind, detail, diff) {
        ApprovalOutcome::Approved => Ok(()),
        ApprovalOutcome::Denied => Err(HttpResponse::err(403, "Forbidden", "已被用户拒绝")),
        ApprovalOutcome::TimedOut => Err(HttpResponse::err(
            403,
            "Forbidden",
            "用户 60 秒内没点确认，已按拒绝处理",
        )),
        // v2.3.3：队列满不挂起、不占线程，直接让调用方退避重试。
        ApprovalOutcome::QueueFull => Err(HttpResponse::err(
            429,
            "Too Many Requests",
            &format!(
                "审批队列已满（最多 {MAX_PENDING_APPROVALS} 项待批），请等面板处理完再试"
            ),
        )),
    }
}

/// v0.9：shell 解释器永远不放行（用户 2026-11-09 定下的永久规则）：
/// cmd / powershell 放行等于绕过白名单。full_access 下也不例外——
/// 任务书的“任意命令”指的是任意开发命令，不是 shell。
///
/// v2.3.3（审查报告 P1 #10）：判定前先把 prog 名归一到 NTFS 语义——
/// 去掉目录、剥尾部 '.'/' '、转小写、去 `.exe`。旧实现只做了小写 + `.exe`，
/// `cmd.exe.` / `powershell.exe ` 在 Win32 上会被规范化成同一个解释器，
/// 却因为多了尾点 / 尾空格匹配不上，FullAccess 档（不查白名单）下直接绕过红线。
pub fn normalize_program_name(argv0: &str) -> String {
    let lower = argv0.to_ascii_lowercase();
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(lower.as_str());
    // NTFS 会静默剥掉文件名末尾的 '.' 与 ' '（verbatim 路径除外，
    // 但 CreateProcess 走的是普通 Win32 路径语义）。
    let base = base.trim_end_matches(['.', ' ']);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    base.trim_end_matches(['.', ' ']).to_string()
}

pub fn is_shell_command(argv0: &str) -> bool {
    // 名单保持用户 2026-11-09 定下的那五个：只补归一化，不擅自扩表
    // （扩大禁令会误伤 `wsl <cmd>` 这类合法构建流程，属另一件事）。
    matches!(
        normalize_program_name(argv0).as_str(),
        "cmd" | "powershell" | "pwsh" | "bash" | "sh"
    )
}

/// v0.9：三档权限下的写 / 修补门控。
/// - 硬禁：bridge-config.json 三档都禁；.git/ 在 ask_always 与 smart_auto 下禁，
///   full_access 下按任务书只禁 bridge-config.json（其余全部直接放行）；
/// - ask_always：其余一律挂起审批；
/// - smart_auto：只有写保护路径（build.rs / .cargo / *.bat 等）挂起审批；
/// - full_access：其余全部直接放行。
/// v1.0：diff 是 write / patch 的 Unified Diff 预览，审批时一起发给面板。
///
/// v2.3.3（审查报告 P0）：判定改用与 `check_write_protected` 同一套归一化
/// （切 `:stream`、剥尾点/尾空格、转小写）。旧实现自己拼了一套 `to_lowercase()`
/// 就比，`bridge-config.json::$DATA` 归一化后是 `bridge-config.json::$data`，
/// 匹配不上硬禁；而 FullAccess 档又会短路跳过 `WriteProtection::Forbidden` 复检，
/// 于是三档全放行。现在「配置文件名」这一条在 match 之前、与权限档无关地先判，
/// 任何档位都拿不到写配置的口子。
fn guard_write(
    state: &ServerState,
    cfg: &BridgeConfig,
    kind: &str,
    rel: &str,
    diff: Option<String>,
) -> Result<(), HttpResponse> {
    let parts = normalize_rel_segments(rel);
    if parts.last().map(|s| s == "bridge-config.json").unwrap_or(false) {
        return Err(HttpResponse::err(
            403,
            "Forbidden",
            &format!("该路径禁止远程修改：{rel}（bridge-config.json 三档硬禁）"),
        ));
    }
    let prot = check_write_protected(rel);
    if prot == WriteProtection::Forbidden {
        // .git 硬禁：full_access 档按任务书放开，其余两档拦。
        if cfg.permission_mode != PermissionMode::FullAccess {
            return Err(HttpResponse::err(
                403,
                "Forbidden",
                &format!("该路径禁止远程修改：{rel}（.git 硬禁）"),
            ));
        }
    }
    let detail = match kind {
        "patch" => format!("修补 {rel}"),
        "write_base64" => format!("写入（二进制） {rel}"),
        "rename" => format!("重命名 {rel}"),
        _ => format!("写入 {rel}"),
    };
    match cfg.permission_mode {
        PermissionMode::FullAccess => Ok(()),
        PermissionMode::AskAlways => approval_gate_with_diff(state, kind, &detail, diff),
        PermissionMode::SmartAuto => match prot {
            WriteProtection::NeedsApproval => {
                approval_gate_with_diff(state, kind, &detail, diff)
            }
            WriteProtection::Forbidden => Err(HttpResponse::err(
                403,
                "Forbidden",
                &format!("该路径禁止远程修改：{rel}（.git 硬禁）"),
            )),
            _ => Ok(()),
        },
    }
}

/// v0.9：三档权限下的建目录门控（ask_always 下也要审批；其余两档直接放行）。
fn guard_mkdir(
    state: &ServerState,
    cfg: &BridgeConfig,
    rel: &str,
) -> Result<(), HttpResponse> {
    match cfg.permission_mode {
        PermissionMode::AskAlways => {
            approval_gate(state, "mkdir", &format!("新建文件夹 {rel}"))
        }
        _ => Ok(()),
    }
}

fn handle_write(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = match data.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            return HttpResponse::err(
                400,
                "Bad Request",
                "需要 root / path / content（content 必须是字符串）",
            )
        }
    };
    if content.len() > MAX_TEXT_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "内容超过 2 MB");
    }
    if rel.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 path");
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", &format!("该路径禁止远程修改：{rel}"));
    }
    if target.is_dir() {
        return HttpResponse::err(400, "Bad Request", &format!("目标是目录，不能写：{rel}"));
    }
    // v1.0（模块三）：先读原内容算 Diff（审批时面板看得见改了什么），再过门控；
    // 内容没变就不带预览。
    //
    // v2.3.3（审查报告 P2）：旧实现 `read_to_string(..).unwrap_or_default()`
    // 把「读失败」吞成空串——非 UTF-8 / 权限失败时 Diff 会变成「整文件新增」，
    // 误导审批者点批准；写回时又用文本覆盖，二进制文件直接被毁。
    // 现在：目标存在但读不出 UTF-8 就 415 拒绝，不再"猜"。
    let old_text = if target.exists() {
        let bytes = match std::fs::read(&target) {
            Ok(b) => b,
            Err(e) => {
                return HttpResponse::err(
                    403,
                    "Forbidden",
                    &format!("读不到原文件（{e}），拒绝盲写：{rel}"),
                )
            }
        };
        match String::from_utf8(bytes) {
            Ok(c) => c,
            Err(_) => {
                return HttpResponse::err(
                    415,
                    "Unsupported Media Type",
                    &format!("{rel} 不是 UTF-8 文本；改二进制请用 /api/write_base64"),
                )
            }
        }
    } else {
        String::new()
    };
    let diff = unified_diff(&rel, &old_text, &content);
    let diff_opt = if diff.is_empty() { None } else { Some(diff) };
    if let Err(r) = guard_write(state, &cfg, "write", &rel, diff_opt) {
        return r;
    }
    // v2.3.3（P1 #11a）：审批窗口结束后重新解析路径，防止中途被换成 junction。
    if let Err(r) = revalidate_target(&cfg, &root, &rel, &target) {
        return r;
    }
    // v1.0（模块二）：落盘前先拍快照。
    let snap_id = take_snapshot(state, &root, &rel, "write", None);
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return HttpResponse::err(403, "Forbidden", &format!("建父目录失败：{e}"));
        }
    }
    match std::fs::write(&target, &content) {
        Ok(()) => {
            let size = target.metadata().map(|m| m.len()).unwrap_or(0);
            HttpResponse::json(
                200,
                "OK",
                json!({"ok": true, "root": root, "path": rel, "size": size, "snapshot_id": snap_id}),
            )
        }
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("无权限：{e}")),
    }
}

fn handle_mkdir(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if rel.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 root / path");
    }
    // v2.3.3：建目录也走同一套归一化硬禁——否则可以先建一个叫
    // `bridge-config.json` 的目录，再让后续写配置的操作落到奇怪的状态。
    let parts = normalize_rel_segments(&rel);
    if parts.iter().any(|p| p == ".git")
        || parts
            .last()
            .map(|p| p == "bridge-config.json")
            .unwrap_or(false)
    {
        return HttpResponse::err(
            403,
            "Forbidden",
            &format!("该路径禁止建目录：{rel}"),
        );
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", &format!("该路径禁止建目录：{rel}"));
    }
    if let Err(r) = guard_mkdir(state, &cfg, &rel) {
        return r;
    }
    match std::fs::create_dir_all(&target) {
        Ok(()) => HttpResponse::json(200, "OK", json!({"ok": true, "root": root, "path": rel})),
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("无权限：{e}")),
    }
}

/// v0.8 原语 1：POST /api/patch。
/// 原子写回：同目录临时文件 + rename，写一半断电也不会剩半截文件。
fn handle_patch(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let patches: Vec<(String, String)> = match data.get("patches") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|p| {
                let s = p.get("search")?.as_str()?.to_string();
                let r = p.get("replace")?.as_str()?.to_string();
                Some((s, r))
            })
            .collect(),
        _ => Vec::new(),
    };
    if rel.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 root / path");
    }
    if patches.is_empty() {
        return HttpResponse::err(
            400,
            "Bad Request",
            "patches 不能为空，格式：[{\"search\": \"...\", \"replace\": \"...\"}]",
        );
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", &format!("该路径禁止远程修改：{rel}"));
    }
    if !target.is_file() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{rel}"));
    }
    let size = match target.metadata() {
        Ok(m) => m.len(),
        Err(e) => return HttpResponse::err(403, "Forbidden", &format!("读不到文件状态：{e}")),
    };
    if size as usize > MAX_TEXT_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "文件超过 2 MB");
    }
    let original = match std::fs::read_to_string(&target) {
        Ok(c) => c,
        Err(_) => {
            return HttpResponse::err(
                415,
                "Unsupported Media Type",
                "不是 UTF-8 文本文件，修补不了",
            )
        }
    };
    // v1.0：先在内存里算出新内容（search 找不到 / 歧义直接报错，不惊动审批），
    // 再算 Diff、过门控，最后才落盘。
    let (new_text, applied) = match apply_patches(&original, &patches) {
        Ok(v) => v,
        Err(PatchError::NotFound) => {
            return HttpResponse::err(
                404,
                "Not Found",
                "Patch Target Not Found：某个 search 在文件里一次都没出现",
            )
        }
        Err(PatchError::Ambiguous) => {
            return HttpResponse::err(
                409,
                "Conflict",
                "Ambiguous Patch Match：某个 search 出现 2 次以上，请多给几行上下文",
            )
        }
        Err(PatchError::Invalid(e)) => return HttpResponse::err(400, "Bad Request", &e),
    };
    let diff = unified_diff(&rel, &original, &new_text);
    let diff_opt = if diff.is_empty() { None } else { Some(diff) };
    if let Err(r) = guard_write(state, &cfg, "patch", &rel, diff_opt) {
        return r;
    }
    // v2.3.3（P1 #11a 第二个后果）：审批窗口内文件可能被别人改过。
    // 旧实现会把「窗口前读到的整份内容」按 patch 结果写回，等于把别人的修改
    // 静默覆盖掉。这里落盘前比对一次：磁盘内容已经不是我们基于的那一份就 409。
    if let Err(r) = revalidate_target(&cfg, &root, &rel, &target) {
        return r;
    }
    match std::fs::read_to_string(&target) {
        Ok(now) if now == original => {}
        _ => {
            return HttpResponse::err(
                409,
                "Conflict",
                "文件在审批期间被其他进程修改，本次修补已放弃；请重新读取后再提交",
            )
        }
    }
    let snap_id = take_snapshot(state, &root, &rel, "patch", None);
    let tmp = target.with_file_name(format!(
        ".pylon-patch-{}-{}.tmp",
        std::process::id(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    if std::fs::write(&tmp, new_text.as_bytes()).is_err() {
        return HttpResponse::err(500, "Internal Server Error", "写临时文件失败");
    }
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return HttpResponse::err(500, "Internal Server Error", "写回文件失败");
    }
    HttpResponse::json(
        200,
        "OK",
        json!({"ok": true, "root": root, "path": rel, "patches_applied": applied, "size": new_text.len(), "snapshot_id": snap_id}),
    )
}

/// v2.1.2：「文件即是程序」—— GET /api/program：读绑定的主程序单文件。
/// 远端 Meta 不用先 list、再拼路径，直接拿到当前唯一程序文件的完整源码
/// 与状态（path / size / mtime_ms）。
/// v2.2.0：额外返回 sha256（expected_sha256 乐观锁的基准）、line_count、
/// last_run（最近一次运行摘要：argv / exit_code / timed_out / at_ms /
/// task_id / async；从未跑过为 null）。
fn handle_program_get(state: &ServerState) -> HttpResponse {
    let cfg = state.config();
    let (root, rel, target) = match cfg.resolve_main_file() {
        Ok(v) => v,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    // v2.3.3：字符串判定 + 解析后的真实文件名复检（P0 第二层）。
    if check_read_protected(&rel) || is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读取");
    }
    if !target.is_file() {
        return HttpResponse::err(404, "Not Found", &format!("主程序文件不存在：{rel}"));
    }
    let size = match target.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            return HttpResponse::err(403, "Forbidden", &format!("读不到文件状态：{e}"))
        }
    };
    if size as usize > MAX_TEXT_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "主程序文件超过 2 MB");
    }
    match std::fs::read(&target) {
        Ok(bytes) => {
            let content = match String::from_utf8(bytes.clone()) {
                Ok(c) => c,
                Err(_) => {
                    return HttpResponse::err(
                        415,
                        "Unsupported Media Type",
                        "主程序文件不是 UTF-8 文本",
                    )
                }
            };
            let mtime_ms = target
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let last_run = state
                .last_run
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|r| r.to_json()))
                .unwrap_or(Value::Null);
            HttpResponse::json(
                200,
                "OK",
                json!({
                    "ok": true, "root": root, "path": rel, "size": size,
                    "mtime_ms": mtime_ms, "content": content,
                    "sha256": sha256_hex(&bytes),
                    "line_count": content.lines().count(),
                    "last_run": last_run,
                }),
            )
        }
        Err(_) => HttpResponse::err(
            415,
            "Unsupported Media Type",
            "主程序文件不是 UTF-8 文本",
        ),
    }
}

/// v2.3.3：一行正文 + 它自己的换行符（用于保留原文件的换行风格）。
struct SourceLine {
    body: String,
    ending: String,
}

/// v2.3.3：按行切分并**保留每行原换行符**（`\r\n` / `\n` / 文件末尾无换行）。
fn split_source_lines(s: &str) -> Vec<SourceLine> {
    let mut out: Vec<SourceLine> = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c == '\n' {
            let ending = if cur.ends_with('\r') {
                cur.pop();
                "\r\n".to_string()
            } else {
                "\n".to_string()
            };
            out.push(SourceLine {
                body: std::mem::take(&mut cur),
                ending,
            });
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(SourceLine {
            body: cur,
            ending: String::new(),
        });
    }
    out
}

/// v2.3.3：文件的主换行风格（插入新行时用它）。
fn dominant_ending(lines: &[SourceLine]) -> String {
    let crlf = lines.iter().filter(|l| l.ending == "\r\n").count();
    let lf = lines.iter().filter(|l| l.ending == "\n").count();
    if crlf > lf {
        "\r\n".to_string()
    } else {
        "\n".to_string()
    }
}

/// v2.1.2：行级热修补。patches 里的行号指原始文件（1-based），
/// 按行号倒序应用（高行先改，低行号不受影响；同行按原顺序）。
/// 每项：{"line": N, "content": "..."}（默认 replace）；
/// {"line": N, "op": "insert", "content": "..."}（在第 N 行前插入）；
/// {"line": N, "op": "delete"}（删第 N 行）。
/// 末尾换行符状态与原文件一致（原文件有就保留，没有就不加）。
///
/// v2.3.3（审查报告 P1 #14）：不再 `lines()` + `join("\n")`——`str::lines()`
/// 会把每行末尾的 `\r` 一起吃掉，拼回来只剩 LF。CRLF 文件经一次行级修补后
/// 整体变成 LF，产生全文件 diff。现在每行连自己的换行符一起存，
/// 替换保留该行原换行符，插入沿用相邻行的风格，删除整行带走。
fn apply_line_patches(original: &str, patches: &[Value]) -> Result<(String, usize), String> {
    let mut lines: Vec<SourceLine> = split_source_lines(original);
    struct LinePatch {
        line: usize,
        op: String,
        content: String,
    }
    let mut parsed: Vec<LinePatch> = Vec::new();
    for (idx, p) in patches.iter().enumerate() {
        let line = p
            .get("line")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("patches[{idx}] 缺 line（1-based 行号）"))?
            as usize;
        if line == 0 {
            return Err(format!("patches[{idx}] 行号从 1 开始，不能是 0"));
        }
        let op = p
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("replace")
            .to_string();
        if !matches!(op.as_str(), "replace" | "insert" | "delete") {
            return Err(format!(
                "patches[{idx}] op 只能是 replace / insert / delete"
            ));
        }
        if (op == "replace" || op == "insert") && p.get("content").is_none() {
            return Err(format!("patches[{idx}] {op} 需要 content"));
        }
        let content = p
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        parsed.push(LinePatch { line, op, content });
    }
    let default_ending = dominant_ending(&lines);
    // 倒序应用（稳定排序：同行保持原顺序）。
    let mut order: Vec<usize> = (0..parsed.len()).collect();
    order.sort_by(|&a, &b| parsed[b].line.cmp(&parsed[a].line).then(a.cmp(&b)));
    let mut applied = 0usize;
    for &i in &order {
        let p = &parsed[i];
        match p.op.as_str() {
            "replace" => {
                if p.line > lines.len() {
                    return Err(format!(
                        "行号 {} 超出文件总行数 {}",
                        p.line,
                        lines.len()
                    ));
                }
                lines[p.line - 1].body = p.content.clone();
                applied += 1;
            }
            "insert" => {
                if p.line > lines.len() + 1 {
                    return Err(format!(
                        "插入行号 {} 超出范围（最多 {}）",
                        p.line,
                        lines.len() + 1
                    ));
                }
                let idx = p.line - 1;
                if idx == lines.len() {
                    // 追加到末尾：末行没有换行符时先把它补上，再挂新行（新行不带换行）。
                    if let Some(last) = lines.last_mut() {
                        if last.ending.is_empty() {
                            last.ending = default_ending.clone();
                            lines.push(SourceLine {
                                body: p.content.clone(),
                                ending: String::new(),
                            });
                        } else {
                            let e = last.ending.clone();
                            lines.push(SourceLine {
                                body: p.content.clone(),
                                ending: e,
                            });
                        }
                    } else {
                        lines.push(SourceLine {
                            body: p.content.clone(),
                            ending: String::new(),
                        });
                    }
                } else {
                    let e = if idx > 0 {
                        lines[idx - 1].ending.clone()
                    } else {
                        lines[0].ending.clone()
                    };
                    lines.insert(
                        idx,
                        SourceLine {
                            body: p.content.clone(),
                            ending: e,
                        },
                    );
                }
                applied += 1;
            }
            "delete" => {
                if p.line > lines.len() {
                    return Err(format!(
                        "行号 {} 超出文件总行数 {}",
                        p.line,
                        lines.len()
                    ));
                }
                lines.remove(p.line - 1);
                applied += 1;
            }
            _ => unreachable!(),
        }
    }
    let mut out = String::with_capacity(original.len() + 32);
    for l in &lines {
        out.push_str(&l.body);
        out.push_str(&l.ending);
    }
    Ok((out, applied))
}

/// v2.1.2：POST /api/program：整文件覆写或行级热修补绑定的主程序单文件。
/// Body 二选一：
/// - {"content": "<完整新内容>"}：整文件覆写；
/// - {"patches": [{"line": N, "content": "..."}, ...]}：行级热修补。
/// 走 guard_write + 快照 + 原子写回（临时文件 + rename），响应带 snapshot_id，
/// 动态里可一键回滚。
/// v2.2.0 增强：
/// - expected_sha256（可选）：与当前文件哈希不匹配 -> 409 + 最新内容，
///   防止远端盲覆盖本地刚改过的文件（乐观锁）；
/// - run_argv（可选）+ async（可选）：原子拍快照 -> 行级热修补 -> 立即启动
///   运行，单次网络往返闭环。async:true 走后台任务引擎返回 task_id；
///   否则同步执行返回 run:{exit_code,stdout,stderr,...}。
///   运行鉴权在写文件之前做（失败不写文件，无半完成状态）。
/// v2.3.3（审查报告 P1 #11a）：审批窗口内目录可能被换成 junction / 软链接，
/// 或者目标被挪走。落盘前用同一套 `resolve_inside` 重新解析，结果必须与
/// 审批前一致；不一致就 409，绝不顺着新链接写出去。
fn revalidate_target(
    cfg: &BridgeConfig,
    root: &str,
    rel: &str,
    expected: &Path,
) -> Result<(), HttpResponse> {
    match cfg.resolve_inside(root, rel) {
        Ok(p) if p == expected => Ok(()),
        Ok(p) => Err(HttpResponse::err(
            409,
            "Conflict",
            &format!(
                "路径在审批期间发生了变化（现在解析到 {}），已放弃写入；请重新发起请求",
                p.display()
            ),
        )),
        Err(e) => Err(HttpResponse::err(
            409,
            "Conflict",
            &format!("路径在审批期间已不可解析（{e}），已放弃写入"),
        )),
    }
}

fn handle_program_post(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let (root, rel, target) = match cfg.resolve_main_file() {
        Ok(v) => v,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    // v2.3.3（审查报告 P1 #12）：写入端同样要过读保护。
    // 旧版只在 GET 检查，`main_file` 被绑到 bridge-config.json 时，
    // 带一个随便错的 expected_sha256 就能从 409 响应里读回全文（明文 token）。
    if check_read_protected(&rel) || is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读写");
    }
    if !target.is_file() {
        return HttpResponse::err(404, "Not Found", &format!("主程序文件不存在：{rel}"));
    }
    let size = match target.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            return HttpResponse::err(
                403,
                "Forbidden",
                &format!("读不到主程序文件状态（{e}），拒绝盲写"),
            )
        }
    };
    if size as usize > MAX_TEXT_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "主程序文件超过 2 MB");
    }
    let original_bytes = match std::fs::read(&target) {
        Ok(b) => b,
        Err(e) => {
            return HttpResponse::err(
                403,
                "Forbidden",
                &format!("读不到主程序文件（{e}），拒绝盲写"),
            )
        }
    };
    let original = match String::from_utf8(original_bytes.clone()) {
        Ok(c) => c,
        Err(_) => {
            return HttpResponse::err(
                415,
                "Unsupported Media Type",
                "主程序文件不是 UTF-8 文本",
            )
        }
    };
    // v2.2.0：乐观锁——文件已被本地修改（哈希不匹配）时拒绝盲覆盖。
    // v2.3.3：409 不再回显 `content`（见上方 P1 #12），只给哈希与大小，
    // 需要正文的调用方走 GET /api/program（那里有读保护）。
    let current_sha256 = sha256_hex(&original_bytes);
    if let Some(expected) = data.get("expected_sha256").and_then(|v| v.as_str()) {
        if !expected.is_empty() && expected != current_sha256 {
            let mtime_ms = target
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            return HttpResponse::json(
                409,
                "Conflict",
                json!({
                    "ok": false,
                    "error": "文件已被本地修改（sha256 不匹配），请先 GET /api/program 取最新内容再提交",
                    "current_sha256": current_sha256,
                    "size": size,
                    "mtime_ms": mtime_ms,
                    "line_count": original.lines().count(),
                }),
            );
        }
    }
    let (new_text, applied) = if let Some(content) =
        data.get("content").and_then(|v| v.as_str())
    {
        if content.len() > MAX_TEXT_BYTES {
            return HttpResponse::err(413, "Payload Too Large", "内容超过 2 MB");
        }
        (content.to_string(), 0usize)
    } else if let Some(Value::Array(items)) = data.get("patches") {
        if items.is_empty() {
            return HttpResponse::err(400, "Bad Request", "patches 不能为空");
        }
        match apply_line_patches(&original, items) {
            Ok(v) => v,
            Err(e) => return HttpResponse::err(400, "Bad Request", &e),
        }
    } else {
        return HttpResponse::err(
            400,
            "Bad Request",
            "需要 content（整文件覆写）或 patches（行级修补）",
        );
    };
    // v2.2.0：改后即跑——运行门控在写文件之前过（失败不写，无半完成状态）。
    let run_argv: Vec<String> = match data.get("run_argv") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    let run_async = data.get("async").and_then(|v| v.as_bool()).unwrap_or(false);
    let run_timeout = data
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RUN_TIMEOUT_SECS)
        .clamp(1, MAX_RUN_TIMEOUT_SECS);
    // 运行目录 = 主程序文件所在目录（相对 root）。
    let run_rel = match rel.rfind(['/', '\\']) {
        Some(i) => rel[..i].to_string(),
        None => ".".to_string(),
    };
    let run_cfg = if !run_argv.is_empty() {
        match authorize_run(state, &root, &run_rel, &run_argv, false, None) {
            Ok(c) => Some(c),
            Err(r) => return r,
        }
    } else {
        None
    };
    let diff = unified_diff(&rel, &original, &new_text);
    let diff_opt = if diff.is_empty() { None } else { Some(diff) };
    if let Err(r) = guard_write(state, &cfg, "write", &rel, diff_opt) {
        return r;
    }
    // v2.3.3：审批窗口结束，落盘前再确认三件事，任一不成立都不写：
    // 1) 路径没被换成 junction；2) 磁盘内容仍是本请求基于的那一份；
    // 3) 目标仍是普通文件（没被换成目录）。
    if let Err(r) = revalidate_target(&cfg, &root, &rel, &target) {
        return r;
    }
    // v2.3.3（P1 #13）：CAS 放在锁内——「比对哈希 + 写回」是一个原子段。
    // 锁只在最后这一小段持有，审批等待（最长 60 秒）不占锁，避免把并发写
    // 全都堵成一条长队。
    // 第二个并发请求会在这里读到「已经是别人写过的内容」，与它窗口前读到的
    // 不一致 -> 409，而不是无条件覆盖（旧实现正是无条件 rename）。
    let _write_guard = match state.program_write_lock.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match std::fs::read(&target) {
        Ok(now) if now == original_bytes => {}
        _ => {
            return HttpResponse::err(
                409,
                "Conflict",
                "文件在审批期间被其他进程修改，本次写入已放弃；请重新读取后再提交",
            )
        }
    }
    if target.is_dir() {
        return HttpResponse::err(409, "Conflict", "主程序文件在审批期间变成了目录，已放弃写入");
    }
    let snap_id = take_snapshot(state, &root, &rel, "write", None);
    let tmp = target.with_file_name(format!(
        ".pylon-program-{}-{}.tmp",
        std::process::id(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    if std::fs::write(&tmp, new_text.as_bytes()).is_err() {
        return HttpResponse::err(500, "Internal Server Error", "写临时文件失败");
    }
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return HttpResponse::err(500, "Internal Server Error", "写回文件失败");
    }
    // 写成功 -> 立即运行（同步或异步）。
    let mut resp = json!({
        "ok": true, "root": root, "path": rel,
        "patches_applied": applied, "size": new_text.len(),
        "snapshot_id": snap_id,
        "sha256": sha256_hex(new_text.as_bytes()),
    });
    if let Some(rcfg) = run_cfg {
        if run_async {
            match crate::tasks::spawn_task(state, &rcfg, &root, &run_rel, &run_argv, &[]) {
                Ok((task_id, pid)) => {
                    resp["task_id"] = json!(task_id);
                    resp["pid"] = json!(pid);
                    resp["async"] = json!(true);
                }
                Err(e) => {
                    resp["run_error"] = json!(e);
                }
            }
        } else {
            let outcome = run_operation(&rcfg, &root, &run_rel, &run_argv, run_timeout);
            if let Ok(mut lr) = state.last_run.lock() {
                *lr = Some(LastRun {
                    argv: run_argv.clone(),
                    exit_code: outcome.output.exit_code,
                    timed_out: outcome.output.timed_out,
                    at_ms: now_unix_secs() * 1000,
                    task_id: None,
                    async_run: false,
                });
            }
            resp["run"] = json!({
                "exit_code": outcome.output.exit_code,
                "stdout": outcome.output.stdout,
                "stderr": outcome.output.stderr,
                "timed_out": outcome.output.timed_out,
                "truncated": outcome.output.truncated,
            });
        }
    }
    HttpResponse::json(200, "OK", resp)
}

/// v2.1.2：GET /api/changes?root=<别名>&since_ms=<毫秒时间戳>。
/// 增量变更感知：扫描 root 下 mtime >= since_ms 的文件（跳过 .git /
/// target / node_modules / .pylon_trash），返回相对路径、size、mtime_ms，
/// 按时间倒序，最多 100 条。远端 Meta 用一个轻量请求就能毫秒级感知
/// “刚刚哪些文件变了”，不用全量 list。
fn handle_changes(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let cfg = state.config();
    let root = query.get("root").cloned().unwrap_or_default();
    let since_ms: u64 = match query.get("since_ms").and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => {
            return HttpResponse::err(
                400,
                "Bad Request",
                "缺少 since_ms（毫秒时间戳）",
            )
        }
    };
    let base = match cfg.resolve_inside(&root, ".") {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if !base.is_dir() {
        return HttpResponse::err(400, "Bad Request", "root 不是目录");
    }
    let mut out: Vec<(String, u64, u64)> = Vec::new(); // (rel, size, mtime_ms)
    let mut stack: Vec<PathBuf> = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let p = ent.path();
            let name = ent.file_name().to_string_lossy().into_owned();
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                // v2.3.3：归一化后比对（`TARGET.` / `NODE_MODULES` 同样跳过）。
                if is_skip_dir_name(&name) {
                    continue;
                }
                stack.push(p);
            } else if ft.is_file() {
                // v2.3.3（审查报告 P2）：配置文件 / 活动日志不出现在变更流里
                //（旧实现会把它们的文件名与时间戳摊出去）。
                if is_sensitive_file_name(&name) {
                    continue;
                }
                let (size, mtime_ms) = match ent.metadata() {
                    Ok(m) => {
                        let ms = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        (m.len(), ms)
                    }
                    Err(_) => continue,
                };
                if mtime_ms >= since_ms {
                    let rel = p
                        .strip_prefix(&base)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| name.clone());
                    out.push((rel, size, mtime_ms));
                }
            }
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out.truncate(100);
    let files: Vec<Value> = out
        .into_iter()
        .map(|(path, size, mtime_ms)| {
            json!({"path": path, "size": size, "mtime_ms": mtime_ms})
        })
        .collect();
    HttpResponse::json(
        200,
        "OK",
        json!({"ok": true, "root": root, "since_ms": since_ms, "count": files.len(), "files": files}),
    )
}

/// v0.8 原语 2：GET /api/search。
fn handle_search(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let cfg = state.config();
    let root = query.get("root").cloned().unwrap_or_default();
    let rel = query
        .get("path")
        .cloned()
        .unwrap_or_else(|| ".".to_string());
    let q = match query.get("query") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return HttpResponse::err(400, "Bad Request", "query 不能为空"),
    };
    let is_regex = query.get("is_regex").map(|s| s == "true").unwrap_or(false);
    let case_sensitive = query
        .get("case_sensitive")
        .map(|s| s == "true")
        .unwrap_or(false);
    let base = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if !base.is_dir() {
        return HttpResponse::err(400, "Bad Request", &format!("不是目录：{rel}"));
    }
    match search_files(&base, &q, is_regex, case_sensitive) {
        Ok((hits, truncated)) => {
            let results: Vec<Value> = hits
                .iter()
                .map(|h| json!({"file": h.file, "line": h.line, "text": h.text}))
                .collect();
            HttpResponse::json(
                200,
                "OK",
                json!({"ok": true, "root": root, "path": rel, "query": q, "results": results, "truncated": truncated}),
            )
        }
        Err(e) => HttpResponse::err(400, "Bad Request", &e),
    }
}

// ---------------------------------------------------------------------------
// v1.0（模块四）：文件操作闭环 —— 软删除、重命名、二进制传输
// ---------------------------------------------------------------------------

/// POST /api/trash —— 软删除：移入 root 下 `.pylon_trash/<时间戳>_<扁平化原名>`，
/// 绝不物理删除。移之前先拍快照（回滚能移回来）。
/// - .pylon_trash 本身、root 根目录、bridge-config.json、.git/ 下的一律不让移；
/// - ask_always / smart_auto 下走人工审批，full_access 下直接移入。
fn handle_trash(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if rel.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 root / path");
    }
    // v2.3.3：与写保护同一套归一化（切 `:stream`、剥尾点/尾空格、转小写），
    // 否则 `bridge-config.json::$DATA` / `.GIT./x` 这类变体能在回收站 / 重命名
    // 两条路径上绕过硬禁。
    let parts = normalize_rel_segments(&rel);
    if parts.is_empty() || parts[0] == TRASH_DIR {
        return HttpResponse::err(
            400,
            "Bad Request",
            "回收站本身不能再移入回收站",
        );
    }
    if parts.iter().any(|p| p == ".git")
        || parts.last().map(|p| p == "bridge-config.json").unwrap_or(false)
    {
        return HttpResponse::err(
            403,
            "Forbidden",
            &format!("该路径禁止移入回收站：{rel}"),
        );
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", &format!("该路径禁止移入回收站：{rel}"));
    }
    if !target.exists() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{rel}"));
    }
    match cfg.permission_mode {
        PermissionMode::FullAccess => {}
        _ => {
            if let Err(r) = approval_gate(state, "trash", &format!("移入回收站 {rel}")) {
                return r;
            }
        }
    }
    // v2.3.3（P1 #11a）：审批后重新解析，防止目标在窗口内被换成 junction。
    if let Err(r) = revalidate_target(&cfg, &root, &rel, &target) {
        return r;
    }
    // 回收站内名：<unix 秒>_<扁平化原名>，同秒重名加 8 位随机后缀。
    let flat = rel.replace('\\', "/").replace('/', "__");
    let trash_dir = match cfg.resolve_inside(&root, TRASH_DIR) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if let Err(e) = std::fs::create_dir_all(&trash_dir) {
        return HttpResponse::err(500, "Internal Server Error", &format!("建回收站失败：{e}"));
    }
    let mut trash_name = format!("{}_{}", now_unix_secs(), flat);
    let mut trash_path = trash_dir.join(&trash_name);
    if trash_path.exists() {
        trash_name = format!(
            "{}_{}_{}",
            now_unix_secs(),
            &uuid::Uuid::new_v4().simple().to_string()[..8],
            flat
        );
        trash_path = trash_dir.join(&trash_name);
    }
    let trash_rel = format!("{TRASH_DIR}/{trash_name}");
    let snap_id = take_snapshot(state, &root, &rel, "trash", Some(trash_rel.clone()));
    match std::fs::rename(&target, &trash_path) {
        Ok(()) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "root": root, "path": rel, "trash_path": trash_rel, "snapshot_id": snap_id}),
        ),
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("移入回收站失败：{e}")),
    }
}

/// POST /api/rename —— 重命名与移动。
/// - from 必须存在；to 的父目录自动建；
/// - to 已存在且是文件：先拍快照再覆盖；to 是目录：拒绝覆盖；
/// - .pylon_trash 内外都不让动（回收站只进不出，靠回滚移回）；
/// - bridge-config.json / .git/ 硬禁；权限门控同写。
fn handle_rename(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let from = data
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let to = data
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if from.is_empty() || to.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 root / from / to");
    }
    // v2.3.3：归一化后比较（`a.txt.` 与 `a.txt` 在 NTFS 上是同一个文件）。
    let norm = |s: &str| normalize_rel_segments(s).join("/");
    if norm(&from) == norm(&to) {
        return HttpResponse::err(400, "Bad Request", "from 与 to 相同");
    }
    for (label, p) in [("from", &from), ("to", &to)] {
        let parts = normalize_rel_segments(p);
        if parts.is_empty() || parts[0] == TRASH_DIR {
            return HttpResponse::err(
                400,
                "Bad Request",
                &format!("{label} 不能是回收站目录"),
            );
        }
        if parts.iter().any(|x| x == ".git")
            || parts.last().map(|x| x == "bridge-config.json").unwrap_or(false)
        {
            return HttpResponse::err(
                403,
                "Forbidden",
                &format!("该路径禁止重命名：{p}"),
            );
        }
    }
    let src = match cfg.resolve_inside(&root, &from) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    let dst = match cfg.resolve_inside(&root, &to) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&src) || is_sensitive_target(&dst) {
        return HttpResponse::err(403, "Forbidden", "该路径禁止重命名（敏感文件）");
    }
    if !src.exists() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{from}"));
    }
    // v2.2.1 复核：rename 的 from / to 两端都要过写保护门控，缺一不可——
    // 只拦 from 时，a.txt -> build.rs / setup.bat 会绕过写保护直接落盘。
    // - from：按写语义门控（smart_auto 只有写保护的源才审批；ask_always 一律审批，
    //   这一次确认覆盖整次重命名，后面 to 为 Allowed 时就不再重复弹窗）；
    // - to：真正决定“落成什么文件”的一端，按写保护单独过一遍（build.rs / *.bat /
    //   .cargo / 危险后缀）。bridge-config.json 与 .git 的硬禁已在上面统一拦掉。
    // 注意顺序：目标父目录的 create_dir_all 与「覆盖已有目标前删旧文件」都在门控之后，
    // 被拒绝的请求不会留下新建目录，也不会先删掉目标再失败。
    if let Err(r) = guard_write(state, &cfg, "rename", &from, None) {
        return r;
    }
    if check_write_protected(&to) != WriteProtection::Allowed {
        if let Err(r) = guard_write(state, &cfg, "rename", &to, None) {
            return r;
        }
    }
    // v2.3.3（P1 #11a）：两端都在审批后复检一次，防止窗口内被换成 junction。
    if let Err(r) = revalidate_target(&cfg, &root, &from, &src) {
        return r;
    }
    if let Err(r) = revalidate_target(&cfg, &root, &to, &dst) {
        return r;
    }
    if dst.is_dir() {
        return HttpResponse::err(
            400,
            "Bad Request",
            "目标是已存在的文件夹，不能覆盖",
        );
    }
    if dst.is_file() {
        // 覆盖前先给目标拍快照（被覆盖的文件还能从快照列表里找回来）。
        let _ = take_snapshot(state, &root, &to, "rename", None);
        if let Err(e) = std::fs::remove_file(&dst) {
            return HttpResponse::err(500, "Internal Server Error", &format!("删旧目标失败：{e}"));
        }
    }
    let snap_id = take_snapshot(state, &root, &from, "rename", Some(to.clone()));
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return HttpResponse::err(403, "Forbidden", &format!("建父目录失败：{e}"));
        }
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "root": root, "from": from, "to": to, "snapshot_id": snap_id}),
        ),
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("重命名失败：{e}")),
    }
}

/// GET /api/read_base64 —— 读二进制文件，上限 10MB。
fn handle_read_base64(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let cfg = state.config();
    let root = query.get("root").cloned().unwrap_or_default();
    let rel = match query.get("path") {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return HttpResponse::err(400, "Bad Request", "缺少 path 参数"),
    };
    // v2.1：敏感文件读保护——配置文件（含明文 token）与活动日志禁读。
    if check_read_protected(&rel) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读取");
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    // v2.3.3（P0 第二层）：真实文件名复检。
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", "敏感文件禁止远程读取");
    }
    if !target.exists() {
        return HttpResponse::err(404, "Not Found", &format!("不存在：{rel}"));
    }
    if !target.is_file() {
        return HttpResponse::err(400, "Bad Request", &format!("不是文件：{rel}"));
    }
    let size = match target.metadata() {
        Ok(m) => m.len(),
        Err(e) => return HttpResponse::err(403, "Forbidden", &format!("读不到文件状态：{e}")),
    };
    if size as usize > MAX_BASE64_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "文件超过 10 MB");
    }
    match std::fs::read(&target) {
        Ok(bytes) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "root": root, "path": rel, "size": size, "content_base64": base64_encode(&bytes)}),
        ),
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("无权限：{e}")),
    }
}

/// POST /api/write_base64 —— 写二进制文件，上限 10MB。
fn handle_write_base64(state: &ServerState, data: &Value) -> HttpResponse {
    let cfg = state.config();
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let b64 = match data.get("content_base64").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return HttpResponse::err(
                400,
                "Bad Request",
                "需要 root / path / content_base64",
            )
        }
    };
    let bytes = match base64_decode(&b64) {
        Ok(b) => b,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if bytes.len() > MAX_BASE64_BYTES {
        return HttpResponse::err(413, "Payload Too Large", "内容超过 10 MB");
    }
    if rel.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 path");
    }
    let target = match cfg.resolve_inside(&root, &rel) {
        Ok(p) => p,
        Err(e) => return HttpResponse::err(400, "Bad Request", &e),
    };
    if is_sensitive_target(&target) {
        return HttpResponse::err(403, "Forbidden", &format!("该路径禁止远程修改：{rel}"));
    }
    if target.is_dir() {
        return HttpResponse::err(400, "Bad Request", &format!("目标是目录，不能写：{rel}"));
    }
    if let Err(r) = guard_write(state, &cfg, "write_base64", &rel, None) {
        return r;
    }
    // v2.3.3（P1 #11a）：审批窗口结束后重新解析路径。
    if let Err(r) = revalidate_target(&cfg, &root, &rel, &target) {
        return r;
    }
    let snap_id = take_snapshot(state, &root, &rel, "write_base64", None);
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return HttpResponse::err(403, "Forbidden", &format!("建父目录失败：{e}"));
        }
    }
    match std::fs::write(&target, &bytes) {
        Ok(()) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "root": root, "path": rel, "size": bytes.len(), "snapshot_id": snap_id}),
        ),
        Err(e) => HttpResponse::err(403, "Forbidden", &format!("无权限：{e}")),
    }
}

// ---------------------------------------------------------------------------
// /api/run 与 /ui/run 共用的执行入口
// ---------------------------------------------------------------------------

/// /api/run 的一次执行结果。
/// - code：200 成功；400 请求不对；403 不在白名单；408 超时；500 起不来。
/// - error：code 不是 200 时的人话原因。
/// - output：命令的退出码与两条流；超时时是超时前已产出的部分输出。
pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
}

pub struct RunOutcome {
    pub code: u16,
    pub error: Option<String>,
    pub output: RunOutput,
}

impl RunOutcome {
    fn fail(code: u16, error: String) -> Self {
        Self {
            code,
            error: Some(error),
            output: RunOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                truncated: false,
            },
        }
    }
}

/// 持续读一个管道直到 EOF；超过 cap 后只截断、继续排空，
/// 这样子进程写满管道时不会被堵住。
fn read_capped<R: Read>(mut r: R, cap: usize) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if truncated {
                    continue;
                }
                if buf.len() + n > cap {
                    let room = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

/// 执行入口（HTTP 薄壳 run_command 与 /ui/run 共用）。
/// 注意：这里只执行，鉴权（白名单 / 审批表 / 参数消毒）由 run_command 先做——
/// v0.7 时白名单检查在 run_operation 里，v0.8 挪出去了，
/// 因为审批通过的命令（如 git push）本来就不在 run_allowlist 里。
/// - argv 是字符串数组，例 ["cargo", "test"]；
/// - 直接 Command 调进程，不经过 shell：`&&`、`|`、`>` 只会变成
///   传给命令的字面参数，不会执行；cmd / powershell 不在白名单里；
/// - cwd 是 path 在 root 内的目录（默认 root 本身），用同一套
///   resolve_inside 锁死，`..` 逃逸同样 400；
/// - 超时（默认 300 秒，上限 1800 秒）后杀直接子进程，返回 408 +
///   已产出的部分输出；Windows 下子进程进专属 Job Object，
///   超时 / 退出时孙进程一并被系统清理（根治 os error 5 文件锁）；
/// - 命令本身退出码非 0 时仍返回 200，调用方看 exit_code 判断。
pub fn run_operation(
    cfg: &BridgeConfig,
    root: &str,
    rel: &str,
    argv: &[String],
    timeout_secs: u64,
) -> RunOutcome {
    if argv.is_empty() {
        return RunOutcome::fail(
            400,
            "需要 argv（字符串数组），例如 [\"cargo\", \"test\"]".to_string(),
        );
    }
    let cwd = match cfg.resolve_inside(root, rel) {
        Ok(p) => p,
        Err(e) => return RunOutcome::fail(400, e),
    };
    if !cwd.is_dir() {
        return RunOutcome::fail(400, "cwd 不是已存在的目录".to_string());
    }
    let timeout_secs = timeout_secs.clamp(1, MAX_RUN_TIMEOUT_SECS);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 中文 Windows 子进程默认按系统 CP936 (GBK) 输出，Python 与命令行
        // 工具的中文输出进 JSON 会乱码；显式注入 UTF-8 环境变量，强制按
        // UTF-8 输出。Linux 下同样无害（LANG=C.UTF-8 本就是常见默认）。
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .env("LANG", "C.UTF-8");
    // v1.0（附加 A）：Windows 下子进程不弹黑框。
    // 主程序设了 windows_subsystem 后，Command 默认会给每个子进程
    // 临时建一个控制台窗口闪一下；CREATE_NO_WINDOW（0x08000000）让它全程静默。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::fail(500, format!("启动命令失败：{e}")),
    };
    // v0.8 漏洞 3：Windows 下把子进程放进一个专属 Job Object；
    // 守卫活到 run_operation 返回，退出时系统把 Job 里残留的进程一锅端。
    #[cfg(windows)]
    let _job_guard = crate::job::JobGuard::new().map(|j| {
        j.assign(&child);
        j
    });
    let out_pipe = match child.stdout.take() {
        Some(p) => p,
        None => return RunOutcome::fail(500, "拿不到 stdout 管道".to_string()),
    };
    let err_pipe = match child.stderr.take() {
        Some(p) => p,
        None => return RunOutcome::fail(500, "拿不到 stderr 管道".to_string()),
    };
    let out_handle = thread::spawn(move || read_capped(out_pipe, MAX_RUN_OUTPUT_BYTES));
    let err_handle = thread::spawn(move || read_capped(err_pipe, MAX_RUN_OUTPUT_BYTES));

    let start = Instant::now();
    let mut timed_out = false;
    let mut exit_code: Option<i32> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if start.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    let (stdout, out_truncated) = out_handle.join().unwrap_or_default();
    let (stderr, err_truncated) = err_handle.join().unwrap_or_default();
    let truncated = out_truncated || err_truncated;
    if timed_out {
        return RunOutcome {
            code: 408,
            error: Some(format!("命令超时（{timeout_secs} 秒），已终止直接子进程")),
            output: RunOutput {
                exit_code,
                stdout,
                stderr,
                timed_out: true,
                truncated,
            },
        };
    }
    RunOutcome {
        code: 200,
        error: None,
        output: RunOutput {
            exit_code,
            stdout,
            stderr,
            timed_out: false,
            truncated,
        },
    }
}

/// v2.2.0：跑命令的统一鉴权门控（/api/run、/ui/run、/api/tasks/start、
/// /api/program 改后即跑共用）。从 run_command 原样抽出，行为不变：
/// - shell 解释器三档都禁（永久规则，full_access 也不例外）；
/// - ask_always：shell 直接 403；其余所有命令先消毒、再挂起审批（本地点按除外）；
/// - smart_auto：白名单自动放行，审批表挂起审批，都没中就 403；
/// - full_access：白名单 / 审批表 / 白名单外的其他开发命令全部直接跑，
///   不查表、不消毒、不审批；只有 shell（cmd / powershell）仍 403。
/// 通过后返回当前配置快照（调用方拿去 resolve_inside / spawn）。
///
/// v2.3.3：
/// - `env_detail`：请求带来的额外环境变量摘要，进审批详情。旧版审批弹窗只显示
///   `argv.join(" ")`，`{"env":{"RUSTC_WRAPPER":...}}` 这类载荷在人眼里完全不可见，
///   等于批准一个看不见内容的命令（报告 P1 #8 的一部分）；
/// - 403 不再回显完整 `run_allowlist` / `run_asklist`（报告 P2：未授权请求即可
///   枚举内部放行规则，方便构造后续载荷）。
fn authorize_run(
    state: &ServerState,
    root: &str,
    rel: &str,
    argv: &[String],
    local: bool,
    env_detail: Option<&str>,
) -> Result<BridgeConfig, HttpResponse> {
    if argv.is_empty() {
        return Err(HttpResponse::err(400, "Bad Request", "argv 不能为空"));
    }
    if is_shell_command(&argv[0]) {
        return Err(HttpResponse::err(
            403,
            "Forbidden",
            &format!(
                "shell 命令永不放行：{}（cmd / powershell 放行等于绕过白名单）",
                argv[0]
            ),
        ));
    }
    let mut detail = argv.join(" ");
    if let Some(env) = env_detail {
        if !env.is_empty() {
            detail.push_str("  [env: ");
            detail.push_str(env);
            detail.push(']');
        }
    }
    let cfg = state.config();
    match cfg.permission_mode {
        PermissionMode::FullAccess => {
            // 彻底放开：不查白名单 / 审批表，不做参数消毒，不挂起审批。
            // cwd 仍锁在白名单根目录内（run_operation 里 resolve_inside）。
        }
        PermissionMode::AskAlways => {
            // 参数级消毒先过（危险参数直接 403，不惊动用户去审批）。
            if let Err(e) = validate_argv_safety(&cfg, root, rel, argv) {
                return Err(HttpResponse::err(403, "Forbidden", &e));
            }
            // 本地点按本身就是确认；远程调用一律挂起审批。
            if !local {
                if let Err(r) = approval_gate(state, "run", &detail) {
                    return Err(r);
                }
            }
        }
        PermissionMode::SmartAuto => {
            // 先过白名单（自动放行），没中再过审批表；都没中就 403。
            // 白名单优先：python --version 在白名单里有更具体的条目，
            // 不会被审批表里裸 ["python"] 拦下来。
            let in_allow = cfg
                .run_allowlist
                .iter()
                .any(|p| argv_starts_with(argv, p));
            let in_ask = cfg
                .run_asklist
                .iter()
                .any(|p| argv_starts_with(argv, p));
            if !in_allow && !in_ask {
                return Err(HttpResponse::err(
                    403,
                    "Forbidden",
                    &format!(
                        "命令不在白名单里：{}（放行 / 审批规则在设置面板查看，响应里不回显）",
                        argv.join(" ")
                    ),
                ));
            }
            // 参数级消毒（白名单与审批表都过这一关）。
            if let Err(e) = validate_argv_safety(&cfg, root, rel, argv) {
                return Err(HttpResponse::err(403, "Forbidden", &e));
            }
            if in_ask && !in_allow && !local {
                if let Err(r) = approval_gate(state, "run", &detail) {
                    return Err(r);
                }
            }
        }
    }
    Ok(cfg)
}

/// POST /api/run 与 POST /ui/run 共用的 HTTP 薄壳。
/// local=true 是本机面板点按：点按本身就是确认，不挂起审批；
/// local=false 是远程调用：按三档权限级别决定是否挂起审批。
fn run_command(state: &ServerState, data: &Value, local: bool) -> HttpResponse {
    let root = data
        .get("root")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rel = data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .to_string();
    let argv: Vec<String> = match data.get("argv") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    let timeout_secs = data
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RUN_TIMEOUT_SECS)
        .clamp(1, MAX_RUN_TIMEOUT_SECS);
    let cfg = match authorize_run(state, &root, &rel, &argv, local, None) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let outcome = run_operation(&cfg, &root, &rel, &argv, timeout_secs);
    // v2.2.0：同步运行也更新最近运行摘要（GET /api/program 的 last_run）。
    if let Ok(mut lr) = state.last_run.lock() {
        *lr = Some(LastRun {
            argv: argv.clone(),
            exit_code: outcome.output.exit_code,
            timed_out: outcome.output.timed_out,
            at_ms: now_unix_secs() * 1000,
            task_id: None,
            async_run: false,
        });
    }
    match outcome.code {
        200 => HttpResponse::json(
            200,
            "OK",
            json!({
                "ok": true,
                "argv": argv,
                "exit_code": outcome.output.exit_code,
                "stdout": outcome.output.stdout,
                "stderr": outcome.output.stderr,
                "timed_out": outcome.output.timed_out,
                "truncated": outcome.output.truncated,
            }),
        ),
        408 => HttpResponse::json(
            408,
            "Request Timeout",
            json!({
                "ok": false,
                "error": outcome.error,
                "argv": argv,
                "stdout": outcome.output.stdout,
                "stderr": outcome.output.stderr,
                "timed_out": outcome.output.timed_out,
                "truncated": outcome.output.truncated,
            }),
        ),
        400 => HttpResponse::err(400, "Bad Request", outcome.error.as_deref().unwrap_or("")),
        500 => HttpResponse::err(
            500,
            "Internal Server Error",
            outcome.error.as_deref().unwrap_or(""),
        ),
        _ => HttpResponse::err(
            500,
            "Internal Server Error",
            outcome.error.as_deref().unwrap_or(""),
        ),
    }
}

// ---------------------------------------------------------------------------
// v2.2.0 模块二：异步后台长任务（/api/tasks/*）
// ---------------------------------------------------------------------------

/// v2.3.3（审查报告 P1 #8）：判断一个环境变量名会不会让子进程「加载外部代码」
/// 或「把操作指到白名单外」。
///
/// 背景：`/api/tasks/start` 的 `env` 不参与鉴权、不进审批详情、也没有键名黑名单，
/// `{"argv":["cargo","build"],"env":{"RUSTC_WRAPPER":"C:\\evil\\w.exe"}}` 就能让
/// 白名单命令执行任意程序；`PYTHONPATH` / `GIT_DIR` / `GIT_CONFIG_*` 同理。
/// 这里按「工具级环境变量」黑名单拦（返回 true = 敏感，拒绝）。
///
/// 为什么用黑名单而不是白名单：白名单式放行（只许几个固定键）会把
/// `MY_DEBUG=1` 这类正常用法也挡掉，可用性损失太大；黑名单覆盖的是已知的
/// 代码加载 / 路径重定向 / 凭据注入面，配合「值长度上限 + 审批详情展示」
/// 已经把可控性收回到人这一侧。
fn env_key_is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    const DENY_EXACT: &[&str] = &[
        "PATH", "PATHEXT", "COMSPEC", "SYSTEMROOT", "WINDIR", "SHELL", "ENV", "PWD",
        "OLDPWD", "HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "TMPDIR",
        "CC", "CXX", "CPP", "LD", "AR", "AS", "RANLIB", "STRIP", "OBJCOPY", "MAKEFLAGS",
        "MFLAGS", "CLASSPATH", "NODE_OPTIONS", "NODE_PATH", "SSH_ASKPASS", "CARGO",
        "PYTHONIOENCODING", "PYTHONUTF8", "LANG", "LC_ALL", "GOPATH", "GOFLAGS", "GOCACHE",
        "GOTOOLCHAIN", "GO111MODULE", "GOENV", "DOTNET_STARTUP_HOOKS",
    ];
    const DENY_PREFIX: &[&str] = &[
        "RUSTC", "RUSTUP_", "RUSTDOC", "CARGO_", "PYTHON", "PYTHONHOME", "PY_", "GIT_",
        "NODE_", "NPM_", "YARN_", "PNPM_", "LD_", "DYLD_", "BASH", "ZSH", "PERL", "RUBY",
        "JAVA", "_JAVA", "CMAKE", "CONDA", "PIP_", "UV_", "POETRY", "MSBUILD", "VCPKG",
        "DOTNET_",
    ];
    if DENY_EXACT.contains(&k.as_str()) {
        return true;
    }
    if DENY_PREFIX.iter().any(|p| k.starts_with(p)) {
        return true;
    }
    k.ends_with("_PROXY")
        || k.contains("WRAPPER")
        || k.contains("PRELOAD")
        || k.contains("ASKPASS")
        || k.contains("TOOLEXEC")
}

/// v2.3.3：`/api/tasks/start` 的 env 校验。规则（全部满足才放行）：
/// - 最多 32 项；键非空、≤128 字符、只允许 `[A-Za-z0-9_]`；
/// - 键名不得命中敏感名单（`env_key_is_sensitive`），也不得覆盖本服务注入的
///   UTF-8 变量；
/// - 值 ≤1024 字符且不含 NUL。
fn validate_extra_env(
    env: &serde_json::Map<String, Value>,
) -> Result<Vec<(String, String)>, String> {
    if env.len() > 32 {
        return Err("env 最多 32 项".to_string());
    }
    let mut out: Vec<(String, String)> = Vec::with_capacity(env.len());
    for (k, v) in env {
        let Some(v) = v.as_str() else {
            return Err("env 的值必须是字符串".to_string());
        };
        if k.is_empty() || k.len() > 128 {
            return Err("env 的键不合法（非空、≤128 字符）".to_string());
        }
        if !k
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(format!(
                "env 的键只能由字母、数字、下划线组成：{k}（不接受 = / NUL / 空格）"
            ));
        }
        if env_key_is_sensitive(k) {
            return Err(format!(
                "env 键 {k} 属于敏感变量（可通过它加载外部代码或把操作指到白名单外），已拒绝"
            ));
        }
        if v.len() > 1024 {
            return Err("env 的值超过 1024 字符".to_string());
        }
        if v.contains('\0') {
            return Err("env 的值不能包含 NUL".to_string());
        }
        out.push((k.clone(), v.to_string()));
    }
    Ok(out)
}

/// POST /api/tasks/start — {"root","path","argv","env"}。
/// 复用 /api/run 的全套门控，后台线程启动 + 专属 Job Object，
/// 立即返回 200 {ok, task_id, pid}，不阻塞 HTTP 连接。
/// v2.2.1：并发上限由 tasks 引擎预占（MAX_CONCURRENT_TASKS）；
/// 超限 429 Too Many Requests（可退避重试），其余启动失败 500。
fn handle_tasks_start(state: &ServerState, data: &Value) -> HttpResponse {
    let root = data.get("root").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rel = data.get("path").and_then(|v| v.as_str()).unwrap_or(".").to_string();
    let argv: Vec<String> = match data.get("argv") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    // 可选额外环境变量（UTF-8 默认注入之后合并）；v2.3.3 起过键名黑名单。
    let extra_env: Vec<(String, String)> = match data.get("env") {
        Some(Value::Object(env)) => match validate_extra_env(env) {
            Ok(v) => v,
            Err(e) => return HttpResponse::err(400, "Bad Request", &e),
        },
        Some(_) => return HttpResponse::err(400, "Bad Request", "env 必须是对象"),
        None => Vec::new(),
    };
    // 环境变量摘要进审批详情：用户点「允许」时看得见 cargo 被塞了什么。
    let env_detail = if extra_env.is_empty() {
        None
    } else {
        let mut s = extra_env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        if s.chars().count() > 300 {
            s = s.chars().take(300).collect::<String>() + "…";
        }
        Some(s)
    };
    let cfg = match authorize_run(state, &root, &rel, &argv, false, env_detail.as_deref()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match crate::tasks::spawn_task(state, &cfg, &root, &rel, &argv, &extra_env) {
        Ok((task_id, pid)) => HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "task_id": task_id, "pid": pid}),
        ),
        // v2.2.1：并发预占失败 -> 429，文案带上限值，调用方退避重试。
        Err(crate::tasks::SpawnError::LimitReached) => HttpResponse::err(
            429,
            "Too Many Requests",
            &format!(
                "并发任务已达上限 {}，请等运行中的任务结束后再启动",
                crate::tasks::MAX_CONCURRENT_TASKS
            ),
        ),
        Err(crate::tasks::SpawnError::Failed(e)) => {
            HttpResponse::err(500, "Internal Server Error", &e)
        }
    }
}

/// GET /api/tasks/status?id=&offset=&stderr_offset=&tail_bytes=
/// — 实时状态 + 增量输出；v2.2.1 起带 tail_bytes 时改为返回缓冲末尾 N 字节。
fn handle_tasks_status(state: &ServerState, query: &HashMap<String, String>) -> HttpResponse {
    let id = query.get("id").map(|s| s.as_str()).unwrap_or("");
    if id.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 id");
    }
    let offset = query.get("offset").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let stderr_offset = query.get("stderr_offset").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    // v2.2.1：长任务日志动辄几 MB，前端每 2 秒拉增量刷不动；tail_bytes 只要末尾 N 字节。
    // clamp 到 1..=MAX_TASK_OUTPUT_BYTES：0 没意义，超过缓冲上限则按上限截。
    let tail_bytes = query
        .get("tail_bytes")
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(1, crate::tasks::MAX_TASK_OUTPUT_BYTES));
    match state.tasks.status(id, offset, stderr_offset, tail_bytes) {
        Some(v) => HttpResponse::json(200, "OK", v),
        None => HttpResponse::err(404, "Not Found", "没有这个任务（可能已被 64 条上限挤掉）"),
    }
}

/// POST /api/tasks/stop — {"id"}。关 Job 句柄，OS 连根清理进程树。
fn handle_tasks_stop(state: &ServerState, data: &Value) -> HttpResponse {
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 id");
    }
    if state.tasks.stop(id) {
        HttpResponse::json(200, "OK", json!({"ok": true, "id": id}))
    } else {
        HttpResponse::err(404, "Not Found", "没有这个任务（可能已被 64 条上限挤掉）")
    }
}

/// GET /api/tasks/list — 运行中全部 + 最近结束 20 条摘要。
fn handle_tasks_list(state: &ServerState) -> HttpResponse {
    HttpResponse::json(200, "OK", json!({"ok": true, "tasks": state.tasks.list()}))
}

// ---------------------------------------------------------------------------
// /ui/* 处理函数（仅本机 + 内存级密钥，见 dispatch_request）
// ---------------------------------------------------------------------------

fn handle_ui_state(state: &ServerState) -> HttpResponse {
    let cfg = state.config();
    HttpResponse::json(
        200,
        "OK",
        json!({
            "ok": true,
            "version": VERSION,
            "status": if state.kill_switch.load(Ordering::SeqCst) { "已熔断" } else { "运行中" },
            "address": cfg.bind_addr(),
            "roots": cfg.roots.iter().map(|(k, v)| format!("{k} -> {}", v.display())).collect::<Vec<_>>(),
            "roots_map": cfg.roots,
            "permission_mode": cfg.permission_mode,
            "network_mode": "tailscale_or_local",
            "kill_switch": state.kill_switch.load(Ordering::SeqCst),
            "token": cfg.token,
            "run_allowlist": cfg.run_allowlist,
            "run_asklist": cfg.run_asklist,
            "require_hmac": cfg.require_hmac,
            // v2.2.1：绑定的主程序单文件（未绑定为 null），设置弹窗回显用。
            "main_file": cfg.main_file,
            // v2.3.3：审批超时与队列上限下发给界面——倒计时不再硬编码 60 秒
            //（报告 P3：`ui/index.html` 与服务端 APPROVAL_TIMEOUT_SECS 未联动）。
            "approval_timeout_secs": APPROVAL_TIMEOUT_SECS,
            "max_pending_approvals": MAX_PENDING_APPROVALS,
            "uptime_secs": state.uptime_secs(),
        }),
    )
}

fn handle_ui_approve(state: &ServerState, data: &Value) -> HttpResponse {
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
    // v2.3.3（审查报告 P2）：非 bool 的 allow 不再静默按「拒绝」处理——
    // 旧实现 `unwrap_or(false)` 会把客户端的字段拼写错误变成一次真实的拒绝。
    let allow = match data.get("allow").and_then(|v| v.as_bool()) {
        Some(a) => a,
        None => {
            return HttpResponse::err(400, "Bad Request", "需要 allow（true / false）")
        }
    };
    if id.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 id");
    }
    if resolve_approval(state, id, allow) {
        HttpResponse::json(200, "OK", json!({"ok": true, "id": id, "allow": allow}))
    } else {
        HttpResponse::err(
            404,
            "Not Found",
            "该审批已不在队列（可能已超时或已被处理）",
        )
    }
}

fn handle_ui_activity(state: &ServerState) -> HttpResponse {
    let logs: Vec<Value> = match state.logs.lock() {
        Ok(g) => g
            .iter()
            .rev()
            .take(50)
            .map(|e| {
                json!({"date": e.date, "time": e.time, "msg": e.msg, "snapshot_id": e.snapshot_id})
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    HttpResponse::json(200, "OK", json!({"ok": true, "logs": logs}))
}

/// v1.0.3：操作热力图——从日志文本按天统计次数。
/// 只认新格式行首的日期（YYYY-MM-DD）；旧格式行（无日期）跳过，
/// 不硬算到今天——跨版本那几行不值得污染统计。
/// v2.2.0：收紧为完整时间戳前缀校验（log_timestamp_prefix），
/// 历史多行命令溢出的脏片段（只有日期、或纯文本续行）不再计入。
/// 纯函数，单测直接调。
pub fn count_heatmap_days(content: &str) -> BTreeMap<String, u64> {
    let mut days: BTreeMap<String, u64> = BTreeMap::new();
    for line in content.lines() {
        if log_timestamp_prefix(line) {
            *days.entry(line[..10].to_string()).or_insert(0) += 1;
        }
    }
    days
}

/// v1.0.3：GET /ui/heatmap——读落盘日志（+轮转后的 .old）按天统计，
/// 面板画 GitHub 风格热力图。只统计文件里的行：内存动态是文件的子集，
/// 两边都算会重复计；文件被清掉后热力图从新文件重新累计。
/// 读文件是纯统计，不记日志（/ui/* 轮询本就不记，见 log_activity 的过滤）。
fn handle_ui_heatmap(_state: &ServerState) -> HttpResponse {
    let mut days: BTreeMap<String, u64> = BTreeMap::new();
    let path = match LOG_FILE.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    if let Some(p) = path {
        for f in [p.clone(), PathBuf::from(format!("{}.old", p.display()))] {
            if let Ok(content) = std::fs::read_to_string(&f) {
                for (d, n) in count_heatmap_days(&content) {
                    *days.entry(d).or_insert(0) += n;
                }
            }
        }
    }
    let total: u64 = days.values().sum();
    HttpResponse::json(200, "OK", json!({"ok": true, "days": days, "total": total}))
}

/// v1.0（模块二）：列出快照（新在前）。
fn handle_ui_snapshots(state: &ServerState) -> HttpResponse {
    HttpResponse::json(
        200,
        "OK",
        json!({"ok": true, "snapshots": list_snapshots(state)}),
    )
}

/// v1.0（模块二）：按 id 回滚一张快照。
fn handle_ui_revert(state: &ServerState, data: &Value) -> HttpResponse {
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return HttpResponse::err(400, "Bad Request", "需要 id");
    }
    match revert_snapshot(state, id) {
        Ok(path) => HttpResponse::json(200, "OK", json!({"ok": true, "id": id, "path": path})),
        Err(e) => HttpResponse::err(404, "Not Found", &e),
    }
}

/// Web 面板保存配置：token / roots / 网络模式 / 权限级别 / HMAC 开关。
/// v0.9：保存后立即热重载——先写盘，再换掉实时配置并通知 NetworkManager，
/// 监听 / 隧道 / 反向连接在 0.5 秒内按新配置重建，返回 needs_restart=false。
/// 校验失败直接 400（此时实时配置与磁盘都不会动）。
fn handle_ui_save_config(state: &ServerState, data: &Value) -> HttpResponse {
    let mut cfg = state.config();
    if let Some(t) = data.get("token").and_then(|v| v.as_str()) {
        if t.is_empty() {
            return HttpResponse::err(400, "Bad Request", "token 不能为空");
        }
        // v2.3.3（审查报告 P2 高位）：token 强度下限。
        // 默认 token 是 128 bit 随机串，但用户能在这里改成 `1234`——那等于
        // 把整个服务交给在线爆破。12 字符起（低于这个长度熵不足以撑住限流窗口）。
        if t.chars().count() < MIN_TOKEN_LEN {
            return HttpResponse::err(
                400,
                "Bad Request",
                &format!("token 至少 {MIN_TOKEN_LEN} 个字符（建议 32 位随机串）"),
            );
        }
        if t.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return HttpResponse::err(400, "Bad Request", "token 不能包含空白或控制字符");
        }
        cfg.token = t.to_string();
    }
    if let Some(Value::Object(roots)) = data.get("roots") {
        if roots.is_empty() {
            return HttpResponse::err(400, "Bad Request", "至少要留一个 root 文件夹");
        }
        let mut new_roots = std::collections::BTreeMap::new();
        for (alias, p) in roots {
            let Some(p) = p.as_str() else {
                return HttpResponse::err(400, "Bad Request", "roots 的值必须是字符串路径");
            };
            let path = Path::new(p);
            if !path.is_absolute() {
                return HttpResponse::err(
                    400,
                    "Bad Request",
                    &format!("roots.{alias} 必须是绝对路径：{p}"),
                );
            }
            if !path.is_dir() {
                return HttpResponse::err(
                    400,
                    "Bad Request",
                    &format!("roots.{alias} 不是已存在的文件夹：{p}"),
                );
            }
            new_roots.insert(alias.clone(), path.to_path_buf());
        }
        cfg.roots = new_roots;
    }
    if let Some(p) = data.get("permission_mode").and_then(|v| v.as_str()) {
        match p {
            "ask_always" => cfg.permission_mode = PermissionMode::AskAlways,
            "smart_auto" => cfg.permission_mode = PermissionMode::SmartAuto,
            "full_access" => cfg.permission_mode = PermissionMode::FullAccess,
            _ => {
                return HttpResponse::err(
                    400,
                    "Bad Request",
                    "permission_mode 只能是 ask_always / smart_auto / full_access",
                )
            }
        }
    }
    if let Some(h) = data.get("require_hmac").and_then(|v| v.as_bool()) {
        cfg.require_hmac = h;
    }
    // v2.2.0：主程序单文件绑定（null / 空串 = 解绑；非空必须能解析）。
    if let Some(mf) = data.get("main_file") {
        let s = mf.as_str().unwrap_or("").trim().to_string();
        cfg.main_file = if s.is_empty() { None } else { Some(s) };
        if cfg.main_file.is_some() {
            // v2.3.3（审查报告 P1 #12）：绑定侧也要过读保护。
            // 旧实现只调 resolve_main_file()，把 main_file 指到 bridge-config.json
            // 是允许的——之后 GET /api/program 直接读出明文 token。
            match cfg.resolve_main_file() {
                Ok((_, rel, abs)) => {
                    if check_read_protected(&rel) || is_sensitive_target(&abs) {
                        cfg.main_file = None;
                        return HttpResponse::err(
                            400,
                            "Bad Request",
                            "主程序文件不能是敏感文件（bridge-config.json / 活动日志 / .git 内）",
                        );
                    }
                }
                Err(e) => {
                    return HttpResponse::err(
                        400,
                        "Bad Request",
                        &format!("主程序文件不对：{e}"),
                    )
                }
            }
        }
    }
    match BridgeConfig::save_config(&state.config_path, &cfg) {
        Ok(()) => {
            // 先落盘成功，再换实时配置并通知热切换；写盘失败则实时配置不动。
            state.update_config(cfg);
            push_log(&state.logs, "配置已保存并热重载生效，无需重启".to_string());
            HttpResponse::json(
                200,
                "OK",
                json!({"ok": true, "needs_restart": false, "applied": true}),
            )
        }
        Err(e) => HttpResponse::err(500, "Internal Server Error", &e),
    }
}

/// Web 面板点选文件夹：列出某绝对路径下的子目录。
/// path 为空时返回盘符（Windows）或根目录（Linux），供用户从顶层选起。
fn handle_ui_browse(query: &HashMap<String, String>) -> HttpResponse {
    let path = query.get("path").cloned().unwrap_or_default();
    if path.is_empty() {
        #[cfg(windows)]
        let drives: Vec<String> = ('C'..='Z')
            .filter(|c| Path::new(&format!("{c}:\\")).exists())
            .map(|c| format!("{c}:\\"))
            .collect();
        #[cfg(not(windows))]
        let drives: Vec<String> = vec!["/".to_string()];
        return HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "path": "", "parent": "", "dirs": [], "drives": drives}),
        );
    }
    let p = Path::new(&path);
    if !p.is_absolute() {
        return HttpResponse::err(400, "Bad Request", "path 必须是绝对路径");
    }
    if !p.is_dir() {
        return HttpResponse::err(400, "Bad Request", "不是文件夹");
    }
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                dirs.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    dirs.sort_by_key(|d| d.to_lowercase());
    let parent = p
        .parent()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    HttpResponse::json(
        200,
        "OK",
        json!({"ok": true, "path": path, "parent": parent, "dirs": dirs, "drives": []}),
    )
}

fn handle_ui_kill_switch(state: &ServerState, data: &Value) -> HttpResponse {
    let paused = match data.get("paused").and_then(|v| v.as_bool()) {
        Some(p) => p,
        None => {
            return HttpResponse::err(400, "Bad Request", "需要 paused（true / false）");
        }
    };
    state.kill_switch.store(paused, Ordering::SeqCst);
    push_log(
        &state.logs,
        if paused {
            "熔断已开启：云端写入已暂停".to_string()
        } else {
            "熔断已解除：云端写入恢复".to_string()
        },
    );
    HttpResponse::json(200, "OK", json!({"ok": true, "paused": paused}))
}

// ---------------------------------------------------------------------------
// v0.8：统一分发核心 dispatch_request
// ---------------------------------------------------------------------------

fn is_loopback(peer_ip: Option<IpAddr>) -> bool {
    peer_ip.map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// v0.9：统一分发核心。HTTP Listener、WebSocket 反向客户端、relay_poll 都调它。
/// - GET /：仅本机（127.0.0.1 / ::1），小 JSON 状态（v2.0 起网页面板已删）；
///   /ui/*：仅 Tauri 内部调用（`dispatch_request_internal`），HTTP Socket 一律 403
///   （v2.3.1：固定请求头 `X-Pylon-UI: 1` 挡不住本机脚本，HTTP 通道彻底关掉）；
///   预授权通道（WS / relay）永远不允许走本机接口；GET / 在 v2.0 改回小 JSON 状态（网页面板已删）；
/// - /health 与 /api/*：远程鉴权（X-Bridge-Token，或 require_hmac 时的 HMAC 签名）；
///   预授权通道跳过这一步——token 已在建连时由中继校验过（见 docs/cloud_ws_relay.py）；
/// - 熔断开启时，/api/write / mkdir / run / patch 一律 403（面板接口不受影响，才能解除熔断）。
pub fn dispatch_request(
    state: &ServerState,
    method: &str,
    full_path: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &[u8],
    peer_ip: Option<IpAddr>,
) -> HttpResponse {
    dispatch_inner(state, method, full_path, path, query, headers, body, peer_ip, false, false)
}

/// v2.3.1：Tauri 内部调用入口——`ui_request` 命令（`invoke('ui_request')`）专用。
/// 与 `dispatch_request` 唯一区别是 `internal=true`：只有这条通道能进 `/ui/*`，
/// HTTP Socket（8765）收到 `/ui/*` 一律 403，见 `dispatch_inner` 的门控。
/// `/api/*` 在此入口同样走 `check_remote_auth`——`ui_request` 已为它注入当前 token。
/// 仅 Windows（Tauri UI）与单测需要：Linux 生产构建里没有 Tauri 调用方，
/// 不编译它，避免 `dead_code` 警告。
#[cfg(any(windows, test))]
pub fn dispatch_request_internal(
    state: &ServerState,
    method: &str,
    full_path: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &[u8],
    peer_ip: Option<IpAddr>,
) -> HttpResponse {
    dispatch_inner(state, method, full_path, path, query, headers, body, peer_ip, false, true)
}

/// v0.9：预授权入口（原供 WebSocket 反向客户端 / relay_poll 长轮询用）。
/// v2.1.3：那两个调用方已删除，只剩单测在用，标为 cfg(test)。
/// 面板接口（/ 与 /ui/*）依然禁止，熔断依然生效。
#[cfg(test)]
pub fn dispatch_request_preauthed(
    state: &ServerState,
    method: &str,
    full_path: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
) -> HttpResponse {
    dispatch_inner(
        state,
        method,
        full_path,
        path,
        query,
        &HashMap::new(),
        body,
        None,
        true,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn dispatch_inner(
    state: &ServerState,
    method: &str,
    full_path: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &[u8],
    peer_ip: Option<IpAddr>,
    pre_authed: bool,
    internal: bool,
) -> HttpResponse {
    // 预授权通道不允许碰本机接口（/ 与 /ui/* 只认本机）。
    if pre_authed && (path == "/" || path.starts_with("/ui/")) {
        return HttpResponse::err(403, "Forbidden", "预授权通道不允许访问本机接口");
    }
    // v2.0：网页面板已删，GET / 改回小 JSON 状态（仅本机，与 /health 同形）。
    if path == "/" {
        if method != "GET" {
            return HttpResponse::err(405, "Method Not Allowed", "GET / 只支持 GET");
        }
        if !is_loopback(peer_ip) {
            return HttpResponse::err(
                403,
                "Forbidden",
                "状态接口只允许本机（127.0.0.1）访问",
            );
        }
        return HttpResponse::json(
            200,
            "OK",
            json!({"ok": true, "version": VERSION, "roots": state.config().roots.keys().collect::<Vec<_>>()}),
        );
    }
    if path == "/favicon.ico" {
        // 浏览器再来要 /favicon.ico 时直接 204，不记 404、不记日志
        //（网页面板已删，但旧书签/缓存仍可能请求它）。
        return HttpResponse {
            code: 204,
            reason: "No Content",
            content_type: "text/plain",
            body: Vec::new(),
        };
    }
    // /ui/*：仅 Tauri 内部调用（v2.3.1，见上方 internal 门控）。
    // 历史：v1.0.2 废除动态密钥后改认本机 IP + 固定请求头 `X-Pylon-UI: 1`；
    // 但固定头挡不住本机脚本——任何本地进程都能自己发这个头，
    // 所以 v2.3.1 把 HTTP 通道彻底关掉：只有 invoke 内部调用能进这里，
    // 下面的本机 + 请求头检查只是纵深防御（内部调用本来就满足这两项）。
    if let Some(rest) = path.strip_prefix("/ui/") {
        if !internal {
            return HttpResponse::err(
                403,
                "Forbidden",
                "面板接口仅 Tauri 内部调用，HTTP Socket 已关闭此通道",
            );
        }
        if !is_loopback(peer_ip) {
            return HttpResponse::err(403, "Forbidden", "面板接口只允许本机访问");
        }
        let ui_ok = headers
            .get("x-pylon-ui")
            .map(|s| s == "1")
            .unwrap_or(false);
        if !ui_ok {
            return HttpResponse::err(401, "Unauthorized", "缺 X-Pylon-UI: 1 请求头");
        }
        return match (method, rest) {
            ("GET", "state") => handle_ui_state(state),
            ("GET", "approvals") => HttpResponse::json(
                200,
                "OK",
                json!({"ok": true, "approvals": list_approvals(state)}),
            ),
            ("POST", "approve") => match parse_body(body) {
                Ok(data) => handle_ui_approve(state, &data),
                Err(r) => r,
            },
            ("POST", "run") => match parse_body(body) {
                Ok(data) => run_command(state, &data, true),
                Err(r) => r,
            },
            ("GET", "activity") => handle_ui_activity(state),
            ("GET", "heatmap") => handle_ui_heatmap(state),
            ("GET", "snapshots") => handle_ui_snapshots(state),
            ("POST", "revert") => match parse_body(body) {
                Ok(data) => handle_ui_revert(state, &data),
                Err(r) => r,
            },
            ("POST", "save-config") => match parse_body(body) {
                Ok(data) => handle_ui_save_config(state, &data),
                Err(r) => r,
            },
            ("GET", "browse") => handle_ui_browse(query),
            ("POST", "kill-switch") => match parse_body(body) {
                Ok(data) => handle_ui_kill_switch(state, &data),
                Err(r) => r,
            },
            // v2.2.0：面板的后台任务卡（本机操作，停止不查远程门控）。
            ("GET", "tasks") => handle_tasks_list(state),
            ("POST", "tasks/stop") => match parse_body(body) {
                Ok(data) => handle_tasks_stop(state, &data),
                Err(r) => r,
            },
            _ => HttpResponse::err(
                404,
                "Not Found",
                &format!("未知面板接口：{method} /ui/{rest}"),
            ),
        };
    }
    // /health 与 /api/*：远程鉴权（预授权通道跳过，建连时已校验）。
    if path == "/health" || path.starts_with("/api/") {
        if !pre_authed {
            // v2.3.3：把对端地址传进去——认证失败要按来源计数限流。
            if let Err(r) =
                check_remote_auth(state, headers, method, full_path, body, peer_ip)
            {
                return r;
            }
        }
        if state.kill_switch.load(Ordering::SeqCst)
            && matches!(
                (method, path),
                ("POST", "/api/write")
                    | ("POST", "/api/mkdir")
                    | ("POST", "/api/run")
                    | ("POST", "/api/patch")
                    | ("POST", "/api/program")
                    | ("POST", "/api/trash")
                    | ("POST", "/api/rename")
                    | ("POST", "/api/write_base64")
                    | ("POST", "/api/tasks/start")
                    | ("POST", "/api/tasks/stop")
            )
        {
            return HttpResponse::err(
                403,
                "Forbidden",
                "已熔断：云端写入已暂停，在面板或托盘恢复后重试",
            );
        }
        return match (method, path) {
            ("GET", "/health") => HttpResponse::json(
                200,
                "OK",
                json!({"ok": true, "version": VERSION, "roots": state.config().roots.keys().collect::<Vec<_>>()}),
            ),
            ("GET", "/api/list") => handle_list(state, query),
            ("GET", "/api/read") => handle_read(state, query),
            ("POST", "/api/write") => match parse_body(body) {
                Ok(data) => handle_write(state, &data),
                Err(r) => r,
            },
            ("POST", "/api/mkdir") => match parse_body(body) {
                Ok(data) => handle_mkdir(state, &data),
                Err(r) => r,
            },
            ("POST", "/api/run") => match parse_body(body) {
                Ok(data) => run_command(state, &data, false),
                Err(r) => r,
            },
            ("POST", "/api/patch") => match parse_body(body) {
                Ok(data) => handle_patch(state, &data),
                Err(r) => r,
            },
            ("GET", "/api/search") => handle_search(state, query),
            ("GET", "/api/changes") => handle_changes(state, query),
            ("GET", "/api/program") => handle_program_get(state),
            ("POST", "/api/program") => match parse_body(body) {
                Ok(data) => handle_program_post(state, &data),
                Err(r) => r,
            },
            ("POST", "/api/trash") => match parse_body(body) {
                Ok(data) => handle_trash(state, &data),
                Err(r) => r,
            },
            ("POST", "/api/rename") => match parse_body(body) {
                Ok(data) => handle_rename(state, &data),
                Err(r) => r,
            },
            ("GET", "/api/read_base64") => handle_read_base64(state, query),
            ("POST", "/api/write_base64") => match parse_body(body) {
                Ok(data) => handle_write_base64(state, &data),
                Err(r) => r,
            },
            // v2.2.0 模块二：异步后台长任务引擎。
            ("POST", "/api/tasks/start") => match parse_body(body) {
                Ok(data) => handle_tasks_start(state, &data),
                Err(r) => r,
            },
            ("GET", "/api/tasks/status") => handle_tasks_status(state, query),
            ("POST", "/api/tasks/stop") => match parse_body(body) {
                Ok(data) => handle_tasks_stop(state, &data),
                Err(r) => r,
            },
            ("GET", "/api/tasks/list") => handle_tasks_list(state),
            _ => HttpResponse::err(404, "Not Found", &format!("未知接口：{method} {path}")),
        };
    }
    HttpResponse::err(404, "Not Found", &format!("未知接口：{method} {path}"))
}

// ---------------------------------------------------------------------------
// 动态记录与 HTTP  plumbing
// ---------------------------------------------------------------------------

/// 界面「动态」卡要看的记录：只记有意义的动作（写文件、建文件夹、修补、跑命令），
/// 读文件 / 列目录 / health / 搜索的成功不记，免得刷屏；失败的请求都记，方便定位问题。
/// v0.9：WS / relay 循环也调它（pub(crate)），让反向通道的请求同样进动态。
pub(crate) fn log_activity(logs: &SharedLogs, method: &str, path: &str, body: &[u8], code: u16, resp: &Value) {
    // v1.0.2：/ui/* 的轮询（state / approvals / activity）不记日志——
    // 每 2 秒一轮能把 pylon-activity.log 刷满（2026-11-09：532 行里 484 行是 401）。
    // 例外：POST /ui/run 与 POST /ui/tasks/stop 是真实业务操作，照常记。
    if path.starts_with("/ui/")
        && !(method == "POST" && (path == "/ui/run" || path == "/ui/tasks/stop"))
    {
        return;
    }
    let rel_of = |body: &[u8]| -> String {
        serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| {
                v.get("path")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default()
    };
    let err_of = |resp: &Value| -> String {
        resp.get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let msg = match (method, path) {
        ("POST", "/api/write") => {
            let rel = rel_of(body);
            if code == 200 {
                let size = resp
                    .get("size")
                    .and_then(|v| v.as_u64())
                    .map(|s| format!("（{s} 字节）"))
                    .unwrap_or_default();
                format!("写入 {rel} -> 200{size}")
            } else {
                format!("写入 {rel} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/patch") => {
            let rel = rel_of(body);
            if code == 200 {
                let n = resp
                    .get("patches_applied")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                format!("修补 {rel} -> 200（{n} 处修改）")
            } else {
                format!("修补 {rel} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/program") => {
            // 主程序单文件：body 里没有 path（绑定在配置里），只说清动作。
            // snapshot_id 照样从响应里取，动态里挂回滚按钮。
            if code == 200 {
                let n = resp
                    .get("patches_applied")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let mut msg = if n > 0 {
                    format!("热修补主程序 -> 200（{n} 处修改）")
                } else {
                    "覆写主程序 -> 200".to_string()
                };
                // v2.2.0：改后即跑闭环——动态里带上运行结果。
                if let Some(tid) = resp.get("task_id").and_then(|v| v.as_str()) {
                    msg.push_str(&format!("，后台运行 {tid}"));
                } else if let Some(ec) = resp.pointer("/run/exit_code").and_then(|v| v.as_i64()) {
                    msg.push_str(&format!("，运行 exit {ec}"));
                }
                msg
            } else {
                format!("主程序写入 -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/write_base64") => {
            let rel = rel_of(body);
            if code == 200 {
                let size = resp
                    .get("size")
                    .and_then(|v| v.as_u64())
                    .map(|s| format!("（{s} 字节）"))
                    .unwrap_or_default();
                format!("写入（二进制） {rel} -> 200{size}")
            } else {
                format!("写入（二进制） {rel} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/trash") => {
            let rel = rel_of(body);
            if code == 200 {
                format!("移入回收站 {rel} -> 200")
            } else {
                format!("移入回收站 {rel} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/rename") => {
            let (from, to) = serde_json::from_slice::<Value>(body)
                .ok()
                .map(|v| {
                    (
                        v.get("from")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        v.get("to")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .unwrap_or_default();
            if code == 200 {
                format!("重命名 {from} -> {to} -> 200")
            } else {
                format!("重命名 {from} -> {to} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/mkdir") => format!("新建文件夹 {} -> {code}", rel_of(body)),
        ("POST", p) if p == "/api/run" || p == "/ui/run" => {
            let argv = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("argv").and_then(|a| a.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                })
                .unwrap_or_default();
            if code == 200 {
                let ec = resp
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".to_string());
                format!("运行 {argv} -> exit {ec}")
            } else if code == 408 {
                format!("运行 {argv} -> 超时")
            } else {
                format!("运行 {argv} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/tasks/start") => {
            // v2.2.0：后台任务启动也进动态（argv 从 body 取，task_id 从响应取）。
            let argv = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("argv").and_then(|a| a.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                })
                .unwrap_or_default();
            if code == 200 {
                let tid = resp.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                format!("后台任务 {argv} -> {tid}")
            } else {
                format!("后台任务 {argv} -> {code} {}", err_of(resp))
            }
        }
        ("POST", "/api/tasks/stop") | ("POST", "/ui/tasks/stop") => {
            let id = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            format!("停止后台任务 {id} -> {code}")
        }
        _ => {
            if code >= 400 {
                format!("{method} {path} -> {code}")
            } else {
                return; // health / list / read / search 的成功不记
            }
        }
    };
    // v1.0（模块二）：写 / 修补 / 回收站等成功响应里带 snapshot_id，
    // 动态这条就挂上回滚按钮。
    let snapshot_id = resp
        .get("snapshot_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    push_log_snapshot(logs, msg, snapshot_id);
}

fn send_response(stream: &mut TcpStream, resp: &HttpResponse) {
    // v1.0.1：Cache-Control: no-store——面板 HTML 不进浏览器缓存，
    // 每次打开都拿最新版（v1.0.2 起不再有动态密钥，但 no-store 照样留着）。
    // v2.3.3：补 X-Content-Type-Options: nosniff（JSON 响应不允许被当 HTML 嗅探）。
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        resp.code,
        resp.reason,
        resp.content_type,
        resp.body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&resp.body);
    let _ = stream.flush();
}

/// 解析 `a=b&c=d` 这种 query，值做 URL 解码（%XX / +）。
/// v2.2.0：pub(crate)——Tauri UI 命令桥（tauri_ui.rs）也用它解析 query。
pub(crate) fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// v2.3.3：连接并发闸门（RAII）。超过上限直接 503 + 关连接，不 spawn 线程。
static LIVE_CONNECTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

struct ConnectionSlot;

impl ConnectionSlot {
    fn acquire() -> Option<Self> {
        let prev = LIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        if prev >= MAX_CONCURRENT_CONNECTIONS {
            LIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(ConnectionSlot)
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        LIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// v2.3.3：带长度上限的 `read_line`。
/// 返回 Ok(None) = 对端已关闭；行超限直接 Err（调用方回 414 / 400 并断开），
/// 避免「不含 `\r\n` 的超长请求行」把内存吃光。
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let mut limited = reader.take(max as u64 + 1);
    let n = limited.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if n > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "行长度超限",
        ));
    }
    Ok(Some(line))
}

/// v0.9：HTTP 连接处理（network.rs 的监听循环调它，pub(crate)）。
///
/// v2.3.3（审查报告 P1 #1，全部补齐）：
/// - accept 后设读 / 写超时：半截请求不会再永久占住一条线程；
/// - 请求行、单条头、头总量、头条数都有硬上限；
/// - `Transfer-Encoding: chunked` 明确 411 拒绝（旧实现把 chunk 帧当原始 body，
///   HMAC 验签与 JSON 解析都会错位）；
/// - 整份请求有总时限（慢速滴灌）；
/// - 并发连接数有上限（超了 503）。
pub(crate) fn handle_client(mut stream: TcpStream, state: Arc<ServerState>) {
    let _slot = match ConnectionSlot::acquire() {
        Some(s) => s,
        None => {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            send_response(
                &mut stream,
                &HttpResponse::err(
                    503,
                    "Service Unavailable",
                    "并发连接数已达上限，请稍后重试",
                ),
            );
            return;
        }
    };
    let started = Instant::now();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(HTTP_READ_TIMEOUT_SECS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(HTTP_WRITE_TIMEOUT_SECS)));
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip());
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    // 读请求行 + 请求头，直到空行
    let request_line = match read_line_limited(&mut reader, MAX_REQUEST_LINE_BYTES) {
        Ok(Some(l)) => l,
        Ok(None) => return,
        Err(_) => {
            send_response(
                &mut stream,
                &HttpResponse::err(414, "URI Too Long", "请求行过长"),
            );
            return;
        }
    };
    if request_line.trim().is_empty() {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0].to_string();
    let full_path = parts[1].to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut content_length: usize = 0;
    let mut header_total = 0usize;
    loop {
        if started.elapsed().as_secs() > HTTP_REQUEST_DEADLINE_SECS {
            send_response(
                &mut stream,
                &HttpResponse::err(408, "Request Timeout", "读取请求超时，已断开"),
            );
            return;
        }
        if headers.len() >= MAX_HEADER_COUNT {
            send_response(
                &mut stream,
                &HttpResponse::err(431, "Request Header Fields Too Large", "请求头过多"),
            );
            return;
        }
        let line = match read_line_limited(&mut reader, MAX_HEADER_LINE_BYTES) {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(_) => {
                send_response(
                    &mut stream,
                    &HttpResponse::err(
                        431,
                        "Request Header Fields Too Large",
                        "请求头单行过长",
                    ),
                );
                return;
            }
        };
        header_total += line.len();
        if header_total > MAX_HEADER_TOTAL_BYTES {
            send_response(
                &mut stream,
                &HttpResponse::err(
                    431,
                    "Request Header Fields Too Large",
                    "请求头总长度超限",
                ),
            );
            return;
        }
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = match val.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        send_response(
                            &mut stream,
                            &HttpResponse::err(400, "Bad Request", "Content-Length 不是数字"),
                        );
                        return;
                    }
                };
            }
            headers.insert(key, val);
        }
    }

    // v2.3.3：不解析 chunked —— 明确拒绝，而不是把 chunk 帧当 body 继续。
    if headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        send_response(
            &mut stream,
            &HttpResponse::err(
                411,
                "Length Required",
                "不支持 Transfer-Encoding: chunked，请改用 Content-Length",
            ),
        );
        return;
    }

    // v2.3.3：按需增长，不再一次预分配 `content_length` 字节——
    // 64 条连接同时声明 2MB body 时，预分配会先吃掉 128MB 再开始读。
    let mut body: Vec<u8> = Vec::new();
    if content_length > 0 {
        if content_length > MAX_TEXT_BYTES + 65536 {
            send_response(
                &mut stream,
                &HttpResponse::err(413, "Payload Too Large", "body 过大（上限约 2 MB）"),
            );
            return;
        }
        // v2.3.3：curl 等在 body 较大时会先发 `Expect: 100-continue` 再等回应。
        // 不回 100 的话客户端要空等约 1 秒才继续发，大文件上传会明显变慢。
        if headers
            .get("expect")
            .map(|v| v.to_ascii_lowercase().contains("100-continue"))
            .unwrap_or(false)
        {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = stream.flush();
        }
        body.reserve(content_length.min(64 * 1024));
        // v2.3.3：body 读取也受整请求总时限约束（慢速滴灌不能靠"每读一次都在
        // 超时以内"绕过去）。把本次读的 socket 超时压到剩余预算，读完整份再核一次。
        let remaining = HTTP_REQUEST_DEADLINE_SECS.saturating_sub(started.elapsed().as_secs());
        if remaining == 0 {
            send_response(
                &mut stream,
                &HttpResponse::err(408, "Request Timeout", "读取请求超时，已断开"),
            );
            return;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(
            remaining.min(HTTP_READ_TIMEOUT_SECS).max(1),
        )));
        let mut limited = reader.take(content_length as u64);
        if limited.read_to_end(&mut body).is_err() {
            return;
        }
        if started.elapsed().as_secs() > HTTP_REQUEST_DEADLINE_SECS {
            send_response(
                &mut stream,
                &HttpResponse::err(408, "Request Timeout", "读取请求超时，已断开"),
            );
            return;
        }
        // 对端没发完就断开：body 不完整，直接放弃（不发响应，免得半截 JSON 被当成请求）。
        if body.len() != content_length {
            return;
        }
    }

    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (full_path.clone(), HashMap::new()),
    };

    let resp = dispatch_request(
        &state,
        &method,
        &full_path,
        &path,
        &query,
        &headers,
        &body,
        peer_ip,
    );
    // 只记 JSON 响应的动态（HTML 面板本身不记）。
    if resp.content_type.starts_with("application/json") {
        if let Ok(v) = serde_json::from_slice::<Value>(&resp.body) {
            log_activity(&state.logs, &method, &path, &body, resp.code, &v);
        }
    }
    send_response(&mut stream, &resp);
}



#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn test_state(dir: &Path) -> ServerState {
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        ServerState::new(cfg, dir.join("bridge-config.json"), logs)
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn log_activity_records_write_and_run() {
        // 动态只记写 / 跑，health 的成功不记；每条前面带 HH:MM:SS。
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "src/main.rs", "content": "x"}))
                .unwrap();
        log_activity(
            &logs,
            "POST",
            "/api/write",
            &body,
            200,
            &json!({"ok": true, "size": 1}),
        );
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": ".", "argv": ["cargo", "check"]}),
        )
        .unwrap();
        log_activity(
            &logs,
            "POST",
            "/api/run",
            &body,
            200,
            &json!({"ok": true, "exit_code": 0}),
        );
        log_activity(&logs, "GET", "/health", &[], 200, &json!({"ok": true}));
        let g = logs.lock().unwrap();
        assert_eq!(g.len(), 2, "health 成功不应记，实际：{g:?}");
        assert!(g[0].msg.contains("写入 src/main.rs -> 200"), "实际：{}", g[0].msg);
        assert!(g[1].msg.contains("运行 cargo check -> exit 0"), "实际：{}", g[1].msg);
        // 时间戳在前：HH:MM:SS（第 2、5 位是冒号）。
        assert_eq!(&g[0].time[2..3], ":");
        assert_eq!(&g[0].time[5..6], ":");
    }

    // ---- 原语 1：patch ----

    #[test]
    fn patch_lf_file() {
        let out = apply_patches(
            "a = 1\nb = 2\n",
            &[("a = 1".to_string(), "a = 10".to_string())],
        )
        .unwrap();
        assert_eq!(out.0, "a = 10\nb = 2\n");
        assert_eq!(out.1, 1);
    }

    #[test]
    fn patch_crlf_file_keeps_crlf() {
        // Windows 文件是 CRLF：search 用 LF 也能匹配，写回仍是 CRLF。
        let out = apply_patches(
            "a = 1\r\nb = 2\r\n",
            &[("a = 1".to_string(), "a = 10".to_string())],
        )
        .unwrap();
        assert_eq!(out.0, "a = 10\r\nb = 2\r\n");
        assert!(!out.0.contains("\n\n") || out.0.contains("\r\n"));
    }

    #[test]
    fn patch_not_found() {
        let err = apply_patches(
            "a = 1\n",
            &[("zzz".to_string(), "y".to_string())],
        )
        .unwrap_err();
        assert_eq!(err, PatchError::NotFound);
    }

    #[test]
    fn patch_ambiguous() {
        // 同一行出现两次，必须 409，不能猜改哪一个。
        let err = apply_patches(
            "x = 1\nx = 1\n",
            &[("x = 1".to_string(), "x = 2".to_string())],
        )
        .unwrap_err();
        assert_eq!(err, PatchError::Ambiguous);
    }

    #[test]
    fn patch_multiple_sequential() {
        // 多条 patch 按序在内存里应用：第二条搜的是第一条改完后的文本。
        let out = apply_patches(
            "a = 1\nb = 2\n",
            &[
                ("a = 1".to_string(), "a = 10".to_string()),
                ("a = 10".to_string(), "a = 99".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(out.0, "a = 99\nb = 2\n");
        assert_eq!(out.1, 2);
    }

    // ---- v2.1.2：/api/program（文件即是程序）----

    fn program_state(dir: &Path) -> ServerState {
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.permission_mode = PermissionMode::FullAccess;
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        cfg.main_file = Some("app.html".to_string());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        ServerState::new(cfg, dir.join("bridge-config.json"), logs)
    }

    #[test]
    fn program_get_reads_bound_file() {
        let dir = test_dir("pylon-program-get-test");
        std::fs::write(dir.join("app.html"), "<h1>hi</h1>\n").unwrap();
        let state = program_state(&dir);
        let r = dispatch_request_preauthed(
            &state,
            "GET",
            "/api/program",
            "/api/program",
            &HashMap::new(),
            &[],
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["path"], "app.html");
        assert_eq!(v["content"], "<h1>hi</h1>\n");
        assert!(v["mtime_ms"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_get_unbound_is_400() {
        let dir = test_dir("pylon-program-unbound-test");
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        // main_file 留空（未绑定）。
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState::new(cfg, dir.join("bridge-config.json"), logs);
        let r = dispatch_request_preauthed(
            &state,
            "GET",
            "/api/program",
            "/api/program",
            &HashMap::new(),
            &[],
        );
        assert_eq!(r.code, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_post_overwrite_and_line_patches() {
        let dir = test_dir("pylon-program-post-test");
        std::fs::write(dir.join("app.html"), "line1\nline2\nline3\n").unwrap();
        let state = program_state(&dir);
        // 整文件覆写。
        let body = serde_json::to_vec(&json!({"content": "A\nB\nC\n"})).unwrap();
        let r = dispatch_request_preauthed(
            &state,
            "POST",
            "/api/program",
            "/api/program",
            &HashMap::new(),
            &body,
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["snapshot_id"].as_str().is_some(), "响应应带 snapshot_id（可回滚）");
        assert_eq!(std::fs::read_to_string(dir.join("app.html")).unwrap(), "A\nB\nC\n");
        // 行级热修补：替换第 2 行 + 在第 1 行前插入 + 删第 3 行
        // （行号指原始文件，倒序应用，互不干扰）。
        let body = serde_json::to_vec(&json!({"patches": [
            {"line": 2, "content": "B2"},
            {"line": 1, "op": "insert", "content": "HEAD"},
            {"line": 3, "op": "delete"},
        ]}))
        .unwrap();
        let r = dispatch_request_preauthed(
            &state,
            "POST",
            "/api/program",
            "/api/program",
            &HashMap::new(),
            &body,
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["patches_applied"], 3);
        // 原始 A/B/C：插 HEAD 到第 1 行前 -> HEAD,A,B2（C 被删）。
        assert_eq!(
            std::fs::read_to_string(dir.join("app.html")).unwrap(),
            "HEAD\nA\nB2\n"
        );
        // 动态里应有“热修补主程序”记录且带 snapshot_id（回滚按钮的来源）。
        // dispatch_request_preauthed 本身不记日志（HTTP 层 / WS 帧处理层
        // 收到响应后调 log_activity），这里按那两层的样子补调一次。
        let resp_v: Value = serde_json::from_slice(&r.body).unwrap();
        log_activity(&state.logs, "POST", "/api/program", &body, r.code, &resp_v);
        let g = state.logs.lock().unwrap();
        let last = g.last().unwrap();
        assert!(last.msg.contains("热修补主程序"), "实际：{}", last.msg);
        assert!(last.snapshot_id.is_some(), "动态应挂 snapshot_id");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_line_patches_units() {
        // replace / insert / delete + 末尾换行符保持 + 越界报错。
        let (out, n) = apply_line_patches(
            "a\nb\nc\n",
            &[json!({"line": 2, "content": "B"})],
        )
        .unwrap();
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(n, 1);
        // 无末尾换行：修补后也不加。
        let (out, _) = apply_line_patches(
            "a\nb",
            &[json!({"line": 1, "content": "A"})],
        )
        .unwrap();
        assert_eq!(out, "A\nb");
        // insert 到末尾（行号 = 总行数 + 1）。
        let (out, _) = apply_line_patches(
            "a\nb\n",
            &[json!({"line": 3, "op": "insert", "content": "c"})],
        )
        .unwrap();
        assert_eq!(out, "a\nb\nc\n");
        // delete 全部行 -> 空文件。
        let (out, _) = apply_line_patches(
            "a\n",
            &[json!({"line": 1, "op": "delete"})],
        )
        .unwrap();
        assert_eq!(out, "");
        // 越界 / 行号 0 / 非法 op 都报错。
        assert!(apply_line_patches("a\n", &[json!({"line": 2, "content": "x"})]).is_err());
        assert!(apply_line_patches("a\n", &[json!({"line": 0, "content": "x"})]).is_err());
        assert!(apply_line_patches("a\n", &[json!({"line": 1, "op": "move"})]).is_err());
        assert!(apply_line_patches("a\n", &[json!({"line": 1})]).is_err());
    }

    // ---- v2.1.2：/api/changes（增量变更感知）----

    #[test]
    fn changes_since_ms_lists_recent_files() {
        let dir = test_dir("pylon-changes-test");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        // .git / target / node_modules / .pylon_trash 跳过。
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/config"), "x").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        let state = test_state_full(&dir);
        // since_ms = 0：全量（除跳过的目录外）。
        let mut q = HashMap::new();
        q.insert("root".to_string(), "work".to_string());
        q.insert("since_ms".to_string(), "0".to_string());
        let r = dispatch_request_preauthed(&state, "GET", "/api/changes", "/api/changes", &q, &[]);
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let paths: Vec<&str> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"a.txt"), "实际：{paths:?}");
        assert!(paths.contains(&"sub/b.txt"), "实际：{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with(".git")), "实际：{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("target")), "实际：{paths:?}");
        // since_ms = 未来：一条都没有。
        let mut q2 = HashMap::new();
        q2.insert("root".to_string(), "work".to_string());
        q2.insert(
            "since_ms".to_string(),
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                + 60_000)
            .to_string(),
        );
        let r2 = dispatch_request_preauthed(&state, "GET", "/api/changes", "/api/changes", &q2, &[]);
        let v2: Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(v2["count"], 0);
        // 缺 since_ms -> 400。
        let mut q3 = HashMap::new();
        q3.insert("root".to_string(), "work".to_string());
        let r3 = dispatch_request_preauthed(&state, "GET", "/api/changes", "/api/changes", &q3, &[]);
        assert_eq!(r3.code, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 漏洞 1：写保护 ----

    #[test]
    fn write_protection_table() {
        assert_eq!(check_write_protected("build.rs"), WriteProtection::NeedsApproval);
        assert_eq!(
            check_write_protected(".cargo/config.toml"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("sub/build.rs"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected(".git/config"),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("sub/.git/objects/x"),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("local-bridge-rust/bridge-config.json"),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("tools/setup.bat"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("tools/run.PS1"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("bin/app.exe"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("src/main.rs"),
            WriteProtection::Allowed
        );
        assert_eq!(check_write_protected("notes.md"), WriteProtection::Allowed);
        // 大小写 / 尾部点号 / 尾部空格绕过（Windows 会静默剥掉段尾的 '.' 与 ' '，
        // 且不区分大小写；不归一化就会放行——NTFS 上它们与原文件是同一个）：
        assert_eq!(
            check_write_protected("BUILD.RS."),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("start.BAT "),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected(".GIT/config"),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected(".git./config"),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("bridge-config.json."),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("bridge-config.JSON "),
            WriteProtection::Forbidden
        );
        assert_eq!(
            check_write_protected("sub/.CARGO/x"),
            WriteProtection::NeedsApproval
        );
        assert_eq!(
            check_write_protected("a/b/c.DLL "),
            WriteProtection::NeedsApproval
        );
        // 正常文件带尾部点号也不应误伤（归一化后是普通名）。
        assert_eq!(check_write_protected("notes.md."), WriteProtection::Allowed);
    }

    // ---- 漏洞 2：argv 消毒 ----

    #[test]
    fn argv_safety_python() {
        let dir = test_dir("bridge-argv-py-test");
        let mut cfg = BridgeConfig::default();
        cfg.roots.insert("work".to_string(), dir.clone());
        std::fs::write(dir.join("script.py"), "print('hi')\n").unwrap();
        // python -c 直接禁。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["python".to_string(), "-c".to_string(), "x".to_string()]
        )
        .is_err());
        // python -m 也禁（除非整条恰好是 pip --version）。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["python".to_string(), "-m".to_string(), "os".to_string()]
        )
        .is_err());
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &[
                "python".to_string(),
                "-m".to_string(),
                "pip".to_string(),
                "--version".to_string()
            ]
        )
        .is_ok());
        // 正常脚本放行；不存在的脚本拦下来。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["python".to_string(), "script.py".to_string()]
        )
        .is_ok());
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["python".to_string(), "missing.py".to_string()]
        )
        .is_err());
        // 非 .py 文件拦下来。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["python".to_string(), "notes.txt".to_string()]
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn argv_safety_git() {
        let cfg = BridgeConfig::default();
        // 任务书点名的：git diff --output=... 必须拦。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &[
                "git".to_string(),
                "diff".to_string(),
                "--output=/tmp/x.patch".to_string()
            ]
        )
        .is_err());
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &[
                "git".to_string(),
                "-c".to_string(),
                "core.sshCommand=evil".to_string(),
                "status".to_string()
            ]
        )
        .is_err());
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &[
                "git".to_string(),
                "--upload-pack=evil".to_string(),
                "x".to_string()
            ]
        )
        .is_err());
        // 顺手补的加固：-C / --git-dir 能把 git 指到白名单外。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["git".to_string(), "-C".to_string(), "/etc".to_string(), "status".to_string()]
        )
        .is_err());
        // 正常命令放行。
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["git".to_string(), "status".to_string()]
        )
        .is_ok());
        assert!(validate_argv_safety(
            &cfg,
            "work",
            ".",
            &["git".to_string(), "log".to_string(), "--oneline".to_string(), "-20".to_string()]
        )
        .is_ok());
    }

    // ---- 原语 2：search ----

    #[test]
    fn search_skips_and_matches() {
        let dir = test_dir("bridge-search-test");
        std::fs::write(dir.join("a.rs"), "let hello = 1;\nlet bye = 2;\n").unwrap();
        std::fs::write(dir.join("b.txt"), "HELLO world\n").unwrap();
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("ignored.rs"), "hello\n").unwrap();
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("x.rs"), "hello\n").unwrap();
        // 二进制（非 UTF-8）跳过。
        std::fs::write(dir.join("bin.dat"), [0xff, 0xfe, b'h', b'e']).unwrap();
        // 大文件（>2MB）跳过。
        std::fs::write(dir.join("big.txt"), vec![b'a'; MAX_TEXT_BYTES + 1]).unwrap();

        let (hits, truncated) = search_files(&dir, "hello", false, false).unwrap();
        assert!(!truncated);
        // 只命中 a.rs 与 b.txt（大小写不敏感），.git / target / 二进制 / 大文件都跳过。
        assert_eq!(hits.len(), 2, "实际：{hits:?}");
        assert!(hits.iter().any(|h| h.file == "a.rs" && h.line == 1));
        assert!(hits.iter().any(|h| h.file == "b.txt" && h.line == 1));
        // 大小写敏感：只命中小写的。
        let (hits, _) = search_files(&dir, "hello", false, true).unwrap();
        assert_eq!(hits.len(), 1);
        // 正则：hello + 一个词。
        let (hits, _) = search_files(&dir, r"hello\s+\w+", true, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "b.txt");
        // 非法正则 400。
        assert!(search_files(&dir, "(", true, false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 原语 3：审批 ----

    #[test]
    fn approval_approve_flow() {
        let dir = test_dir("bridge-approval-test");
        let state = Arc::new(test_state(&dir));
        let state2 = Arc::clone(&state);
        // 200ms 后在另一个线程点允许。
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let id = {
                let q = state2.approvals.lock().unwrap();
                q.front().map(|r| r.id.clone())
            };
            if let Some(id) = id {
                assert!(resolve_approval(&state2, &id, true));
            }
        });
        let outcome = request_approval_timeout_with_diff(&state, "run", "git push", None, 5);
        assert_eq!(outcome, ApprovalOutcome::Approved);
        assert!(state.approvals.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_deny_flow() {
        let dir = test_dir("bridge-approval-deny-test");
        let state = Arc::new(test_state(&dir));
        let state2 = Arc::clone(&state);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let id = {
                let q = state2.approvals.lock().unwrap();
                q.front().map(|r| r.id.clone())
            };
            if let Some(id) = id {
                assert!(resolve_approval(&state2, &id, false));
            }
        });
        let outcome = request_approval_timeout_with_diff(&state, "run", "git push", None, 5);
        assert_eq!(outcome, ApprovalOutcome::Denied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_timeout_cleans_queue() {
        let dir = test_dir("bridge-approval-timeout-test");
        let state = test_state(&dir);
        let outcome = request_approval_timeout_with_diff(&state, "run", "git push", None, 1);
        assert_eq!(outcome, ApprovalOutcome::TimedOut);
        assert!(state.approvals.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- dispatch：面板隔离 / 鉴权 / 熔断 / HMAC ----

    #[test]
    fn dispatch_panel_loopback_only() {
        let dir = test_dir("bridge-dispatch-panel-test");
        let state = test_state(&dir);
        // 非本机 IP 访问 / 直接 403。
        let r = dispatch_request(
            &state,
            "GET",
            "/",
            "/",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        // 本机访问 / 拿到小 JSON 状态（v2.0：网页面板已删，不再是 HTML）。
        let r = dispatch_request(
            &state,
            "GET",
            "/",
            "/",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let body = String::from_utf8_lossy(&r.body).to_string();
        assert!(!body.contains("<html"), "GET / 不应再返回 HTML 页面");
        let v: serde_json::Value = serde_json::from_str(&body).expect("GET / 应返回 JSON");
        assert_eq!(v["ok"], true);
        assert_eq!(v["version"], VERSION);
        // v2.3.1：HTTP Socket 访问 /ui/* 一律 403——哪怕本机 + 固定头齐全
        //（第三方本机脚本再也不能偷调管理接口）；只有 Tauri 内部调用能进。
        let r = dispatch_request(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        let r = dispatch_request(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        let r = dispatch_request_internal(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_state_header_auth() {
        // v2.0：网页面板已删——没有页面可抓 header，固定头 X-Pylon-UI: 1
        // 直接写死在测试里（它就是常量，见 dispatch 的 /ui/* 分支）。
        let dir = test_dir("bridge-ui-auth-test");
        let state = test_state(&dir);
        let r = dispatch_request(
            &state,
            "GET",
            "/",
            "/",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let body = String::from_utf8_lossy(&r.body).to_string();
        assert!(!body.contains("<html"), "GET / 不应再返回 HTML 页面");
        // /favicon.ico 直接 204，不记 404。
        let r = dispatch_request(
            &state,
            "GET",
            "/favicon.ico",
            "/favicon.ico",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 204);
        // v2.3.1：面板接口只认 Tauri 内部调用——测试里用 dispatch_request_internal
        // 模拟面板；HTTP Socket（dispatch_request）哪怕本机 + 固定头齐全也 403。
        for path in ["/ui/state", "/ui/approvals", "/ui/activity", "/ui/heatmap"] {
            let r = dispatch_request_internal(
                &state,
                "GET",
                path,
                path,
                &HashMap::new(),
                &headers(&[("x-pylon-ui", "1")]),
                &[],
                Some("127.0.0.1".parse().unwrap()),
            );
            assert_eq!(r.code, 200, "{path} 应该 200");
            let r = dispatch_request(
                &state,
                "GET",
                path,
                path,
                &HashMap::new(),
                &headers(&[("x-pylon-ui", "1")]),
                &[],
                Some("127.0.0.1".parse().unwrap()),
            );
            assert_eq!(r.code, 403, "{path} 经 HTTP 应该 403");
        }
        // HTTP 没 header → 403（v2.3.1 起 /ui/* 的 HTTP 通道整个关掉，不再是 401）；
        // 非本机 → 403（哪怕 header 对）。
        let r = dispatch_request(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        let r = dispatch_request(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_state_reports_main_file() {
        // v2.2.1：/ui/state 必须回显 cfg.main_file（未绑定为 null，绑定为字符串），
        // 设置弹窗靠它回填主程序单文件。
        let dir = test_dir("bridge-ui-state-mainfile");
        let state = test_state(&dir);
        let r = dispatch_request_internal(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["main_file"].is_null(), "未绑定时应为 null：{}", v["main_file"]);
        let mut cfg = state.config();
        cfg.main_file = Some("app.html".to_string());
        state.update_config(cfg);
        let r = dispatch_request_internal(
            &state,
            "GET",
            "/ui/state",
            "/ui/state",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &[],
            Some("127.0.0.1".parse().unwrap()),
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["main_file"], "app.html");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_token_auth() {
        let dir = test_dir("bridge-dispatch-auth-test");
        let state = test_state(&dir);
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &HashMap::new(),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 401);
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_kill_switch_blocks_writes_only() {
        let dir = test_dir("bridge-dispatch-kill-test");
        let state = test_state(&dir);
        state.kill_switch.store(true, Ordering::SeqCst);
        let body = serde_json::to_vec(&json!({"root": "work", "path": "a.txt", "content": "x"})).unwrap();
        let r = dispatch_request(
            &state,
            "POST",
            "/api/write",
            "/api/write",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            &body,
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        // 读不受熔断影响。
        let r = dispatch_request(
            &state,
            "GET",
            "/api/list?root=work&path=.",
            "/api/list",
            &headers(&[("root", "work"), ("path", ".")]),
            &headers(&[("x-bridge-token", "t")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_hmac_mode() {
        let dir = test_dir("bridge-dispatch-hmac-test");
        let state = test_state(&dir);
        state.cfg.write().unwrap().require_hmac = true;
        state.cfg.write().unwrap().token = "secret".to_string();
        let ts = now_unix_secs().to_string();
        let sig = hmac_sha256_hex(b"secret", &ts, "GET", "/health", &[]);
        // 签名对了放行。
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &headers(&[
                ("x-bridge-timestamp", &ts),
                ("x-bridge-signature", &sig),
            ]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        // 签名错了 401。
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &headers(&[
                ("x-bridge-timestamp", &ts),
                ("x-bridge-signature", "00"),
            ]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 401);
        // 时间差超过 60 秒 401（重放）。
        let old_ts = (now_unix_secs() - 300).to_string();
        let old_sig = hmac_sha256_hex(b"secret", &old_ts, "GET", "/health", &[]);
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &headers(&[
                ("x-bridge-timestamp", &old_ts),
                ("x-bridge-signature", &old_sig),
            ]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 401);
        // HMAC 模式下光有 token 不够（token 是 key，不再当通行证）。
        let r = dispatch_request(
            &state,
            "GET",
            "/health",
            "/health",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "secret")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 401);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_patch_crlf_roundtrip() {
        let dir = test_dir("bridge-dispatch-patch-test");
        let state = test_state(&dir);
        std::fs::write(dir.join("a.txt"), "x = 1\r\ny = 2\r\n").unwrap();
        let body = serde_json::to_vec(&json!({
            "root": "work", "path": "a.txt",
            "patches": [{"search": "x = 1", "replace": "x = 10"}]
        }))
        .unwrap();
        let r = dispatch_request(
            &state,
            "POST",
            "/api/patch",
            "/api/patch",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            &body,
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(content, "x = 10\r\ny = 2\r\n");
        // 歧义修补走 409。
        let body = serde_json::to_vec(&json!({
            "root": "work", "path": "a.txt",
            "patches": [{"search": "y = 2", "replace": "y = 2"}]
        }))
        .unwrap();
        let _ = body;
        std::fs::write(dir.join("b.txt"), "k = 1\nk = 1\n").unwrap();
        let body = serde_json::to_vec(&json!({
            "root": "work", "path": "b.txt",
            "patches": [{"search": "k = 1", "replace": "k = 2"}]
        }))
        .unwrap();
        let r = dispatch_request(
            &state,
            "POST",
            "/api/patch",
            "/api/patch",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            &body,
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 409);
        // 写保护：build.rs 远程写要审批——测试里没人点，1 秒超时后按拒绝算。
        // 这里只验硬禁：.git 下的文件直接 403，不进审批。
        let body = serde_json::to_vec(&json!({
            "root": "work", "path": ".git/config", "content": "x"
        }))
        .unwrap();
        let r = dispatch_request(
            &state,
            "POST",
            "/api/write",
            "/api/write",
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            &body,
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_command_asklist_needs_approval() {
        // git push 在审批表里：远程调用时，没人点允许就 60 秒超时——
        // 测试用 1 秒超时版 request_approval 逻辑验证：这里直接走 run_command，
        // 另起线程 200ms 后点拒绝，应得 403，且 git push 一次都没跑出去。
        let dir = test_dir("bridge-run-ask-test");
        let state = Arc::new(test_state(&dir));
        let state2 = Arc::clone(&state);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let id = {
                let q = state2.approvals.lock().unwrap();
                q.front().map(|r| r.id.clone())
            };
            if let Some(id) = id {
                let _ = resolve_approval(&state2, &id, false);
            }
        });
        let data = json!({"root": "work", "path": ".", "argv": ["git", "push"]});
        let r = run_command(&state, &data, false);
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_command_asklist_approved_runs() {
        // 点允许后，git push 真跑出去（在临时目录里不是 git 仓库，
        // 立刻失败 exit 128——重点是它确实执行了，而不是被 403 拦下）。
        let dir = test_dir("bridge-run-ask-ok-test");
        let state = Arc::new(test_state(&dir));
        let state2 = Arc::clone(&state);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let id = {
                let q = state2.approvals.lock().unwrap();
                q.front().map(|r| r.id.clone())
            };
            if let Some(id) = id {
                let _ = resolve_approval(&state2, &id, true);
            }
        });
        let data = json!({"root": "work", "path": ".", "argv": ["git", "push"]});
        let r = run_command(&state, &data, false);
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["exit_code"], 128);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v0.9：三档权限 ----

    /// 另起线程 200ms 后处理队首审批（点允许 / 拒绝）。
    fn resolve_front_after(state: Arc<ServerState>, allow: bool) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let id = {
                let q = state.approvals.lock().unwrap();
                q.front().map(|r| r.id.clone())
            };
            if let Some(id) = id {
                let _ = resolve_approval(&state, &id, allow);
            }
        });
    }

    fn dispatch_remote(
        state: &ServerState,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> HttpResponse {
        dispatch_request(
            state,
            method,
            path,
            path,
            &HashMap::new(),
            &headers(&[("x-bridge-token", "t")]),
            body,
            Some("203.0.113.1".parse().unwrap()),
        )
    }

    /// 带 query 的远程调用（/api/read、/api/search、/api/tasks/status 这类 GET 用）。
    fn dispatch_remote_path(
        state: &ServerState,
        method: &str,
        path: &str,
        query: &HashMap<String, String>,
    ) -> HttpResponse {
        dispatch_request(
            state,
            method,
            path,
            path,
            query,
            &headers(&[("x-bridge-token", "t")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        )
    }

    fn set_permission(state: &ServerState, mode: crate::config::PermissionMode) {
        let mut cfg = state.config();
        cfg.permission_mode = mode;
        state.update_config(cfg);
    }

    #[test]
    fn permission_ask_always_write_needs_approval() {
        // ask_always：普通写文件也要审批；点允许才落盘，点拒绝 403。
        let dir = test_dir("bridge-perm-ask-write-test");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::AskAlways);
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "a.txt", "content": "hello"}))
                .unwrap();
        resolve_front_after(Arc::clone(&state), true);
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 200);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "hello");
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "b.txt", "content": "no"}))
                .unwrap();
        resolve_front_after(Arc::clone(&state), false);
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 403);
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_ask_always_mkdir_and_run_need_approval() {
        // ask_always：建目录与跑命令也要审批；只读接口（list/read）仍直接放行。
        let dir = test_dir("bridge-perm-ask-mkdir-test");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::AskAlways);
        let body = serde_json::to_vec(&json!({"root": "work", "path": "newdir"})).unwrap();
        resolve_front_after(Arc::clone(&state), true);
        let r = dispatch_remote(&state, "POST", "/api/mkdir", &body);
        assert_eq!(r.code, 200);
        assert!(dir.join("newdir").is_dir());
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": ".", "argv": ["cargo", "--version"]}),
        )
        .unwrap();
        resolve_front_after(Arc::clone(&state), true);
        let r = dispatch_remote(&state, "POST", "/api/run", &body);
        assert_eq!(r.code, 200);
        // 只读接口不受影响。
        let r = dispatch_request(
            &state,
            "GET",
            "/api/list",
            "/api/list",
            &headers(&[("root", "work"), ("path", ".")]),
            &headers(&[("x-bridge-token", "t")]),
            &[],
            Some("203.0.113.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_smart_auto_only_risky_needs_approval() {
        // smart_auto（默认）：普通写直接放行；build.rs 写要审批。
        let dir = test_dir("bridge-perm-smart-test");
        let state = Arc::new(test_state(&dir));
        assert_eq!(
            state.config().permission_mode,
            crate::config::PermissionMode::SmartAuto
        );
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "a.txt", "content": "x"})).unwrap();
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 200);
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": "build.rs", "content": "fn main(){}"}),
        )
        .unwrap();
        resolve_front_after(Arc::clone(&state), false);
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 403);
        assert!(!dir.join("build.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_full_access_unlisted_command_runs() {
        // full_access：白名单外的开发命令也直接跑；build.rs 直接写；
        // 但 bridge-config.json 仍硬禁，shell 仍禁。
        let dir = test_dir("bridge-perm-full-test");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::FullAccess);
        // cargo --help 不在白名单也不在审批表，full_access 下直接跑。
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": ".", "argv": ["cargo", "--help"]}),
        )
        .unwrap();
        let r = dispatch_remote(&state, "POST", "/api/run", &body);
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["exit_code"], 0);
        // build.rs 直接写，不挂起。
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": "build.rs", "content": "fn main(){}"}),
        )
        .unwrap();
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 200);
        // bridge-config.json 仍硬禁。
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": "bridge-config.json", "content": "{}"}),
        )
        .unwrap();
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 403);
        // shell 三档都禁。
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": ".", "argv": ["cmd", "/c", "echo hi"]}),
        )
        .unwrap();
        let r = dispatch_remote(&state, "POST", "/api/run", &body);
        assert_eq!(r.code, 403);
        let body = serde_json::to_vec(
            &json!({"root": "work", "path": ".", "argv": ["powershell", "-c", "echo hi"]}),
        )
        .unwrap();
        let r = dispatch_remote(&state, "POST", "/api/run", &body);
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_mode_switch_applies_live() {
        // 权限级别切换立即生效，不用重启：smart_auto 下直接写，
        // 切到 ask_always 后同一接口立刻要审批。
        let dir = test_dir("bridge-perm-live-test");
        let state = Arc::new(test_state(&dir));
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "a.txt", "content": "x"})).unwrap();
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 200);
        set_permission(&state, crate::config::PermissionMode::AskAlways);
        let body =
            serde_json::to_vec(&json!({"root": "work", "path": "b.txt", "content": "x"})).unwrap();
        resolve_front_after(Arc::clone(&state), false);
        let r = dispatch_remote(&state, "POST", "/api/write", &body);
        assert_eq!(r.code, 403);
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_preauthed_cannot_touch_panel() {
        // 预授权通道（WS / relay）：health 能进，面板接口一律 403。
        let dir = test_dir("bridge-perm-preauth-test");
        let state = test_state(&dir);
        let r = dispatch_request_preauthed(&state, "GET", "/health", "/health", &HashMap::new(), &[]);
        assert_eq!(r.code, 200);
        let r = dispatch_request_preauthed(&state, "GET", "/", "/", &HashMap::new(), &[]);
        assert_eq!(r.code, 403);
        let r = dispatch_request_preauthed(&state, "GET", "/ui/state", "/ui/state", &HashMap::new(), &[]);
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_save_config_hot_reload_response() {
        // /ui/save-config：改权限级别后返回 needs_restart=false，
        // 实时配置与磁盘文件都已更新（NetworkManager 不在单测里，通知直接忽略）。
        let dir = test_dir("bridge-save-hot-test");
        let state = test_state(&dir);
        let cfg_path = dir.join("bridge-config.json");
        let body = serde_json::to_vec(&json!({
            "permission_mode": "full_access",
        }))
        .unwrap();
        let r = dispatch_request_internal(
            &state,
            "POST",
            "/ui/save-config",
            "/ui/save-config",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &body,
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["needs_restart"], false);
        assert_eq!(v["applied"], true);
        let live = state.config();
        assert_eq!(live.permission_mode, crate::config::PermissionMode::FullAccess);
        let on_disk =
            crate::config::BridgeConfig::load_or_create_example(&cfg_path).unwrap();
        assert_eq!(
            on_disk.permission_mode,
            crate::config::PermissionMode::FullAccess
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_http_socket_blocked_but_internal_ok() {
        // v2.3.1：HTTP Socket 直接 POST /ui/save-config 被拒（403），
        // 而 Tauri 内部调用（dispatch_request_internal）正常（200）——
        // 本机第三方脚本再也不能带 X-Pylon-UI: 1 把权限静默篡改为 full_access。
        let dir = test_dir("bridge-ui-http-blocked-test");
        let state = test_state(&dir);
        let body = serde_json::to_vec(&json!({
            "permission_mode": "full_access",
        }))
        .unwrap();
        // 模拟本机第三方脚本：127.0.0.1 + X-Pylon-UI: 1，走 HTTP 通道。
        let r = dispatch_request(
            &state,
            "POST",
            "/ui/save-config",
            "/ui/save-config",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &body,
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 403);
        // 权限没被改掉（默认 smart_auto）。
        assert_eq!(
            state.config().permission_mode,
            crate::config::PermissionMode::SmartAuto
        );
        // 同一请求走 Tauri 内部调用：200，权限正常改掉。
        let r = dispatch_request_internal(
            &state,
            "POST",
            "/ui/save-config",
            "/ui/save-config",
            &HashMap::new(),
            &headers(&[("x-pylon-ui", "1")]),
            &body,
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(r.code, 200);
        assert_eq!(
            state.config().permission_mode,
            crate::config::PermissionMode::FullAccess
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v1.0 模块二 / 三 / 四 / 五 ----

    fn test_state_full(dir: &Path) -> ServerState {
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.permission_mode = PermissionMode::FullAccess;
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        ServerState::new(cfg, dir.join("bridge-config.json"), logs)
    }

    #[test]
    fn unified_diff_simple_change() {
        let d = unified_diff("a.txt", "x = 1\ny = 2\nz = 3\n", "x = 1\ny = 20\nz = 3\n");
        assert!(d.contains("--- a.txt (原)"), "实际：{d}");
        assert!(d.contains("+++ a.txt (新)"), "实际：{d}");
        assert!(d.contains("-y = 2"), "实际：{d}");
        assert!(d.contains("+y = 20"), "实际：{d}");
        assert!(d.contains("@@ -1,3 +1,3 @@"), "实际：{d}");
    }

    #[test]
    fn unified_diff_identical_is_empty() {
        assert_eq!(unified_diff("a.txt", "a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn unified_diff_add_and_delete() {
        let d = unified_diff("a.txt", "a\nb\n", "a\nc\nd\n");
        assert!(d.contains("-b"), "实际：{d}");
        assert!(d.contains("+c"), "实际：{d}");
        assert!(d.contains("+d"), "实际：{d}");
    }

    #[test]
    fn unified_diff_truncates_at_4000_chars() {
        let old: String = (0..300).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..300).map(|i| format!("LINE {i}\n")).collect();
        let d = unified_diff("big.txt", &old, &new);
        assert!(d.chars().count() <= 4000 + 20, "实际长度：{}", d.chars().count());
        assert!(d.contains("已截断 4000 字符"), "实际：{d}");
    }

    #[test]
    fn snapshot_write_then_revert() {
        let dir = test_dir("bridge-snapshot-test");
        let state = test_state_full(&dir);
        let f = dir.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        // 写 v2：响应里带 snapshot_id。
        let r = handle_write(
            &state,
            &json!({"root": "work", "path": "a.txt", "content": "v2"}),
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let snap = v["snapshot_id"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2");
        // 回滚：v1 回来。
        let path = revert_snapshot(&state, &snap).unwrap();
        assert_eq!(path, "a.txt");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // 新建文件的快照：回滚 = 删掉它。
        let r = handle_write(
            &state,
            &json!({"root": "work", "path": "new.txt", "content": "n"}),
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let snap2 = v["snapshot_id"].as_str().unwrap().to_string();
        assert!(dir.join("new.txt").exists());
        revert_snapshot(&state, &snap2).unwrap();
        assert!(!dir.join("new.txt").exists());
        // 30 条上限：再写 35 次，只剩 30 张。
        for i in 0..35 {
            let _ = handle_write(
                &state,
                &json!({"root": "work", "path": "a.txt", "content": format!("v{i}")}),
            );
        }
        assert_eq!(state.snapshots.lock().unwrap().len(), 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_takes_protection_snapshot() {
        // 回滚前保护：回滚把 v1 写回去之前，先为当前文件（v3）拍一张保护快照，
        // 误点历史回滚也能二次反悔（再回滚保护快照就回到 v3）。
        let dir = test_dir("bridge-revert-protect-test");
        let state = test_state_full(&dir);
        let f = dir.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        // 写 v2：响应里的快照拍的是 v1。
        let r = handle_write(
            &state,
            &json!({"root": "work", "path": "a.txt", "content": "v2"}),
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let snap = v["snapshot_id"].as_str().unwrap().to_string();
        // 用户后来又改成 v3（不在任何快照里）。
        std::fs::write(&f, "v3").unwrap();
        // 回滚到 v1。
        let path = revert_snapshot(&state, &snap).unwrap();
        assert_eq!(path, "a.txt");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // 保护快照已入队，且存的是回滚前的 v3。
        let guard = state.snapshots.lock().unwrap();
        let prot = guard
            .iter()
            .find(|s| s.action == "回滚前保护")
            .expect("回滚前应自动拍一张保护快照");
        assert_eq!(prot.rel, "a.txt");
        assert_eq!(prot.content.as_deref(), Some(b"v3".as_slice()));
        let prot_id = prot.id.clone();
        drop(guard);
        // 动态里最后一条应挂着保护快照的 id（面板上能一键二次回滚）。
        let logs = state.logs.lock().unwrap();
        let last = logs.last().expect("回滚应记一条动态");
        assert_eq!(last.snapshot_id.as_deref(), Some(prot_id.as_str()));
        drop(logs);
        // 二次反悔：回滚保护快照，v3 回来。
        let path2 = revert_snapshot(&state, &prot_id).unwrap();
        assert_eq!(path2, "a.txt");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_carries_diff_preview() {
        let dir = test_dir("bridge-approval-diff-test");
        let state = Arc::new(test_state(&dir));
        let state2 = Arc::clone(&state);
        let diff =
            "--- a.txt (原)\n+++ a.txt (新)\n@@ -1,1 +1,1 @@\n-x = 1\n+x = 2\n".to_string();
        let handle = std::thread::spawn(move || {
            request_approval_timeout_with_diff(&state2, "write", "写入 a.txt", Some(diff), 5)
        });
        std::thread::sleep(Duration::from_millis(200));
        let list = list_approvals(&state);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["kind"], "write");
        assert!(
            list[0]["diff_preview"]
                .as_str()
                .unwrap()
                .contains("-x = 1"),
            "实际：{}",
            list[0]
        );
        let id = list[0]["id"].as_str().unwrap().to_string();
        assert!(resolve_approval(&state, &id, true));
        assert_eq!(handle.join().unwrap(), ApprovalOutcome::Approved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trash_moves_to_pylon_trash_and_reverts() {
        let dir = test_dir("bridge-trash-test");
        let state = test_state_full(&dir);
        std::fs::write(dir.join("x.txt"), "hello").unwrap();
        let r = handle_trash(&state, &json!({"root": "work", "path": "x.txt"}));
        assert_eq!(r.code, 200, "{}", String::from_utf8_lossy(&r.body));
        assert!(!dir.join("x.txt").exists());
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let trash_rel = v["trash_path"].as_str().unwrap().to_string();
        assert!(trash_rel.starts_with(".pylon_trash/"), "实际：{trash_rel}");
        assert!(dir.join(&trash_rel).exists());
        let snap = v["snapshot_id"].as_str().unwrap().to_string();
        // list 不展示回收站。
        let r = handle_list(
            &state,
            &HashMap::from([
                ("root".to_string(), "work".to_string()),
                ("path".to_string(), ".".to_string()),
            ]),
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let names: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(!names.contains(&".pylon_trash"), "实际：{names:?}");
        // 回滚：文件移回来。
        revert_snapshot(&state, &snap).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x.txt")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_and_overwrites_with_snapshot() {
        let dir = test_dir("bridge-rename-test");
        let state = test_state_full(&dir);
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        std::fs::write(dir.join("b.txt"), "B").unwrap();
        let r = handle_rename(
            &state,
            &json!({"root": "work", "from": "a.txt", "to": "c.txt"}),
        );
        assert_eq!(r.code, 200);
        assert!(!dir.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "A");
        // 覆盖 b.txt：b 的旧内容进快照，c 移过去。
        let r = handle_rename(
            &state,
            &json!({"root": "work", "from": "c.txt", "to": "b.txt"}),
        );
        assert_eq!(r.code, 200);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "A");
        // 回滚这次重命名：b.txt 回到 c.txt。
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let snap = v["snapshot_id"].as_str().unwrap();
        revert_snapshot(&state, snap).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "A");
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_protected_destination_requires_approval() {
        // v2.2.1：rename 的 to 端必须过写保护门控——smart_auto 下
        // a.txt -> build.rs / a.txt -> setup.bat 要挂起审批（不能直接 200 落盘）；
        // 用户拒绝后 403、目标不存在、源文件还在，审批 detail 里能看见目标文件名。
        let dir = test_dir("bridge-rename-protected-dst");
        let state = Arc::new(test_state(&dir));
        assert_eq!(
            state.config().permission_mode,
            crate::config::PermissionMode::SmartAuto
        );
        for dst in ["build.rs", "setup.bat"] {
            let src = format!("src-{dst}.txt");
            std::fs::write(dir.join(&src), "payload").unwrap();
            let body = json!({"root": "work", "from": src, "to": dst}).to_string();
            // 后台线程等审批入队，记下 detail 后点拒绝（超时 5 秒兜底防挂死）。
            let state2 = Arc::clone(&state);
            let dst_name = dst.to_string();
            let waiter = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut detail = String::new();
                while Instant::now() < deadline {
                    let id = {
                        let q = state2.approvals.lock().unwrap();
                        q.front().map(|r| {
                            detail = r.detail.clone();
                            r.id.clone()
                        })
                    };
                    if let Some(id) = id {
                        assert!(resolve_approval(&state2, &id, false));
                        return detail;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                detail
            });
            let r = dispatch_remote(&state, "POST", "/api/rename", body.as_bytes());
            let detail = waiter.join().unwrap();
            assert_eq!(
                r.code,
                403,
                "rename -> {dst} 应被审批拒绝：{}",
                String::from_utf8_lossy(&r.body)
            );
            assert!(!dir.join(dst).exists(), "拒绝后目标 {dst} 不能落盘");
            assert!(dir.join(&src).exists(), "拒绝后源文件必须还在");
            assert!(
                detail.contains(&dst_name),
                "审批 detail 应含目标文件名 {dst_name}：{detail:?}"
            );
            assert!(state.approvals.lock().unwrap().is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_roundtrip_via_handlers() {
        let dir = test_dir("bridge-base64-test");
        let state = test_state_full(&dir);
        let bytes: Vec<u8> = (0..=255u8).collect();
        let b64 = base64_encode(&bytes);
        let r = handle_write_base64(
            &state,
            &json!({"root": "work", "path": "bin.dat", "content_base64": b64}),
        );
        assert_eq!(r.code, 200);
        let r = handle_read_base64(
            &state,
            &HashMap::from([
                ("root".to_string(), "work".to_string()),
                ("path".to_string(), "bin.dat".to_string()),
            ]),
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            base64_decode(v["content_base64"].as_str().unwrap()).unwrap(),
            bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 日志落盘的两个测试不碰全局落盘路径（set_log_file），
    // 直接调纯逻辑函数 append_log_file_to / restore_log_file，
    // 免得并行跑的其他测试的 push_log 也往文件里写。
    #[test]
    fn log_file_append_and_restore() {
        let dir = test_dir("bridge-logfile-test");
        let log_path = dir.join("pylon-activity.log");
        let _ = std::fs::remove_file(&log_path);
        append_log_file_to(
            &log_path,
            &LogEntry {
                date: "2026-11-09".to_string(),
                time: "10:00:01".to_string(),
                msg: "第一条".to_string(),
                snapshot_id: None,
            },
        );
        append_log_file_to(
            &log_path,
            &LogEntry {
                date: "2026-11-09".to_string(),
                time: "10:00:02".to_string(),
                msg: "第二条".to_string(),
                snapshot_id: None,
            },
        );
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("第一条") && content.contains("第二条"), "实际：{content}");
        // 启动恢复：新列表读回末尾 50 条。
        let logs2: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let n2 = restore_log_file(&logs2, &log_path);
        assert_eq!(n2, 2, "返回值应等于恢复条数");
        let g = logs2.lock().unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].msg, "第一条");
        assert_eq!(g[0].time, "10:00:01");
        assert_eq!(g[0].date, "2026-11-09");
        // 旧格式行（v1.0.2 及以前，无日期）也能恢复，日期按今天算。
        std::fs::write(&log_path, "09:00:00 旧格式的一条\n").unwrap();
        let logs3: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let n3 = restore_log_file(&logs3, &log_path);
        assert_eq!(n3, 1, "返回值应等于恢复条数");
        let g3 = logs3.lock().unwrap();
        assert_eq!(g3.len(), 1);
        assert_eq!(g3[0].msg, "旧格式的一条");
        assert_eq!(g3[0].time, "09:00:00");
        assert_eq!(g3[0].date, chrono::Local::now().format("%Y-%m-%d").to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v2.1：重启回读要同时读主文件 + `.old` 轮转文件（与热力图同一口径）。
    /// 场景：主文件只有 5 条新记录，`.old` 里有 60 条旧记录——合起来取
    /// 末尾 50 条，应是 `.old` 的后 45 条 + 主文件的 5 条。
    #[test]
    fn restore_reads_old_file_too() {
        let dir = test_dir("bridge-logfile-old-test");
        let log_path = dir.join("pylon-activity.log");
        let old_path = PathBuf::from(format!("{}.old", log_path.display()));
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&old_path);
        // .old 里写 60 条旧记录（日期 2026-11-10）。
        let mut old_content = String::new();
        for i in 0..60 {
            old_content.push_str(&format!(
                "2026-11-10 09:{:02}:{:02} 旧记录{}\n",
                i / 60,
                i % 60,
                i
            ));
        }
        std::fs::write(&old_path, &old_content).unwrap();
        // 主文件写 5 条新记录（日期 2026-11-11）。
        let mut main_content = String::new();
        for i in 0..5 {
            main_content.push_str(&format!("2026-11-11 10:00:0{} 新记录{}\n", i, i));
        }
        std::fs::write(&log_path, &main_content).unwrap();
        // 重启回读。
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let n = restore_log_file(&logs, &log_path);
        assert_eq!(n, 50, "合起来取末尾 50 条");
        let g = logs.lock().unwrap();
        assert_eq!(g.len(), 50);
        // 前 45 条是 .old 的后 45 条（旧记录15..59），后 5 条是主文件的新记录。
        assert_eq!(g[0].msg, "旧记录15", "实际首条：{}", g[0].msg);
        assert_eq!(g[44].msg, "旧记录59");
        assert_eq!(g[45].msg, "新记录0");
        assert_eq!(g[49].msg, "新记录4");
        // 只有 .old、没有主文件时也能读。
        let _ = std::fs::remove_file(&log_path);
        let logs2: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let n2 = restore_log_file(&logs2, &log_path);
        assert_eq!(n2, 50, "只有 .old 时也取末尾 50 条");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v2.1：敏感文件禁读——配置文件与活动日志（大小写/尾部点号/空格
    /// 变体也要拦），普通文件放行。
    #[test]
    fn read_protection_table() {
        // 配置文件：原名、大小写变体、尾部点号/空格变体全拦。
        assert!(check_read_protected("bridge-config.json"));
        assert!(check_read_protected("BRIDGE-CONFIG.JSON."));
        assert!(check_read_protected("bridge-config.JSON "));
        assert!(check_read_protected("sub/bridge-config.json"));
        assert!(check_read_protected("local-bridge-rust/bridge-config.json"));
        // 活动日志：主文件与 .old 轮转文件全拦。
        assert!(check_read_protected("pylon-activity.log"));
        assert!(check_read_protected("PYLON-ACTIVITY.LOG."));
        assert!(check_read_protected("target/release/pylon-activity.log"));
        assert!(check_read_protected("pylon-activity.log.old"));
        assert!(check_read_protected("pylon-activity.log.old."));
        // .git 下的文件也拦。
        assert!(check_read_protected(".git/config"));
        // 普通文件放行（不误伤）。
        assert!(!check_read_protected("notes.md"));
        assert!(!check_read_protected("src/bridge.rs"));
        assert!(!check_read_protected("bridge-config.json.bak"));
        assert!(!check_read_protected("my-pylon-activity.log.txt"));
    }

    #[test]
    fn read_and_search_protected_rotated_log() {
        // v2.2.1 端点级回归：轮转日志 pylon-activity.log.old 与主日志同级保护，
        // 三个读出口都要覆盖——/api/read、/api/read_base64 403；
        // /api/search 不返回它的命中（同目录普通文件的同名标记照常搜得到）。
        let dir = test_dir("bridge-read-protected-old");
        let marker = "PylonRotatedLogMarker7f3a";
        std::fs::write(
            dir.join("pylon-activity.log.old"),
            format!("2026-11-13 10:00:00 写入 secret {marker}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("normal.txt"), format!("普通文件 {marker}\n")).unwrap();
        let state = Arc::new(test_state(&dir));
        for route in ["/api/read", "/api/read_base64"] {
            let r = dispatch_remote_path(
                &state,
                "GET",
                route,
                &parse_query("root=work&path=pylon-activity.log.old"),
            );
            assert_eq!(
                r.code,
                403,
                "{route} 读 .old 轮转日志必须 403：{}",
                String::from_utf8_lossy(&r.body)
            );
        }
        let r = dispatch_remote_path(
            &state,
            "GET",
            "/api/search",
            &parse_query(&format!("root=work&path=.&query={marker}")),
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let files: Vec<&str> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["file"].as_str())
            .collect();
        assert!(
            !files.iter().any(|f| f.ends_with("pylon-activity.log.old")),
            "轮转日志不得被搜索命中：{files:?}"
        );
        // 反向对照：普通文件能搜到，证明搜索本身没坏（不是全空导致的假绿）。
        assert!(
            files.iter().any(|f| *f == "normal.txt"),
            "普通文件的标记应该能搜到：{files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heatmap_counts_days() {
        // v1.0.3：新格式行按日期统计；旧格式行跳过；空行/脏行跳过。
        let content = "2026-11-08 09:00:01 写入 a\n2026-11-08 10:00:02 写入 b\n2026-11-09 10:00:03 写入 c\n09:00:00 旧格式的一条\n不是日志\n";
        let days = count_heatmap_days(content);
        assert_eq!(days.get("2026-11-08"), Some(&2));
        assert_eq!(days.get("2026-11-09"), Some(&1));
        assert_eq!(days.len(), 2);
        assert_eq!(days.values().sum::<u64>(), 3);
    }

    #[test]
    fn log_file_rotates_over_5mb() {
        let dir = test_dir("bridge-logrotate-test");
        let log_path = dir.join("pylon-activity.log");
        // 先造一个超 5MB 的旧日志。
        let big = vec![b'x'; 5 * 1024 * 1024 + 100];
        std::fs::write(&log_path, &big).unwrap();
        append_log_file_to(
            &log_path,
            &LogEntry {
                date: "2026-11-09".to_string(),
                time: "10:00:03".to_string(),
                msg: "轮转后的一条".to_string(),
                snapshot_id: None,
            },
        );
        assert!(dir.join("pylon-activity.log.old").exists());
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("轮转后的一条"));
        assert!(content.len() < 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_file_concurrent_appends_stay_whole() {
        // v1.0.1：8 线程 × 25 次并发追加，200 行必须行行完整——
        // 之前没加写锁时，真机日志里出现过两行字串在一起的脏行。
        let dir = test_dir("bridge-logconcurrent-test");
        let log_path = dir.join("pylon-activity.log");
        let _ = std::fs::remove_file(&log_path);
        let path = Arc::new(log_path);
        let mut handles = Vec::new();
        for t in 0..8 {
            let p = Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    append_log_file_to(
                        &p,
                        &LogEntry {
                            date: "2026-11-09".to_string(),
                            time: "10:00:01".to_string(),
                            msg: format!("msg-{t}-{i}"),
                            snapshot_id: None,
                        },
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let content = std::fs::read_to_string(path.as_path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            200,
            "实际行数：{}，内容头：{}",
            lines.len(),
            &content[..content.len().min(300)]
        );
        for line in &lines {
            assert!(
                line.len() > 20
                    && line.as_bytes()[10] == b' '
                    && line.as_bytes()[19] == b' '
                    && line[20..].starts_with("msg-"),
                "脏行：{line}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_response_carries_no_store() {
        // v1.0.1：响应头必须带 Cache-Control: no-store，否则浏览器
        // 缓存了注入旧密钥的面板 HTML，重启后 /ui/* 全是 401。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            send_response(&mut s, &HttpResponse::json(200, "OK", json!({"ok": true})));
        });
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("Cache-Control: no-store"),
            "实际响应头：{}",
            text.split("\r\n\r\n").next().unwrap_or("")
        );
        server.join().unwrap();
    }

    // ---- v2.2.0 模块一：日志单行化 + 脏行容错 ----

    #[test]
    fn log_msg_multiline_becomes_single_line() {
        // 多行命令（python -c "...\n..."）的日志必须严格单行：
        // \r\n / \r / \n 全部变 " ↵ "，连续空白折叠，首尾修剪。
        assert_eq!(sanitize_log_msg("a\r\nb\rc\nd"), "a ↵ b ↵ c ↵ d");
        assert_eq!(sanitize_log_msg("x   y\t z  "), "x y z");
        assert_eq!(
            sanitize_log_msg("运行 python -c \"print(1)\nprint(2)\" -> exit 0"),
            "运行 python -c \"print(1) ↵ print(2)\" -> exit 0"
        );
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        push_log(&logs, "第一行\n第二行\r\n第三行".to_string());
        let g = logs.lock().unwrap();
        assert_eq!(g.len(), 1);
        assert!(!g[0].msg.contains('\n') && !g[0].msg.contains('\r'));
        assert_eq!(g[0].msg, "第一行 ↵ 第二行 ↵ 第三行");
    }

    #[test]
    fn log_file_dirty_lines_skipped_on_restore_and_heatmap() {
        // 历史日志里残留的多行片段（无时间戳前缀）不得污染恢复列表与热力图。
        let dir = test_dir("bridge-dirty-log-test");
        let path = dir.join("pylon-activity.log");
        let _ = std::fs::remove_file(&path);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        std::fs::write(
            &path,
            format!(
                "{today} 10:00:00 正常一\n这是上一行命令溢出的脏片段\n{today} 10:00:01 正常二\n2026-11-13 只有日期没有时间\n"
            ),
        )
        .unwrap();
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let n = restore_log_file(&logs, &path);
        assert_eq!(n, 2, "脏行必须被跳过，只恢复 2 条");
        let days = count_heatmap_days(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(days.get(&today), Some(&2), "脏行不得计入热力图");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v2.2.0 模块二：异步后台任务引擎 ----

    #[test]
    fn tasks_start_status_list_stop_flow() {
        // full_access 下启动 git --version（云端/真机都有 git），立即返回
        // task_id+pid；轮询到结束：exit 0、stdout 含 "git"、offset 推进；
        // list 可见；对已结束任务 stop 幂等 ok。
        let dir = test_dir("bridge-tasks-flow");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::FullAccess);
        let body = json!({"root":"work","path":".","argv":["git","--version"]});
        let r = dispatch_request(
            &state, "POST", "/api/tasks/start", "/api/tasks/start",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            body.to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["ok"], true);
        let id = v["task_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("task-"));
        assert!(v["pid"].as_u64().unwrap() > 0);
        // 轮询 status 直到结束（最多 10 秒），增量拼 stdout。
        let mut offset = 0usize;
        let mut done = false;
        let mut out = String::new();
        for _ in 0..100 {
            let q = format!("id={id}&offset={offset}");
            let r = dispatch_request(
                &state, "GET", "/api/tasks/status", "/api/tasks/status",
                &parse_query(&q), &headers(&[("x-bridge-token", "t")]), b"", None,
            );
            assert_eq!(r.code, 200);
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            out.push_str(v["stdout_chunk"].as_str().unwrap());
            offset = v["next_offset"].as_u64().unwrap() as usize;
            if !v["running"].as_bool().unwrap() {
                assert_eq!(v["exit_code"], 0);
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(done, "任务 10 秒内没结束");
        assert!(out.contains("git"), "stdout 应含 git 版本串：{out}");
        // list 里能看到它。
        let r = dispatch_request(
            &state, "GET", "/api/tasks/list", "/api/tasks/list",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]), b"", None,
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["tasks"].as_array().unwrap().iter().any(|t| t["id"] == id));
        // 已结束任务 stop 幂等。
        let r = dispatch_request(
            &state, "POST", "/api/tasks/stop", "/api/tasks/stop",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            json!({"id": id}).to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_smart_auto_unlisted_403() {
        // smart_auto 下白名单外命令（git --version 不在默认白名单）-> 403，不启动。
        let dir = test_dir("bridge-tasks-403");
        let state = Arc::new(test_state(&dir));
        let body = json!({"root":"work","path":".","argv":["git","--version"]});
        let r = dispatch_request(
            &state, "POST", "/api/tasks/start", "/api/tasks/start",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            body.to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_concurrency_limit_429() {
        // v2.2.1：并发预占满（with_limit(0)）时 /api/tasks/start 必须 429，
        // 文案含上限 MAX_CONCURRENT_TASKS，且一条任务都不能入列。
        let dir = test_dir("bridge-tasks-429");
        let mut st = test_state(&dir);
        st.tasks = Arc::new(crate::tasks::TaskManager::with_limit(0));
        set_permission(&st, crate::config::PermissionMode::FullAccess);
        let state = Arc::new(st);
        let body = json!({"root":"work","path":".","argv":["git","--version"]});
        let r = dispatch_request(
            &state, "POST", "/api/tasks/start", "/api/tasks/start",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            body.to_string().as_bytes(), None,
        );
        assert_eq!(
            r.code,
            429,
            "并发超限应 429：{}",
            String::from_utf8_lossy(&r.body)
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let err = v["error"].as_str().unwrap_or("");
        assert!(
            err.contains(&crate::tasks::MAX_CONCURRENT_TASKS.to_string()),
            "429 文案要含并发上限：{err:?}"
        );
        // list 为空：被拒的请求不能留下任务记录。
        let r = dispatch_remote_path(&state, "GET", "/api/tasks/list", &HashMap::new());
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v["tasks"].as_array().unwrap().is_empty(),
            "超限拒绝后不能有任务入列：{v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_status_tail_bytes() {
        // v2.2.1：tail_bytes=8 返回全量输出末尾 8 字节（UTF-8 边界对齐），
        // next_offset 仍是当前总长度——前端拿它当已看到末尾，游标不回退。
        let dir = test_dir("bridge-tasks-tail");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::FullAccess);
        let body = json!({"root":"work","path":".","argv":["git","--version"]});
        let r = dispatch_request(
            &state, "POST", "/api/tasks/start", "/api/tasks/start",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            body.to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let id = v["task_id"].as_str().unwrap().to_string();
        // 轮询到结束，累计全量 stdout（offset 增量推进）。
        let mut offset = 0usize;
        let mut full_out = String::new();
        let mut done = false;
        for _ in 0..150 {
            let q = parse_query(&format!("id={id}&offset={offset}"));
            let r = dispatch_remote_path(&state, "GET", "/api/tasks/status", &q);
            assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            full_out.push_str(v["stdout_chunk"].as_str().unwrap());
            offset = v["next_offset"].as_u64().unwrap() as usize;
            if !v["running"].as_bool().unwrap() {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(done, "任务 7.5 秒内没结束");
        let total = full_out.len();
        assert!(total >= 8, "git --version 输出应至少有 8 字节：{full_out:?}");
        assert_eq!(offset, total, "全量轮询后 next_offset 应等于总字节数");
        // tail_bytes=8：内容必须是全量后缀，next_offset 仍是总长度。
        let q = parse_query(&format!("id={id}&offset={offset}&tail_bytes=8"));
        let r = dispatch_remote_path(&state, "GET", "/api/tasks/status", &q);
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let tail = v["stdout_chunk"].as_str().unwrap();
        assert_eq!(v["tail_bytes"], 8, "响应要带 tail_bytes：{v}");
        assert!(!tail.is_empty(), "tail 不能为空：{v}");
        assert!(full_out.ends_with(tail), "tail 必须是全量输出的后缀：{tail:?}");
        if full_out.is_ascii() {
            assert_eq!(tail, &full_out[total - 8..], "ASCII 下 tail 就是原样末尾 8 字节");
        }
        assert_eq!(v["next_offset"].as_u64().unwrap() as usize, total);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn tasks_incremental_and_job_stop() {
        // Windows 真机：ping -n 30 长跑 -> 增量读到输出 -> stop 关 Job
        // 连根清理，3 秒内 running=false 且 stopped=true。
        let dir = test_dir("bridge-tasks-stop");
        let state = Arc::new(test_state(&dir));
        set_permission(&state, crate::config::PermissionMode::FullAccess);
        let body = json!({"root":"work","path":".","argv":["ping","-n","30","127.0.0.1"]});
        let r = dispatch_request(
            &state, "POST", "/api/tasks/start", "/api/tasks/start",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            body.to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let id = v["task_id"].as_str().unwrap().to_string();
        std::thread::sleep(Duration::from_millis(1500));
        let r = dispatch_request(
            &state, "GET", "/api/tasks/status", "/api/tasks/status",
            &parse_query(&format!("id={id}&offset=0")),
            &headers(&[("x-bridge-token", "t")]), b"", None,
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["running"].as_bool().unwrap());
        assert!(
            v["stdout_chunk"].as_str().unwrap().contains("127.0.0.1"),
            "应已增量读到 ping 输出：{}",
            v["stdout_chunk"]
        );
        let r = dispatch_request(
            &state, "POST", "/api/tasks/stop", "/api/tasks/stop",
            &HashMap::new(), &headers(&[("x-bridge-token", "t")]),
            json!({"id": id}).to_string().as_bytes(), None,
        );
        assert_eq!(r.code, 200);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let r = dispatch_request(
                &state, "GET", "/api/tasks/status", "/api/tasks/status",
                &parse_query(&format!("id={id}&offset=0")),
                &headers(&[("x-bridge-token", "t")]), b"", None,
            );
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            if !v["running"].as_bool().unwrap() {
                assert_eq!(v["stopped"], true);
                break;
            }
            assert!(Instant::now() < deadline, "Job stop 后 3 秒仍在跑");
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v2.2.0 模块三：Program Capsule 增强 ----

    #[test]
    fn program_get_returns_hash_lines_lastrun() {
        let dir = test_dir("bridge-prog-meta");
        std::fs::write(dir.join("app.html"), "line1\nline2\n").unwrap();
        let state = program_state(&dir);
        let r = handle_program_get(&state);
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(v["line_count"], 2);
        assert!(v["last_run"].is_null());
        // sha256 与内容一致（纯函数交叉验证）。
        assert_eq!(
            v["sha256"].as_str().unwrap(),
            sha256_hex(b"line1\nline2\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_post_expected_sha256_conflict_409() {
        let dir = test_dir("bridge-prog-conflict");
        std::fs::write(dir.join("app.html"), "original\n").unwrap();
        let state = program_state(&dir);
        // 错误哈希 -> 409 + 当前哈希（防远端盲覆盖）。
        let bad = json!({"content":"x","expected_sha256":"0".repeat(64)});
        let r = handle_program_post(&state, &bad);
        assert_eq!(r.code, 409);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // v2.3.3（P1 #12）：409 不再回显主程序全文——旧实现让任意错误哈希都能
        // 把整份内容读回来，等于绕开 /api/read 的读保护。需要正文请走 GET。
        assert!(
            v.get("content").is_none(),
            "409 不应回显 content：{v}"
        );
        assert_eq!(v["current_sha256"].as_str().unwrap().len(), 64);
        // 文件未被改动（冲突时不写）。
        assert_eq!(std::fs::read_to_string(dir.join("app.html")).unwrap(), "original\n");
        // 正确哈希 -> 200。
        let good_hash = v["current_sha256"].as_str().unwrap().to_string();
        let ok = json!({"content":"print('v2')\n","expected_sha256":good_hash});
        assert_eq!(handle_program_post(&state, &ok).code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_post_patch_then_run_sync_and_async() {
        // 改后即跑：patches + run_argv 同步拿 run 结果；async:true 拿 task_id。
        let dir = test_dir("bridge-prog-run");
        std::fs::write(dir.join("app.html"), "# original\n").unwrap();
        let state = Arc::new(program_state(&dir));
        let body = json!({"patches":[{"line":1,"content":"# patched"}],
            "run_argv":["git","--version"]});
        let r = handle_program_post(&state, &body);
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["patches_applied"], 1);
        assert_eq!(v["run"]["exit_code"], 0);
        assert!(v["run"]["stdout"].as_str().unwrap().contains("git"));
        // 文件确实被修补了。
        assert_eq!(std::fs::read_to_string(dir.join("app.html")).unwrap(), "# patched\n");
        // last_run 已更新（GET 能看到）。
        let g = handle_program_get(&state);
        let gv: Value = serde_json::from_slice(&g.body).unwrap();
        assert_eq!(gv["last_run"]["exit_code"], 0);
        assert_eq!(gv["last_run"]["async"], false);
        // async 分支：返回 task_id，轮询到结束。
        let body = json!({"content":"# async\n","run_argv":["git","--version"],"async":true});
        let r = handle_program_post(&state, &body);
        assert_eq!(r.code, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let tid = v["task_id"].as_str().unwrap().to_string();
        assert!(tid.starts_with("task-"));
        let mut done = false;
        for _ in 0..100 {
            if let Some(s) = state.tasks.status(&tid, 0, 0, None) {
                if !s["running"].as_bool().unwrap() {
                    assert_eq!(s["exit_code"], 0);
                    done = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(done, "异步任务 10 秒内没结束");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // v2.3.3：Open Code Review 修复项的回归测试
    // -----------------------------------------------------------------

    #[test]
    fn p0_ads_and_trailing_dot_variants_are_blocked() {
        // P0：NTFS 备用数据流 `bridge-config.json::$DATA` 在旧实现里归一化后是
        // `bridge-config.json::$data`，匹配不上黑名单，而 std::fs 在 verbatim
        // 路径下依然解析到主数据流（实机已验证）。这里按判定层回归。
        for rel in [
            "bridge-config.json",
            "BRIDGE-CONFIG.JSON",
            "bridge-config.json.",
            "bridge-config.json::$DATA",
            "bridge-config.json::$data",
            "sub/bridge-config.json.::$DATA",
            "pylon-activity.log",
            "PYLON-ACTIVITY.LOG.OLD",
        ] {
            assert!(check_read_protected(rel), "读保护漏了：{rel}");
        }
        for rel in [".git/config", ".GIT./config", "sub/.git./HEAD"] {
            assert!(check_read_protected(rel), ".git 读保护漏了：{rel}");
        }
        assert!(check_write_protected("bridge-config.json::$DATA") == WriteProtection::Forbidden);
        assert!(check_write_protected("BUILD.RS.") == WriteProtection::NeedsApproval);
        assert!(check_write_protected("a.bat::$DATA") == WriteProtection::NeedsApproval);
        // 正常文件不受影响。
        assert!(!check_read_protected("src/main.rs"));
        assert!(check_write_protected("src/main.rs") == WriteProtection::Allowed);
    }

    #[test]
    fn p0_guard_write_blocks_config_in_full_access() {
        // P0：旧 guard_write 在 FullAccess 档短路跳过 Forbidden 复检，
        // 且硬禁只比原始最后一段。三档都必须拒绝写配置。
        let dir = test_dir("bridge-p0-guardwrite");
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState::new(cfg.clone(), dir.join("bridge-config.json"), logs);
        for mode in [
            PermissionMode::AskAlways,
            PermissionMode::SmartAuto,
            PermissionMode::FullAccess,
        ] {
            let mut c = cfg.clone();
            c.permission_mode = mode;
            for rel in ["bridge-config.json", "bridge-config.json::$DATA"] {
                let r = guard_write(&state, &c, "write", rel, None);
                assert!(r.is_err(), "{mode:?} 档没拦住 {rel}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_resolve_inside_rejects_ads_rooted_and_nul() {
        // P1 #11：入口直接拒绝 `:`（ADS / 盘符）、rooted 路径与 NUL。
        let dir = test_dir("bridge-p1-resolve");
        let mut cfg = BridgeConfig::default();
        cfg.roots.insert("work".to_string(), dir.clone());
        for bad in [
            "bridge-config.json::$DATA",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "C:/Windows/win.ini",
            "a\0b",
        ] {
            assert!(
                cfg.resolve_inside("work", bad).is_err(),
                "应拒绝：{bad:?}"
            );
        }
        assert!(cfg.resolve_inside("work", "ok.txt").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_shell_and_argv0_normalization() {
        // P1 #9 / #10：`cmd.exe.`、`powershell.exe ` 与 `python.exe` 的变体
        // 必须和原名同等对待。
        for name in ["cmd", "CMD.EXE", "cmd.exe.", "cmd.exe ", "sub\\cmd.exe"] {
            assert!(is_shell_command(name), "shell 判定漏了：{name}");
        }
        assert!(!is_shell_command("cargo"));
        let cfg = BridgeConfig::default();
        for prog in ["python", "python.exe", "PYTHON.EXE.", "py", "py.exe"] {
            let argv = vec![prog.to_string(), "-c".to_string(), "print(1)".to_string()];
            let r = validate_argv_safety(&cfg, "work", ".", &argv);
            assert!(r.is_err(), "{prog} -c 没被拦下");
        }
        // git 的危险参数对 `git.exe` 同样生效。
        let argv = vec![
            "git.exe".to_string(),
            "-C".to_string(),
            "C:\\outside".to_string(),
            "status".to_string(),
        ];
        assert!(validate_argv_safety(&cfg, "work", ".", &argv).is_err());
    }

    #[test]
    fn p1_tasks_env_blacklist() {
        // P1 #8：工具级环境变量能让白名单命令加载外部代码 / 把操作指到根外。
        for bad in [
            "RUSTC_WRAPPER",
            "PYTHONPATH",
            "GIT_DIR",
            "GIT_CONFIG_GLOBAL",
            "LD_PRELOAD",
            "NODE_OPTIONS",
            "PATH",
            "HTTPS_PROXY",
            "PYTHONIOENCODING",
        ] {
            let mut m = serde_json::Map::new();
            m.insert(bad.to_string(), json!("x"));
            assert!(validate_extra_env(&m).is_err(), "env {bad} 应被拒绝");
        }
        let mut ok = serde_json::Map::new();
        ok.insert("MY_DEBUG".to_string(), json!("1"));
        assert_eq!(
            validate_extra_env(&ok).unwrap(),
            vec![("MY_DEBUG".to_string(), "1".to_string())]
        );
        // 键里带 `=` / 非法字符同样拒绝。
        let mut bad_key = serde_json::Map::new();
        bad_key.insert("A=B".to_string(), json!("1"));
        assert!(validate_extra_env(&bad_key).is_err());
    }

    #[test]
    fn p1_hmac_replay_is_rejected() {
        // P1 #6：60 秒窗口内同一签名第二次出现必须判重放。
        let dir = test_dir("bridge-p1-replay");
        let mut cfg = BridgeConfig::default();
        cfg.token = "secret".to_string();
        cfg.require_hmac = true;
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState::new(cfg, dir.join("bridge-config.json"), logs);
        let ts = now_unix_secs().to_string();
        let sig = hmac_sha256_hex(b"secret", &ts, "GET", "/api/list?root=work", b"");
        let h = headers(&[
            ("x-bridge-timestamp", &ts),
            ("x-bridge-signature", &sig),
        ]);
        assert!(
            check_remote_auth(&state, &h, "GET", "/api/list?root=work", b"", None).is_ok(),
            "首次验签应通过"
        );
        let r = check_remote_auth(&state, &h, "GET", "/api/list?root=work", b"", None);
        assert!(r.is_err(), "重放应被拒绝");
        assert_eq!(r.err().unwrap().code, 401);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_auth_failures_are_throttled() {
        // P1 #6：连续失败到阈值后返回 429（而不是无限次试 token）。
        let dir = test_dir("bridge-p1-authlimit");
        let mut cfg = BridgeConfig::default();
        cfg.token = "secret".to_string();
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState::new(cfg, dir.join("bridge-config.json"), logs);
        let bad = headers(&[("x-bridge-token", "wrong")]);
        let mut last = 401;
        for _ in 0..(AUTH_MAX_FAILURES + 1) {
            last = check_remote_auth(&state, &bad, "GET", "/health", b"", None)
                .err()
                .unwrap()
                .code;
        }
        assert_eq!(last, 429, "超过失败阈值应返回 429");
        // 正确 token 在锁定期内也拿不到 200（这是限流的目的），
        // 但换一个来源地址可以（按客户端隔离）。
        let good = headers(&[("x-bridge-token", "secret")]);
        assert!(check_remote_auth(&state, &good, "GET", "/health", b"", None).is_err());
        assert!(check_remote_auth(
            &state,
            &good,
            "GET",
            "/health",
            b"",
            Some("10.0.0.9".parse().unwrap())
        )
        .is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_approval_queue_is_bounded() {
        // P1 #2：队列满直接 QueueFull（调用方映射 429），不再无限入队。
        let dir = test_dir("bridge-p1-approvalcap");
        let mut cfg = BridgeConfig::default();
        cfg.token = "t".to_string();
        cfg.roots.insert("work".to_string(), dir.to_path_buf());
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(ServerState::new(cfg, dir.join("bridge-config.json"), logs));
        {
            let mut q = state.approvals.lock().unwrap();
            for i in 0..MAX_PENDING_APPROVALS {
                let (tx, _rx) = mpsc::channel::<bool>();
                q.push_back(ApprovalRequest {
                    id: format!("fake-{i}"),
                    kind: "run".to_string(),
                    detail: "x".to_string(),
                    diff_preview: None,
                    created_at: now_unix_secs(),
                    responder: tx,
                });
            }
        }
        let out = request_approval_timeout_with_diff(&state, "run", "git push", None, 1);
        assert_eq!(out, ApprovalOutcome::QueueFull);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_patch_preserves_crlf_and_mixed_endings() {
        // P1 #14：旧实现对 CRLF 文件做行级修补后整体变 LF；
        // 混合换行文件被整体改成 CRLF。
        let crlf = "a\r\nb\r\nc\r\n";
        let (out, n) =
            apply_line_patches(crlf, &[json!({"line": 2, "content": "B"})]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "a\r\nB\r\nc\r\n", "CRLF 必须保留");

        // 块级修补：CRLF 文件里替换一段，写回仍是 CRLF。
        let (out, _) = apply_patches(
            "a = 1\r\nb = 2\r\n",
            &[("a = 1".to_string(), "a = 10\nz = 0".to_string())],
        )
        .unwrap();
        assert_eq!(out, "a = 10\r\nz = 0\r\nb = 2\r\n");

        // 混合换行：只改动的那一行风格变化，其余行原样。
        let mixed = "one\r\ntwo\nthree\r\n";
        let (out, _) = apply_patches(
            mixed,
            &[("two".to_string(), "TWO\nEXTRA".to_string())],
        )
        .unwrap();
        assert_eq!(out, "one\r\nTWO\nEXTRA\nthree\r\n");

        // LF 文件保持 LF。
        let (out, _) = apply_patches("a\nb\n", &[("a".to_string(), "A".to_string())]).unwrap();
        assert_eq!(out, "A\nb\n");
        // 行尾无换行的文件，在末尾插入时不会把上一行和新行黏在一起。
        let (out, _) = apply_line_patches(
            "a\nb",
            &[json!({"line": 3, "op": "insert", "content": "c"})],
        )
        .unwrap();
        assert_eq!(out, "a\nb\nc");
    }

    #[test]
    fn p1_write_refuses_non_utf8_target() {
        // P2（数据损坏）：旧 handle_write 把读失败吞成空串，随后用文本覆盖
        // ——二进制文件直接被毁。现在应 415 拒绝。
        let dir = test_dir("bridge-p1-binary-write");
        std::fs::write(dir.join("blob.bin"), [0xFFu8, 0xFE, 0x00, 0x01]).unwrap();
        let state = program_state(&dir);
        let body = json!({"root":"work","path":"blob.bin","content":"text"});
        let r = handle_write(&state, &body);
        assert_eq!(r.code, 415, "实际：{}", String::from_utf8_lossy(&r.body));
        // 原字节没被破坏。
        assert_eq!(
            std::fs::read(dir.join("blob.bin")).unwrap(),
            vec![0xFFu8, 0xFE, 0x00, 0x01]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_program_post_concurrent_cas() {
        // P1 #13：两个并发 POST /api/program 都拿旧哈希，必须一个成功一个 409，
        // 不能双双 200 然后后者静默覆盖前者。
        let dir = test_dir("bridge-p1-program-cas");
        std::fs::write(dir.join("app.html"), "v0\n").unwrap();
        let state = Arc::new(program_state(&dir));
        let old_hash = sha256_hex(b"v0\n");
        let mut handles = Vec::new();
        for i in 0..2 {
            let st = Arc::clone(&state);
            let h = old_hash.clone();
            handles.push(std::thread::spawn(move || {
                let body = json!({
                    "content": format!("win-{i}\n"),
                    "expected_sha256": h,
                });
                handle_program_post(&st, &body).code
            }));
        }
        let codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let ok = codes.iter().filter(|c| **c == 200).count();
        let conflict = codes.iter().filter(|c| **c == 409).count();
        assert_eq!(
            (ok, conflict),
            (1, 1),
            "并发写入应一个成功一个冲突，实际：{codes:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2_snapshot_cannot_be_reverted_twice() {
        // P2：同一张快照连点两次「回滚」不应重复写回。
        let dir = test_dir("bridge-p2-revert-twice");
        std::fs::write(dir.join("a.txt"), "old").unwrap();
        let state = program_state(&dir);
        let body = json!({"root":"work","path":"a.txt","content":"new"});
        assert_eq!(handle_write(&state, &body).code, 200);
        let snap_id = state
            .snapshots
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|s| s.action == "write")
            .map(|s| s.id.clone())
            .unwrap();
        assert!(revert_snapshot(&state, &snap_id).is_ok());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old");
        let second = revert_snapshot(&state, &snap_id);
        assert!(second.is_err(), "第二次回滚应被拒绝");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2_list_and_changes_hide_sensitive_files() {
        // P2：列表 / 变更流不再暴露配置文件与活动日志。
        let dir = test_dir("bridge-p2-hide");
        std::fs::write(dir.join("bridge-config.json"), "{}").unwrap();
        std::fs::write(dir.join("pylon-activity.log"), "x").unwrap();
        std::fs::write(dir.join("ok.txt"), "x").unwrap();
        let state = program_state(&dir);
        let mut q = HashMap::new();
        q.insert("root".to_string(), "work".to_string());
        let r = handle_list(&state, &q);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let names: Vec<String> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"ok.txt".to_string()), "正常文件应可见：{names:?}");
        assert!(!names.iter().any(|n| n.contains("bridge-config")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("pylon-activity")), "{names:?}");

        let mut q2 = HashMap::new();
        q2.insert("root".to_string(), "work".to_string());
        q2.insert("since_ms".to_string(), "0".to_string());
        let r = handle_changes(&state, &q2);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let files: Vec<String> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(!files.iter().any(|f| f.contains("bridge-config")), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("pylon-activity")), "{files:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn p1_search_does_not_follow_symlinks() {
        // P1 #5：root 内一个指向外部的软链接，递归搜索不能穿过去。
        let dir = test_dir("bridge-p1-searchlink");
        let work = dir.join("work");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "TOKEN=SECRET\n").unwrap();
        std::os::unix::fs::symlink(&outside, work.join("jump")).unwrap();
        let (hits, _) = search_files(&work, "SECRET", false, true).unwrap();
        assert!(hits.is_empty(), "搜索穿透了软链接：{hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2_token_strength_enforced_on_save_config() {
        // P2 高位：save_config 不再接受 4 位弱 token。
        let dir = test_dir("bridge-p2-token");
        let state = program_state(&dir);
        let r = handle_ui_save_config(&state, &json!({"token": "1234"}));
        assert_eq!(r.code, 400);
        let r = handle_ui_save_config(&state, &json!({"token": "0123456789abcdef"}));
        assert_eq!(r.code, 200, "实际：{}", String::from_utf8_lossy(&r.body));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2_main_file_binding_rejects_sensitive() {
        // P1 #12：把 main_file 绑到 bridge-config.json 曾被允许，
        // 之后 GET /api/program 就能读出明文 token。绑定侧必须拦。
        let dir = test_dir("bridge-p2-mainfile");
        std::fs::write(dir.join("bridge-config.json"), r#"{"token":"SECRET"}"#).unwrap();
        std::fs::write(dir.join("app.html"), "x").unwrap();
        let state = program_state(&dir);
        let r = handle_ui_save_config(&state, &json!({"main_file": "bridge-config.json"}));
        assert_eq!(r.code, 400, "实际：{}", String::from_utf8_lossy(&r.body));
        let r = handle_ui_save_config(&state, &json!({"main_file": "app.html"}));
        assert_eq!(r.code, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

