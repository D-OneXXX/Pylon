//! v2.2.0 异步后台长任务引擎（/api/tasks/*）；v2.2.1 加并发上限、回车折叠与尾部截取。
//!
//! 每个任务：专属 Job Object（Windows，KILL_ON_JOB_CLOSE 连根清理进程树）
//! + 双管道读线程持续追加进 capped 缓冲 + watcher 线程 100ms 轮询退出码。
//! HTTP 启动立即返回 task_id + pid，不阻塞连接；远端用 offset 增量轮询输出，
//! 或用 tail_bytes 直接取末尾一段（UTF-8 边界对齐，适合只关心结尾的长任务）。
//!
//! 跨平台核心（Linux 云端可编译可测）；Job 绑定仅 Windows。
//! 停止 = 关 Job 句柄（OS 清树）+ child.kill 兜底，彻底告别手动查 PID 强杀。
//!
//! v2.2.1 并发：TaskManager 用 running/limit 计数 + RAII TaskSlot 预占，上限
//! MAX_CONCURRENT_TASKS（默认 8）；超限 spawn_task 返回 SpawnError::LimitReached，
//! 调用方映射成 429。槽在任务结束 / 被 stop / watcher 线程 panic 时自动归还。
//!
//! v2.2.1 回车折叠：进度条那种 `\r` 覆盖同一行只留最后一版（append_capped 内做），
//! stdout / stderr 都生效；`\r\n` 原样保留。
//!
//! 锁 ordering：stop() 持 tasks 锁再拿 job/child 锁；watcher 先放 child 锁
//! 再拿 tasks 锁——两条路径不形成环，无死锁。running 锁只在 try_reserve /
//! TaskSlot::drop 里单独短暂持有，从不与 tasks / child 锁嵌套。

use crate::bridge::{push_log, LastRun, ServerState, SharedLogs};
use crate::config::BridgeConfig;
use serde_json::{json, Value};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 每条流（stdout / stderr 各算）的缓冲上限；超出截断并标记 truncated。
pub const MAX_TASK_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// v2.2.1：同时运行的任务上限；超限 /api/tasks/start 返回 429 Too Many Requests。
pub const MAX_CONCURRENT_TASKS: usize = 8;
/// GET /api/tasks/list 返回最近结束的条数（运行中的全部返回）。
pub const TASK_LIST_LIMIT: usize = 20;
/// 内存里保留的任务记录总数上限（运行中 + 最近结束）。
const MAX_TASKS_KEPT: usize = 64;

// ---------------------------------------------------------------------------
// 输出缓冲：capped 追加 + offset 增量读取
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct TaskBuffers {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub out_truncated: bool,
    pub err_truncated: bool,
    /// v2.3.3：回车折叠把缓冲"回退"过（旧游标随之失效）。
    /// 增量轮询的调用方看到它就该把 offset 重置为 0 重新取，
    /// 否则会拿着一个大于缓冲长度的游标读到空内容。
    pub rewound: bool,
}

impl TaskBuffers {
    pub fn append_stdout(&mut self, bytes: &[u8]) {
        append_capped(&mut self.stdout, &mut self.out_truncated, &mut self.rewound, bytes);
    }
    pub fn append_stderr(&mut self, bytes: &[u8]) {
        append_capped(&mut self.stderr, &mut self.err_truncated, &mut self.rewound, bytes);
    }
    /// 从 offset 起的增量输出 + 最新游标（= 当前总字节数）。
    ///
    /// v2.3.3（审查报告 P2）：`offset > 全长` 时返回空串，而不是从 0 重发整段。
    /// 旧实现 `offset.min(len)` 会把整份缓冲当成"增量"再发一次，调用方拼起来
    /// 就是重复输出（回车折叠让缓冲变短时最容易触发）。
    pub fn stdout_since(&self, offset: usize) -> (String, usize) {
        if offset > self.stdout.len() {
            return (String::new(), self.stdout.len());
        }
        (
            String::from_utf8_lossy(&self.stdout[offset..]).into_owned(),
            self.stdout.len(),
        )
    }
    pub fn stderr_since(&self, offset: usize) -> (String, usize) {
        if offset > self.stderr.len() {
            return (String::new(), self.stderr.len());
        }
        (
            String::from_utf8_lossy(&self.stderr[offset..]).into_owned(),
            self.stderr.len(),
        )
    }

    /// v2.2.1：末尾 n 字节（起点对齐 UTF-8 字符边界）+ 有损解码。
    /// n 超过缓冲长度时返回全部内容。
    pub fn stdout_tail(&self, n: usize) -> String {
        tail_suffix(&self.stdout, n)
    }
    /// v2.2.1：stderr 版本的末尾截取，规则同 stdout_tail。
    pub fn stderr_tail(&self, n: usize) -> String {
        tail_suffix(&self.stderr, n)
    }
}

/// 末尾 n 字节：起点若落在 UTF-8 续字节（10xxxxxx）上就向后推进到下一个字符
/// 的起始字节，保证结果是一个完整后缀且不含替换符（缓冲本身含非法字节除外）。
fn tail_suffix(buf: &[u8], n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    if n >= buf.len() {
        return String::from_utf8_lossy(buf).into_owned();
    }
    let mut start = buf.len() - n;
    while start < buf.len() && (buf[start] & 0xC0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

fn append_capped(
    buf: &mut Vec<u8>,
    truncated: &mut bool,
    rewound: &mut bool,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    // 先做回车折叠（可能丢弃被覆盖的半行，让缓冲变短），最后再套 4MB 上限。
    // v2.3.3：截断后仍要折叠吗？不——但要注意 *truncated 时旧实现直接 return，
    // 连"缓冲是否被回退"都不再判断；这里保持"截断后只排空管道"的语义。
    if *truncated {
        return;
    }
    if fold_carriage_returns(buf, bytes) {
        *rewound = true;
    }
    if buf.len() > MAX_TASK_OUTPUT_BYTES {
        buf.truncate(MAX_TASK_OUTPUT_BYTES);
        *truncated = true;
    }
}

/// v2.2.1 回车折叠：进度条用 `\r` 回到行首反复覆盖同一行，只保留最后一版。
///
/// 规则：按孤立 `\r`（下一字节不是 `\n`）把本次 bytes 分段；被 `\r` 覆盖的
/// 那一段只保留到段内最后一个 `\n`（含），段内没有 `\n` 就把缓冲回退到最后一个
/// `\n` 之后——即丢掉被覆盖的半行；末段（后面没有 `\r` 覆盖它）完整追加。
/// `\r\n` 原样保留。跨 chunk 的悬挂 `\r`：先留在缓冲末尾，由下一次追加的首字节
/// 裁决（`\n` = 原样 CRLF；其他 = 孤立的覆盖，先回退再追加）。
///
/// v2.3.3：返回「缓冲是否被回退过」——回退会让已发出的 offset 失效，
/// 调用方要靠这个标志重置游标（否则增量轮询会读到空 / 重复内容）。
fn fold_carriage_returns(buf: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let mut rewound = false;
    // 上一次追加末尾留下的悬挂 `\r`：本次首字节不是 `\n` 就按孤立 CR 处理。
    if buf.last() == Some(&b'\r') && bytes[0] != b'\n' {
        buf.pop();
        rewound |= rewind_to_last_newline(buf);
    }
    let mut seg_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        // i + 1 == bytes.len() 时看不出是不是 CRLF，留给下一次追加裁决。
        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] != b'\n' {
            rewound |= keep_up_to_last_newline(buf, &bytes[seg_start..i]);
            seg_start = i + 1;
        }
        i += 1;
    }
    buf.extend_from_slice(&bytes[seg_start..]);
    rewound
}

/// 被 `\r` 覆盖的一段：有 `\n` 就只追加到最后一个 `\n`（含，前面同行的半行
/// 接得上），否则整段丢弃并把缓冲回退到最后一个 `\n` 之后。
fn keep_up_to_last_newline(buf: &mut Vec<u8>, seg: &[u8]) -> bool {
    match seg.iter().rposition(|&b| b == b'\n') {
        Some(pos) => {
            buf.extend_from_slice(&seg[..=pos]);
            false
        }
        None => rewind_to_last_newline(buf),
    }
}

/// 缓冲回退到最后一个 `\n` 之后；一个 `\n` 都没有就清空。返回是否真的变短了。
fn rewind_to_last_newline(buf: &mut Vec<u8>) -> bool {
    let before = buf.len();
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(pos) => buf.truncate(pos + 1),
        None => buf.clear(),
    }
    buf.len() != before
}

// ---------------------------------------------------------------------------
// 任务记录与管理器
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum TaskState {
    Running,
    Exited,
    Stopped,
}

pub struct TaskRecord {
    pub id: String,
    pub pid: u32,
    pub argv: Vec<String>,
    pub root: String,
    pub rel: String,
    pub started_at: Instant,
    pub started_unix: u64,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub buffers: Arc<Mutex<TaskBuffers>>,
    pub child: Arc<Mutex<std::process::Child>>,
    pub stop_requested: Arc<AtomicBool>,
    /// Windows：专属 Job Object。stop 时取出 drop = 关句柄，
    /// KILL_ON_JOB_CLOSE 让 OS 连根清理进程树及所有子进程。
    #[cfg(windows)]
    pub job: Mutex<Option<crate::job::JobGuard>>,
}

pub struct TaskManager {
    pub tasks: Mutex<Vec<TaskRecord>>,
    /// v2.2.1：已预占（运行中 + 正在启动）的槽数；只由 try_reserve /
    /// TaskSlot::drop 改动，锁不与其他锁嵌套。
    running: Mutex<usize>,
    /// v2.2.1：并发上限；new() 取 MAX_CONCURRENT_TASKS，测试可用 with_limit。
    limit: usize,
}

/// v2.2.1 spawn_task 的错误：并发超限可被路由层单独映射成 429，
/// 其余失败信息与 v2.2.0 一致（原 String）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// 并发槽已满（上限见 MAX_CONCURRENT_TASKS 或 TaskManager::with_limit）。
    LimitReached,
    /// 启动前的校验 / 建进程失败，内容是给用户看的中文原因。
    Failed(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::LimitReached => write!(
                f,
                "并发任务已达上限（最多 {MAX_CONCURRENT_TASKS} 个），请等运行中的任务结束或先停止"
            ),
            SpawnError::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SpawnError {}

/// 方便把 v2.2.0 遗留的 `Result<_, String>` 用 `?` 直接转进来。
impl From<String> for SpawnError {
    fn from(msg: String) -> Self {
        SpawnError::Failed(msg)
    }
}

/// 序列化成 Display 文案（字符串）：/api/program 的 run_error 字段历史上就是
/// 一句给用户看的中文原因，换成 SpawnError 后 `json!(e)` 仍然产出同样的字符串。
impl serde::Serialize for SpawnError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// v2.2.1 并发槽的 RAII 守卫：Drop 时把 TaskManager::running 减一。
/// 持 `Arc<TaskManager>` 而不是裸引用，才能 move 进 watcher 线程；
/// TaskManager 不持有 slot，不构成引用环。watcher 正常返回 / 任务被 stop /
/// 线程 panic 展开都会走到 Drop，槽一定归还。
struct TaskSlot {
    manager: Arc<TaskManager>,
}

impl Drop for TaskSlot {
    fn drop(&mut self) {
        // 计数只在 try_reserve 与这里改；即使 mutex 被 poison 也要把计数还回去，
        // 否则一次 panic 就永久少一个并发名额。
        match self.manager.running.lock() {
            Ok(mut n) => {
                let cur = *n;
                *n = cur.saturating_sub(1);
            }
            Err(poisoned) => {
                let mut n = poisoned.into_inner();
                let cur = *n;
                *n = cur.saturating_sub(1);
            }
        }
    }
}

impl TaskManager {
    /// 默认上限 MAX_CONCURRENT_TASKS（8）。
    pub fn new() -> Self {
        Self::with_limit(MAX_CONCURRENT_TASKS)
    }

    /// v2.2.1：自定义并发上限（测试里用 with_limit(0) 造“永远超限”）。
    /// 传 0 表示一律拒绝，不做下限钳制。
    pub fn with_limit(max: usize) -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            running: Mutex::new(0),
            limit: max,
        }
    }

    /// v2.2.1：预占一个并发槽；已满返回 None（= SpawnError::LimitReached）。
    /// 用关联函数而不是 `&self` 方法，是因为槽要持 `Arc<TaskManager>` 活到
    /// watcher 线程结束，必须从 `&Arc<TaskManager>` 克隆出来。
    fn try_reserve(manager: &Arc<TaskManager>) -> Option<TaskSlot> {
        {
            let mut running = match manager.running.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if *running >= manager.limit {
                return None;
            }
            *running += 1;
        }
        Some(TaskSlot {
            manager: Arc::clone(manager),
        })
    }

    /// 实时状态 + 增量输出。offset 是 stdout 游标，stderr_offset 是 stderr
    /// 游标（两条流独立推进）；next_offset / next_stderr_offset 供下次轮询。
    ///
    /// v2.2.1：tail_bytes=Some(n) 时改成“末尾 n 字节”语义——stdout_chunk /
    /// stderr_chunk 取缓冲末尾（UTF-8 边界对齐），next_offset / next_stderr_offset
    /// 直接给当前总长度（调用方不会再拿这两个游标去取增量），响应里多一个
    /// `"tail_bytes": n`；None 时完全保持 v2.2.0 的 offset 行为并回 `null`。
    pub fn status(
        &self,
        id: &str,
        offset: usize,
        stderr_offset: usize,
        tail_bytes: Option<usize>,
    ) -> Option<Value> {
        let tasks = self.tasks.lock().ok()?;
        let rec = tasks.iter().find(|t| t.id == id)?;
        let b = rec.buffers.lock().ok()?;
        let (stdout_chunk, stderr_chunk, next_offset, next_stderr_offset) = match tail_bytes {
            Some(n) => (
                b.stdout_tail(n),
                b.stderr_tail(n),
                b.stdout.len(),
                b.stderr.len(),
            ),
            None => {
                let (out, next_out) = b.stdout_since(offset);
                let (err, next_err) = b.stderr_since(stderr_offset);
                (out, err, next_out, next_err)
            }
        };
        Some(json!({
            "ok": true,
            "id": rec.id,
            "pid": rec.pid,
            "argv": rec.argv,
            "running": rec.state == TaskState::Running,
            "stopped": rec.state == TaskState::Stopped,
            "exit_code": rec.exit_code,
            "elapsed_secs": rec.started_at.elapsed().as_secs(),
            "started_unix": rec.started_unix,
            "stdout_chunk": stdout_chunk,
            "stderr_chunk": stderr_chunk,
            "next_offset": next_offset,
            "next_stderr_offset": next_stderr_offset,
            "stdout_bytes": b.stdout.len(),
            "stderr_bytes": b.stderr.len(),
            "truncated": b.out_truncated || b.err_truncated,
            // v2.3.3：缓冲被回车折叠回退过 -> 调用方应把 offset 归零重取。
            "buffer_rewound": b.rewound,
            "tail_bytes": tail_bytes,
        }))
    }

    /// 停止任务：置停止标志 + 关 Job 句柄（Windows 连根清树）+ kill 兜底。
    /// 已结束的任务也返回 true（幂等）。找不到 id 返回 false。
    pub fn stop(&self, id: &str) -> bool {
        let tasks = match self.tasks.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(rec) = tasks.iter().find(|t| t.id == id) else {
            return false;
        };
        rec.stop_requested.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        {
            // 取出 JobGuard 并 drop：CloseHandle 触发 KILL_ON_JOB_CLOSE，
            // 操作系统把 Job 里的进程树（含孙进程）一锅端。
            if let Ok(mut j) = rec.job.lock() {
                let _ = j.take();
            }
        }
        if let Ok(mut c) = rec.child.lock() {
            let _ = c.kill();
        }
        true
    }

    /// 运行中全部 + 最近结束 TASK_LIST_LIMIT 条（新在前）。
    pub fn list(&self) -> Vec<Value> {
        let tasks = match self.tasks.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut running: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .collect();
        let mut finished: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|t| t.state != TaskState::Running)
            .collect();
        running.sort_by(|a, b| b.started_unix.cmp(&a.started_unix));
        finished.sort_by(|a, b| b.started_unix.cmp(&a.started_unix));
        finished.truncate(TASK_LIST_LIMIT);
        running
            .into_iter()
            .chain(finished)
            .map(|t| {
                json!({
                    "id": t.id,
                    "pid": t.pid,
                    "argv": t.argv,
                    "root": t.root,
                    "path": t.rel,
                    "running": t.state == TaskState::Running,
                    "stopped": t.state == TaskState::Stopped,
                    "exit_code": t.exit_code,
                    "elapsed_secs": t.started_at.elapsed().as_secs(),
                    "started_unix": t.started_unix,
                })
            })
            .collect()
    }

    /// 入队新记录；超上限时挤掉最旧的已结束记录（运行中的不动）。
    fn push_record(&self, rec: TaskRecord) {
        let mut tasks = match self.tasks.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        tasks.push(rec);
        while tasks.len() > MAX_TASKS_KEPT {
            match tasks.iter().position(|t| t.state != TaskState::Running) {
                Some(pos) => {
                    tasks.remove(pos);
                }
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 启动与 watcher
// ---------------------------------------------------------------------------

/// 持续读管道直到 EOF，追加进共享缓冲（超上限截断标记）。
fn pipe_reader<R: Read + Send + 'static>(
    mut pipe: R,
    buffers: Arc<Mutex<TaskBuffers>>,
    is_stderr: bool,
) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut b) = buffers.lock() {
                    if is_stderr {
                        b.append_stderr(&chunk[..n]);
                    } else {
                        b.append_stdout(&chunk[..n]);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 后台启动一个任务：复用 /api/run 的 cwd 白名单校验与 UTF-8 环境变量注入，
/// 立即返回 (task_id, pid)，不阻塞 HTTP 连接。
/// 鉴权（白名单 / 消毒 / 三档门控）由调用方（handle_tasks_start）先做。
///
/// v2.2.1：先预占并发槽，满了直接 `SpawnError::LimitReached`（路由层映射 429）；
/// 其余失败统一 `SpawnError::Failed(原因)`。任何早期失败都会让槽随 Drop 自动归还。
pub fn spawn_task(
    state: &ServerState,
    cfg: &BridgeConfig,
    root: &str,
    rel: &str,
    argv: &[String],
    extra_env: &[(String, String)],
) -> Result<(String, u32), SpawnError> {
    let slot = match TaskManager::try_reserve(&state.tasks) {
        Some(slot) => slot,
        None => return Err(SpawnError::LimitReached),
    };
    // slot 之后在 spawn_task_reserved 末尾移交给 watcher 线程；中途任何 `?`
    // 提前返回都会 drop 它，并发计数立刻回退。
    spawn_task_reserved(state, cfg, root, rel, argv, extra_env, slot)
}

/// 已持槽的启动主体：与 v2.2.0 的 spawn_task 行为一致，只是错误类型换成
/// SpawnError、并把 TaskSlot 交给 watcher 线程托管。
fn spawn_task_reserved(
    state: &ServerState,
    cfg: &BridgeConfig,
    root: &str,
    rel: &str,
    argv: &[String],
    extra_env: &[(String, String)],
    slot: TaskSlot,
) -> Result<(String, u32), SpawnError> {
    if argv.is_empty() {
        return Err(SpawnError::Failed("argv 不能为空".to_string()));
    }
    let cwd = cfg.resolve_inside(root, rel)?;
    if !cwd.is_dir() {
        return Err(SpawnError::Failed("cwd 不是已存在的目录".to_string()));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 与 run_operation 同一套 UTF-8 注入（中文 Windows 子进程不乱码）。
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .env("LANG", "C.UTF-8");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Windows 下子进程不弹黑框（与 run_operation 一致）。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| SpawnError::Failed(format!("启动命令失败：{e}")))?;
    let pid = child.id();
    // 专属 Job Object：stop / Pylon 退出时 OS 连根清理进程树。
    #[cfg(windows)]
    let job = crate::job::JobGuard::new().map(|j| {
        j.assign(&child);
        j
    });
    let out_pipe = child
        .stdout
        .take()
        .ok_or_else(|| SpawnError::Failed("拿不到 stdout 管道".to_string()))?;
    let err_pipe = child
        .stderr
        .take()
        .ok_or_else(|| SpawnError::Failed("拿不到 stderr 管道".to_string()))?;
    let buffers = Arc::new(Mutex::new(TaskBuffers::default()));
    // v2.3.3（审查报告 P1 #7）：读线程的 JoinHandle 交给 watcher，进程退出后
    // 等管道排空再收尾。旧实现直接 detach，`try_wait` 一返回就写"任务结束"，
    // 最后几行输出（编译错误、测试摘要）还没被读进缓冲就丢了尾部。
    let out_handle = {
        let b = Arc::clone(&buffers);
        std::thread::spawn(move || pipe_reader(out_pipe, b, false))
    };
    let err_handle = {
        let b = Arc::clone(&buffers);
        std::thread::spawn(move || pipe_reader(err_pipe, b, true))
    };
    let id = format!(
        "task-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let started_unix = now_unix_secs();
    let child = Arc::new(Mutex::new(child));
    let stop_requested = Arc::new(AtomicBool::new(false));
    // 最近运行摘要：异步任务先记启动，watcher 结束时补 exit_code。
    if let Ok(mut lr) = state.last_run.lock() {
        *lr = Some(LastRun {
            argv: argv.to_vec(),
            exit_code: None,
            timed_out: false,
            at_ms: started_unix * 1000,
            task_id: Some(id.clone()),
            async_run: true,
        });
    }
    let rec = TaskRecord {
        id: id.clone(),
        pid,
        argv: argv.to_vec(),
        root: root.to_string(),
        rel: rel.to_string(),
        started_at: Instant::now(),
        started_unix,
        state: TaskState::Running,
        exit_code: None,
        buffers: Arc::clone(&buffers),
        child: Arc::clone(&child),
        stop_requested: Arc::clone(&stop_requested),
        #[cfg(windows)]
        job: Mutex::new(job),
    };
    state.tasks.push_record(rec);
    // watcher：100ms 轮询退出码；stop_requested 时 kill + wait 置 Stopped。
    let manager = Arc::clone(&state.tasks);
    let last_run = Arc::clone(&state.last_run);
    let logs = Arc::clone(&state.logs);
    let id2 = id.clone();
    let argv2 = argv.to_vec();
    std::thread::spawn(move || {
        // slot 随 watcher 线程一起活到任务结束：正常退出 / 被 stop / panic
        // 展开都会 Drop 归还并发槽。
        watch_task(
            manager,
            id2,
            argv2,
            child,
            stop_requested,
            last_run,
            logs,
            slot,
            out_handle,
            err_handle,
        );
    });
    push_log(
        &state.logs,
        format!("后台任务启动 {}（{id}，pid {pid}）", argv.join(" ")),
    );
    Ok((id, pid))
}

/// v2.3.3：poison 不再 unwrap（watcher 里 panic 会让整条任务链断掉）。
fn lock_child_guard(
    c: &Arc<Mutex<std::process::Child>>,
) -> std::sync::MutexGuard<'_, std::process::Child> {
    match c.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// watcher 线程主体：等任务结束（或被 stop），更新记录 / last_run / 动态。
/// `_slot` 只用来托管并发槽的 RAII 生命周期，本函数不读它。
///
/// v2.3.3（审查报告 P1 #7）两处收尾修正：
/// 1. 进程退出后先**关掉专属 Job Object**（`job.take()` 即 CloseHandle，
///    KILL_ON_JOB_CLOSE 触发，残留的孙进程被系统清掉）。旧实现只有在
///    `/api/tasks/stop` 或记录被挤出 64 条时才 drop guard，正常退出路径
///    直接把进程树留在系统里，而且孙进程还攥着管道写端。
/// 2. 再**等两条读线程把管道排空**（最多 2 秒）——这样最后几行输出不会丢。
///    孙进程若把写端传给了更远的后代，超时后放弃等待（detach），不会卡死 watcher。
#[allow(clippy::too_many_arguments)]
fn watch_task(
    manager: Arc<TaskManager>,
    id: String,
    argv: Vec<String>,
    child: Arc<Mutex<std::process::Child>>,
    stop_requested: Arc<AtomicBool>,
    last_run: Arc<Mutex<Option<LastRun>>>,
    logs: SharedLogs,
    _slot: TaskSlot,
    out_handle: std::thread::JoinHandle<()>,
    err_handle: std::thread::JoinHandle<()>,
) {
    let lock_child = lock_child_guard;
    let (state, exit_code) = loop {
        if stop_requested.load(Ordering::SeqCst) {
            let mut c = lock_child(&child);
            let _ = c.kill();
            let _ = c.wait();
            break (TaskState::Stopped, None);
        }
        // try_wait 非阻塞；锁在块末释放，绝不持 child 锁再拿 tasks 锁。
        let status = {
            let mut c = lock_child(&child);
            c.try_wait()
        };
        match status {
            Ok(Some(st)) => break (TaskState::Exited, st.code()),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break (TaskState::Exited, None),
        }
    };
    // 1) 关 Job：残留进程树连根清理（同时释放它们占用的管道写端）。
    #[cfg(windows)]
    {
        if let Ok(tasks) = manager.tasks.lock() {
            if let Some(rec) = tasks.iter().find(|t| t.id == id) {
                if let Ok(mut j) = rec.job.lock() {
                    let _ = j.take();
                }
            }
        }
    }
    // 2) 排空管道：等读线程结束，最多 2 秒。
    drain_pipe_reader(out_handle);
    drain_pipe_reader(err_handle);
    if let Ok(mut tasks) = manager.tasks.lock() {
        if let Some(rec) = tasks.iter_mut().find(|t| t.id == id) {
            rec.state = state;
            rec.exit_code = exit_code;
        }
    }
    if let Ok(mut lr) = last_run.lock() {
        if let Some(r) = lr.as_mut() {
            if r.task_id.as_deref() == Some(id.as_str()) {
                r.exit_code = exit_code;
            }
        }
    }
    let msg = match state {
        TaskState::Stopped => format!("后台任务已停止 {}（{id}）", argv.join(" ")),
        _ => format!(
            "后台任务结束 {}（{id}）-> exit {}",
            argv.join(" "),
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    };
    push_log(&logs, msg);
}

/// v2.3.3：等读线程结束（最多 2 秒），超时则 detach。
/// 用 `is_finished()` 轮询而不是直接 `join()`：孙进程可能把管道写端传给更远的
/// 后代，硬 join 会把 watcher 永久挂住、连带并发槽再也不归还。
fn drain_pipe_reader(handle: std::thread::JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        let _ = handle.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_offset_incremental() {
        // 纯缓冲逻辑：offset 增量读取、next_offset 推进、重复 offset 幂等。
        let mut b = TaskBuffers::default();
        b.append_stdout(b"hello ");
        b.append_stdout(b"world");
        let (chunk, next) = b.stdout_since(0);
        assert_eq!(chunk, "hello world");
        assert_eq!(next, 11);
        b.append_stdout(b"!");
        let (chunk, next) = b.stdout_since(next);
        assert_eq!(chunk, "!");
        assert_eq!(next, 12);
        let (empty, same) = b.stdout_since(next);
        assert_eq!(empty, "");
        assert_eq!(same, 12);
        // 越界 offset 钳到末尾，不 panic。
        let (empty, same) = b.stdout_since(999);
        assert_eq!(empty, "");
        assert_eq!(same, 12);
    }

    #[test]
    fn buffers_cap_truncates() {
        let mut b = TaskBuffers::default();
        b.append_stdout(&vec![b'x'; MAX_TASK_OUTPUT_BYTES + 10]);
        assert!(b.out_truncated);
        assert_eq!(b.stdout.len(), MAX_TASK_OUTPUT_BYTES);
        // 截断后继续追加不再增长（只排空管道，不存字节）。
        b.append_stdout(b"more");
        assert_eq!(b.stdout.len(), MAX_TASK_OUTPUT_BYTES);
    }

    #[test]
    fn buffers_fold_cr_progress() {
        // 进度条用 \r 反复覆盖同一行：只留最后一版；stderr 同样折叠。
        let mut b = TaskBuffers::default();
        b.append_stdout(b"epoch 1 loss=1\repoch 2 loss=.5\repoch 3 loss=.1\nError: boom\n");
        assert_eq!(
            String::from_utf8_lossy(&b.stdout),
            "epoch 3 loss=.1\nError: boom\n"
        );
        b.append_stderr(b"1/3\r2/3\r3/3\n");
        assert_eq!(String::from_utf8_lossy(&b.stderr), "3/3\n");
    }

    #[test]
    fn buffers_crlf_preserved() {
        // \r\n 是正常换行，不能被当成覆盖。
        let mut b = TaskBuffers::default();
        b.append_stdout(b"a\r\nb");
        assert_eq!(String::from_utf8_lossy(&b.stdout), "a\r\nb");
    }

    #[test]
    fn buffers_fold_cr_across_chunks() {
        // 悬挂 \r 落在 chunk 末尾：由下一次追加的首字节裁决。
        let mut b = TaskBuffers::default();
        b.append_stdout(b"epoch 1 loss=1\r");
        b.append_stdout(b"epoch 2 loss=.5\r");
        b.append_stdout(b"epoch 3 loss=.1\n");
        assert_eq!(String::from_utf8_lossy(&b.stdout), "epoch 3 loss=.1\n");
        // 跨 chunk 切开的 CRLF 不能丢。
        b.append_stdout(b"x\r");
        b.append_stdout(b"\ny");
        assert_eq!(String::from_utf8_lossy(&b.stdout), "epoch 3 loss=.1\nx\r\ny");
    }

    #[test]
    fn buffers_tail_utf8_boundary() {
        // "前" 3 字节 + 'a' 1 + 🚀 4 + 'b' 1 = 9 字节。
        let mut b = TaskBuffers::default();
        b.append_stdout("前a🚀b".as_bytes());
        // n 大于缓冲 = 全部。
        let full = b.stdout_tail(usize::MAX);
        assert_eq!(full, "前a🚀b");
        // n=2：起点落在 🚀 的续字节里，推进到 'b' 的字符边界。
        let cut = b.stdout_tail(2);
        assert_eq!(cut, "b");
        assert!(!cut.contains('\u{FFFD}'), "截断点必须对齐字符边界");
        assert!(full.ends_with(cut.as_str()), "截取结果必须是末尾后缀");
        // n=5：起点正好是 🚀 的起始字节，整颗 emoji 保留。
        assert_eq!(b.stdout_tail(5), "🚀b");
        assert_eq!(b.stdout_tail(0), "");
        // stderr 同一套规则：错(3) + 🚀(4) + !(1)。
        b.append_stderr("错🚀!".as_bytes());
        assert_eq!(b.stderr_tail(2), "!");
        assert_eq!(b.stderr_tail(5), "🚀!");
        assert_eq!(b.stderr_tail(999), "错🚀!");
    }

    #[test]
    fn manager_limit_reservation() {
        let mgr = Arc::new(TaskManager::with_limit(2));
        let s1 = TaskManager::try_reserve(&mgr);
        let s2 = TaskManager::try_reserve(&mgr);
        assert!(s1.is_some(), "第 1 次预占应成功");
        assert!(s2.is_some(), "第 2 次预占应成功");
        // 第 3 次满员：即 spawn_task 的 SpawnError::LimitReached。
        assert!(
            TaskManager::try_reserve(&mgr).is_none(),
            "超过 limit 必须拒绝"
        );
        // 归还一个立刻能再占一个（RAII Drop 减计数）。
        drop(s1);
        assert!(
            TaskManager::try_reserve(&mgr).is_some(),
            "归还后应能再占"
        );
        drop(s2);
        // 上面那个临时槽随语句结束已归还，计数回到 0。
        assert_eq!(*mgr.running.lock().unwrap(), 0);
        // limit=0 永远拒绝：bridge 用它构造 429 场景。
        let zero = Arc::new(TaskManager::with_limit(0));
        assert!(TaskManager::try_reserve(&zero).is_none());
        // 默认上限对齐常量。
        assert_eq!(TaskManager::new().limit, MAX_CONCURRENT_TASKS);
    }
}