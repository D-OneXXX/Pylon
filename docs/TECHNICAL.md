# Pylon 技术文档

> 版本：v2.3.3（2026-11-15）
> 品牌名：**Pylon**（2026-11-09 定名，桥塔之意，呼应图标里的两座白塔）。包名与 exe 文件名仍为 `muse-local-bridge` / `muse-local-bridge.exe`（改显示名不用改包名）。

## 0. 一句话

Pylon 是跑在用户 Windows 电脑上的一个**本地白名单服务 + 异步任务引擎 + 系统托盘 + Tauri 2 内嵌单文件全宽单页窗口（无边框一体化顶栏、关窗即销毁）**：云端的 Muse（Astro）经 Tailscale 私网连进来，在用户指定的文件夹里读写文件、跑白名单命令与后台长任务、做行级修补与代码检索，成果直接落在本地。程序驻留托盘，开关在用户手里。

设计上它是 Muse 的**专用工具**，不是多 agent 平台：权限控制在边界上（token 鉴权、白名单命令、文件夹锁、不经过 shell、超时与输出上限），在用户指定的文件夹内 Astro 有完全控制权。

v2.3.3（本次）：按独立代码审查（Open Code Review）修完 1 个 P0 + 14 个 P1 + 15 项 P2/P3（报告原文已随开源清理删除，修复内容已全部落进代码与本文）。① **P0 NTFS `::$DATA` 绕过**：`normalize_rel_segments` 一处实现「切 `:stream` + 剥尾点/尾空格 + 转小写」，读保护 / 写保护 / 回收站 / 重命名 / 建目录全部复用；`resolve_inside` 直接拒绝含 `:`、NUL、以根开头的 path；解析后再用 `is_sensitive_target` 按真实文件名复检（判定层与 I/O 层对齐）。② **P1 HTTP 层**：读 20s / 写 30s 超时、请求行 8KB、单头 8KB、头总量 32KB、头条数 100、整请求 60s 总时限、并发连接 64（超 503）、`chunked` 明确 411、body 按需增长、支持 `Expect: 100-continue`。③ **认证**：token 改常量时间比较、按来源 IP 失败限流（10 次 / 60s → 锁 60s）、HMAC 签名 60s 窗口内去重（真防重放）、时间差 i128、不再回显服务端时间。④ **审批**：队列上限 16（满 429）、快照 `reverted` 幂等、快照总字节上限 32MB。⑤ **写入正确性**：`apply_patches` 逐字节记录换行风格、`apply_line_patches` 每行连换行符一起存（CRLF 不再被改成 LF、混合换行不再被拉平）；`handle_write` 拒绝非 UTF-8 目标（不再毁二进制）；`POST /api/program` 的读-校验-写进锁内做 CAS、409 不回显全文。⑥ **路径解析**：`resolve_via_existing_ancestor` 全失败改 `Err`（原 fail-open）；审批后 `revalidate_target` + 内容比对。⑦ **配置与前端**：`save_config` 原子写 + 保存互斥、配置查找不再 CWD 优先（`--config` / `PYLON_CONFIG` / exe 同级；仅 debug 看 CWD）、`csp` 由 `null` 改为白名单（内联脚本/样式外置为 `ui/app.js` / `ui/app.css` / `ui/boot-error-banner.js`）。测试数：`cargo test` 102/102（101 单元 + 1 图标；新增 14 项安全回归 + 3 项真连 socket 的 HTTP 端到端）。

v2.3.2：界面 JS 初始化崩溃紧急修复。根因是 `ui/index.html` 第 1113 行残留 `$("source-close")` 的转义反斜杠，整个 `<script>` 解析失败（`SyntaxError: Invalid or unexpected token`），事件绑定与全部数据轮询一行都没跑——顶栏数据卡死在静态占位符、所有按钮点按无反应。修复：① 删掉 stray 反斜杠，`node --check` 双脚本全过；② 全部 18 处顶层直接绑定改走安全绑定 `on(id, ev, fn)`（元素不存在时静默跳过）；③ 窗口控制（—/□/×/⏻ 退出）移到脚本最顶部独立 `try/catch` 最先绑定；④ 拖拽区收窄：不再挂整个 `<header>`，只挂无按钮的 brand 与空白占位 `.drag-spacer`；⑤ 独立小脚本先于主脚本注册 `window.onerror` / `onunhandledrejection`，出错时顶部显示红色错误横幅，杜绝静默假死；⑥ `invoke` 未就绪时 1 秒内重试 10 次，就绪后重载再跑。Rust 侧核查结论：`ui_request`（`body: Option<serde_json::Value>`）与 `dispatch_request_internal` 链路无问题，单测已覆盖 `/ui/state` 等内部调用，无需改动。测试数：云端 Linux `cargo check` 零警告、`cargo test` 82/82（源码 85 项；真机 Windows `cargo check` 零警告、`cargo test` 84/84）。
v2.3.1（本次）：顶栏单层融合 + 4 项底层盲区补全。① **顶栏单层一体化**：彻底砍掉 32px 独立假系统条（双图标/双标题套娃的根源），全页面顶部只剩唯一一行 60px Header；整行空白处挂 `data-tauri-drag-region`（拖拽窗口、双击最大化/还原）；左侧 38×38 Cyber-Cat + Pylon + v2.3.1 徽章 + `● 运行中 · 监听 0.0.0.0:8765` 状态胶囊；右侧同一水平线：三档权限分段切换器、[主题] [设置] 红底白字 [熔断]、竖线分隔、[ — ] [ □ ] [ × ]（销毁窗口缩回托盘，悬停提示“隐藏到托盘（释放内存）”）与暗红边框 [⏻ 退出]（彻底退出进程）；啰嗦的 [最小化到托盘] 按钮已删（与 × 重复）。② **关窗几何恢复**：销毁前把 `outer_position` / `inner_size` / `is_maximized` 存进进程级静态 `LAST_WINDOW_GEOMETRY`，托盘重建时恢复，不再每次重置回屏幕正中央。③ **第二实例唤醒**：重复双击 exe 时，第二实例触发命名 Event `Local\PylonShowWindowEvent` 再退出，主实例托盘线程监听到信号立即重建并置顶窗口。④ **切断 HTTP 偷调 /ui/\***：`dispatch_request_internal`（Tauri `invoke` 内部调用）是唯一能进 `/ui/*` 的通道，HTTP Socket（8765）收到 `/ui/*` 一律 403；新增单测 `ui_http_socket_blocked_but_internal_ok`（HTTP POST `/ui/save-config` 403、内部调用 200）。⑤ **后台任务日志弹窗**：任务表每行新增 [查看输出]，弹窗深色等宽终端框每 1 秒拉 `GET /api/tasks/status?id=&tail_bytes=16384` 实时刷 stdout/stderr 尾部。⑥ **文档**：§4 `GET /` 修正为小 JSON 状态、§14 修正为 v2.0.1 已实现的“回滚前自动拍保护快照，支持二次反悔”。测试数：云端 Linux `cargo check` 零警告、`cargo test` 82/82（源码 85 项；真机 Windows `cargo check` 零警告、`cargo test` 84/84）。

v2.3.0：Cyber-Cat 新图标 + 无边框一体化顶栏。① **全新独立品牌图标**：奶油蜜桃色赛博猫（头顶双桥塔天线、胸前桥塔徽章、双爪环抱发光青绿数字桥、无线信号尾巴、珊瑚粉渐变 Squircle）；1024 透明源图经 Squircle 透明裁切生成 `assets/icon-v2-256.png` 与含 16/24/32/48/256 五档的 `assets/icon.ico`（`icons/icon.ico` / `assets/icon-v2.ico` 同步）；`build.rs` 经 `tauri-build` 内置 tauri-winres 把 `assets/icon.ico` 嵌进 EXE（任务栏 / Alt+Tab / 资源管理器），托盘沿用 `assets/icon.ico`，UI 用同一份 base64（CSS 变量单份复用）。② **无边框一体化顶栏**：`WebviewWindowBuilder.decorations(false)` 去掉 Windows 原生标题栏，保留 `resizable(true)` 边缘缩放与 `shadow(true)` 系统阴影；顶部 32px 自绘系统条（18px 迷你图标 + Pylon + `data-tauri-drag-region` 拖拽/双击最大化 + `— / □ / ×` 控制，关闭按钮 hover 红底白字），主控制栏无缝衔接；新增 `window_minimize` / `window_toggle_maximize` / `window_close_destroy` / `window_start_drag` 命令；`×` 与 [最小化到托盘] 仍走 destroy + `EmptyWorkingSet`，`ExitRequested` 拦截非主动退出；Light 主题同步。③ **资产回归**：新增 `tests/icon_assets.rs` 校验 ICO 五档与 256 PNG。测试数：Windows 83 项、Linux 81 项（源码 84 项）。

v2.2.1：全宽单页 UI + 线程与安全补全。① **UI 改全宽单页**：删除侧栏，`max-width:1120px` 居中容器；顶栏三档权限分段快捷切换（`full_access` 橙色警示）、审批置顶横幅（仅在有挂起时显示）、整行全宽热力图（53 列流体网格，不出现横向滚动条）、程序胶囊绑定 / 查看源码 / 立即运行、严格单行动态；所有数据仍走 `invoke('ui_request')`。② **托盘建窗回主线程**：`TrayBridge::request_show()` 统一经 `AppHandle::run_on_main_thread` 调度，托盘线程不再直接建 WebView；`ui_request` 改 `async fn`，克隆 `Arc<ServerState>` 后交 `tauri::async_runtime::spawn_blocking`，不再阻塞 Tauri 主线程。③ **任务引擎增强**：并发上限 `MAX_CONCURRENT_TASKS = 8`（超出 `POST /api/tasks/start` 回 429）、`GET /api/tasks/status` 新增 `tail_bytes`（末尾 N 字节，UTF-8 边界对齐）、`TaskBuffers` 回车进度折叠（孤立 `\r` 覆盖的半行只保留最终行，`\r\n` 原样保留）。④ **安全边界回归与加固**：`rename` 的 `to` 写入审批门控回归、轮转日志 `pylon-activity.log.old` 禁读回归、`resolve_inside` 对最深已存在祖先 canonicalize（拦 junction / 软链接父目录逃逸）、任务并发上限。测试数：Windows 82 项、Linux 80 项（源码新增 13 项，每端实跑 12 项；unix symlink 与 windows junction 各 1 项平台专属）。

v2.0.0 是界面换代：**网页面板彻底删除**（`src/ui.html` 与 `GET /` 的 HTML 返回、`/ui/*` 面板接口全部移除，`GET /` 改回小 JSON 状态），界面改成 egui 单页窗口（`src/ui_egui.rs`，eframe 0.37.0）：状态条 / KPI 四宫格 / 审批拦截横幅（无挂起时隐藏）/ 操作热力图 / 实时操作动态 / 底部，配置区砍掉、改成右上角 `[⚙ 设置]` 弹窗。窗口默认 760×860，暖白纸底 #FAFAF8、青绿 #10B981 唯一点缀色；中文用系统全量微软雅黑（msyh.ttc + msyhbd.ttc）；图标为 AI 猫脸版。内存约 97MB（空窗口探针证实 eframe 下限约 98MB，60MB 到不了，用户已拍板就这样用）。云端 `cargo test` 65/65，真机 66/66（含 Windows 专属 Job 测试）。

v2.1.3：只保留 Tailscale 模式（用户 2026-11-13 拍板，为保稳定）：删除 Cloudflare Tunnel / WS 反向 / iroh P2P 三种模式及 `src/ws.rs` / `src/relay.rs` / `src/tls.rs` 与两个中继脚本（`docs/cloud_ws_relay.py` / `docs/cloud_relay_poll.py`）；设置弹窗删网络模式单选与 ws_url / cf_token / iroh 节点 ID 控件，网络卡片固定显示 Tailscale；`network.rs` 的 NetworkManager 只剩 HTTP 监听热切换（端口改了 0.5 秒内生效）；删 `sha1` / `rustls` / `webpki-roots` 依赖，base64 编解码改走 `base64` crate（`/api/read_base64` / `/api/write_base64` 行为不变）；`dispatch_request_preauthed` 只剩单测在用，标 `#[cfg(test)]`。云端 `cargo check` 零警告、`cargo test` 60/60，真机 `cargo check` 零警告、`cargo test` 61/61（含 Windows 专属 Job 测试）。

v2.1.2：修复 v2.1.1 真机问题——窗口上半截透明穿透（根因：winit-software 局部脏矩形重绘与 Win32 ShowWindow 冲突，未重绘区被 DWM 当透明通道；恢复 femtovg 整帧渲染 + 全窗实心背景 Rectangle + 唤醒后 InvalidateRect 全窗重绘）；热力图斜边（根因：Slint 的 `/` 是浮点除法，`(i/7)*11px` 让同一列不同行 x 错开，改 `Math.floor(i/7)`；星期标签宽锁 24px；卡片锁高 170px；外层 padding 16px 防左裁切）；设置弹窗补主程序单文件（`main_file`，相对 work 根或绝对路径）与 iroh 节点 ID 展示+复制、ws_url 格式校验（必须 ws:// 或 wss://）；WS 状态进动态日志（「WS 已连接 -> <ws_url>」/ 断线重连提示）+ 网络卡片详情行；落地「文件即是程序」：`GET/POST /api/program`（整文件覆写 + 行级热修补，见 docs/FILE_AS_PROGRAM.md）与 `GET /api/changes`（增量变更感知，跳过 .git/target/node_modules/.pylon_trash，最多 100 条）；配置/日志收敛 exe 同级目录、无配置时内存默认+随机 Token 启动（不往工作区写配置文件）。云端 `cargo check` 零警告、`cargo test` 73/73；WS 反向端到端冒烟通过（中继握手/鉴权帧 version 2.1.2/重连/热修补全通），真机待验。

v2.1.1：修复 v2.1.0 真机问题——托盘最小化失效（`window.run()` 改 `show()` + `run_event_loop_until_quit()`，点 X/最小化按钮统一 `hide()` + Win32 `SW_HIDE`）；窗口/任务栏图标缺失（Slint `icon: @image-url("../assets/icon-v2-256.png")` + 顶栏 24×24 猫脸 Logo，build.rs 图标嵌入成功/失败均打印明确信息）；热力图全灰（根因：Slint 索引访问 `heatmap[i]` 不随模型替换更新，Rust 逻辑经单测验证无误；改 `for day[i] in heatmap` 直接迭代 + 绝对定位画 53×7 网格，9px 格 + 2px 间距总宽 581px，补月份/星期标签）；动态行高 34px + 回滚按钮预留区、活动卡 `vertical-stretch: 1` 防底栏截断；危险按钮自定义（[熔断] 红底白字、[退出] 红字）；设置弹窗选项加边框/选中底色/选中圆点，[保存并热重载] 青绿实底白字；rfd 目录选择器改独立线程（阻塞主事件循环会假死）；默认走软件渲染（`BackendSelector` 选 `winit-software`，不唤醒 RTX 独显）；审批横幅带队列总数「共 N 项，当前第 1 项」。云端 `cargo check` 零警告、`cargo test` 67/67，真机待验。

v2.1.0：GUI 从 eframe/egui 彻底迁至 **Slint**（纯 Rust 原生声明式 UI，`build.rs` 用 `slint_build::compile_with_config` 编译 `ui/main.slint`，样式 `fluent-light`；`backend-winit` + `renderer-femtovg` + `renderer-software`，无独显/老核显/RDP 下回退软渲染，不强制唤醒 RTX 独显；目标常驻内存 15–25MB，待真机实测）。界面仍是单页卡片式，`Timer` 每 300ms 从 `Arc<ServerState>` 同步；关闭窗口拦截为隐藏到托盘，托盘经 `invoke_from_event_loop` + Win32 ShowWindow/SetForegroundWindow 唤醒。另修复：重启后动态历史丢失（`restore_log_file` 改读主文件 + `.old`，启动日志带恢复条数）；补敏感文件读保护（`read`/`search`/`read_base64` 禁读 `bridge-config.json` 与 `pylon-activity.log`）。云端 `cargo check` 零警告、`cargo test` 66/66，真机待验。

v2.0.1：界面重构 + 底层隐患摸底修复——恢复系统全量中文字体（废除 1210 字子集字体）、设置弹窗、动态列表重画（滚动条按需出现、底部留白、行对齐、超长截断+悬停）、热力图补月份/星期标签与图例、full_access 警示橙；5 处隐患：写保护大小写/尾部点号绕过、子进程 UTF-8 环境变量、回滚前自动拍保护快照、托盘唤醒存原生 HWND 强制拉起；listener_loop 已确认无问题（v0.9.1 已修，独立线程 + set_nonblocking(false)）。（原报告已随开源清理删除，修复内容已全部落进代码与本文。）

v0.8 的改动最大的一点：**GPUI 被彻底砍掉了**（它在 Linux 云端因 `xattr` 编不过，没法完整验证）。界面改成「系统托盘 + 内置单文件 Web 面板」，整包在 Linux 和 Windows 下都能 `cargo check` 零警告通过。

v0.9.0 把 v0.8 留下的三处尾巴一次性清掉：**三档权限**（对齐 Codex：请求批准 / 帮我批准 / 完全访问权限，顶栏 Popover 切换，立即生效）；**四模网络全实装**（`ws_reverse` 的纯标准库 WebSocket 客户端与 `iroh_p2p` 的 `relay_poll` 长轮询中继都写完了，不再是骨架）；**配置热重载**（`NetworkManager` 动态切换，保存后 0.5 秒内生效，彻底删掉“保存后要重启”的提示）。新增依赖：**零**（WS 客户端与中继客户端都是纯 std 实现）。

v1.0.0 把 v0.9.0 留下的五处短板一次性清掉：**出站 TLS**（`src/tls.rs`：rustls + webpki-roots，`wss://` 与 `https://`，Sec-WebSocket-Accept 严格校验）；**写前快照与一键回滚**（`VecDeque<Snapshot>` 上限 30 条，`/ui/snapshots` + `/ui/revert`，面板动态里点 ↶ 回滚）；**审批红绿 Diff 预览**（Unified Diff，`-` 暗红底 / `+` 暗绿底）+ **托盘气泡通知** + **单实例锁**；**文件操作闭环**（`/api/trash` 软删除进 `.pylon_trash/`、`/api/rename`、`/api/read_base64` 与 `/api/write_base64` 上限 10MB）；**动态日志落盘**（`pylon-activity.log`，超 5MB 轮转 `.old`，启动读回末尾 50 条）。附加：**启动不再挂 CMD 黑窗口**（`windows_subsystem = "windows"` + 子进程 `CREATE_NO_WINDOW` + `start.bat` 编译后后台脱钩启动自动关窗）。云端 `cargo test` 59/59，真机 60/60。

v1.0.1 是 v1.0.0 的验收补丁：**所有响应带 `Cache-Control: no-store`**（面板不再缓存旧页面）+ **面板 JS 在 401 时自动刷新一次拿新密钥**（只刷一次防死循环）+ **`append_log_file_to` 加静态写锁**（多线程并发追加不再把两行字串在一起）。云端 `cargo test` 61/61，真机 62/62。

**v1.0.1 重启后验证（2026-11-09 21:06，真机）**：health 报 1.0.1；响应头实测带 `Cache-Control: no-store`；重启后的 343 行日志里 0 空行、0 错行（写锁生效）。但面板 401 还在继续——屏幕上的 Edge 窗口仍是旧版 v1.0.0 页面，它里面没有自刷新代码，自愈只对 v1.0.1 之后加载的页面生效；21:04–21:05 有一次页面加载（`GET /favicon.ico -> 404` 为证），之后 401 还在全速继续，说明**普通 F5 不一定能冲掉缓存**，得 **Ctrl+F5 硬刷**，或关掉 Edge 窗口重开 `http://127.0.0.1:8765/`。全日志 532 行里 `/ui/*` 的 200 是 0 次——面板一次都没成功加载过。401 轮询节奏是前台全速（每 2 秒一轮）、后台每分钟 1 轮，这是浏览器后台节流，不是 bug。详见 §14。

v1.0.3 是面板体验版：**动态改回最新在前**（根因是服务端已按新在前返回、面板 JS 又 `.reverse()` 翻回旧在前，新条目沉到底部看不到）；**删掉命令快速执行台**（整块卡 + `renderOps` / `runOp` / `opState` 全删，服务端 `/ui/run` 接口保留）；**新增操作热力图**（GitHub 贡献图风格：53 周 × 7 天，月份/星期标签，5 档绿，悬停显示“某日：N 次操作”；数据来自 `GET /ui/heatmap`，读落盘日志 + `.old` 按天统计，60 秒一轮 + 动态变化时刷新）；**日志行格式改成 `YYYY-MM-DD HH:MM:SS 消息`**（`LogEntry` 加 `date` 字段，`restore_log_file` 认新格式、旧格式行按今天算，`count_heatmap_days` 纯函数单测）。云端 `cargo test` 63/63，真机 64/64（含 Windows 专属 Job 测试与新增 `heatmap_counts_days`）。

v1.0.2 是根治版：**废除每次重启随机变化的动态 `UI_SECRET`，`/ui/*` 改认本机 IP + 固定请求头 `X-Pylon-UI: 1`**；**内联 favicon + `/favicon.ico` 直接 204**；**`log_activity` 不再记 `/ui/*` 的轮询**（只留面板里点的跑命令一条）；新增端到端测试 `ui_end_to_end_browser_flow`（模拟浏览器：GET / 拿页面 → 从页面 JS 里提取前端实际发的 header → 调 `/ui/state` 必须 200）。云端 `cargo test` 62/62，真机 63/63。

**根因复核（2026-11-09 21:17，用户质疑后实测）**：用户指出“第一次打开没有旧缓存，为什么连第一次也是 401”——这个质疑是对的，逼出了一次端到端实测。结果：**鉴权链路本身没有 bug**。云端实机（真 socket）：GET / 返回的 HTML 里密钥已正确注入（占位符残留 0），用页面里提取的密钥调 `/ui/state` 回 200；用户机上同样跑探针脚本：GET / 200、占位符 0、无 header 401、有页面密钥时 `/ui/state`、`/ui/activity`、`/ui/approvals` 全是 200。真正的根因：屏幕上的页面是 v0.9.1 时代留下来的旧页面（带旧进程的密钥），重启和 21:04 那次加载都没把它换掉——v1.0.1 的自愈只对新页面生效，旧页面里没有自刷新代码。但设计本身确实有错：每次重启变一次密钥，等于保证已经开着的面板必然失联一轮。所以 v1.0.2 直接把动态密钥废掉，重启十次面板也能无缝恢复。

```
云端的 Muse (Astro)
  │  Tailscale 私网（Muse 设备 → 用户电脑:8765）
  ▼
Pylon (Windows, 0.0.0.0:8765, Tailscale 私网)
  ├─ HTTP 核心服务 (独立 std 线程, dispatch_request 统一分发)
  │    ├─ config.rs  配置解析、白名单、审批表、三档权限、防穿越
  │    ├─ bridge.rs  health / list / read / write / mkdir / run / patch / search
  │    │    ├─ /api/tasks/*  异步长任务（start/status/stop/list；v2.2.1 并发 8 + tail_bytes）
  │    │    └─ /ui/* 本机 GUI 桥（仅本机 + X-Pylon-UI: 1；HTTP 与 invoke 共用）
  │    ├─ tasks.rs   任务引擎：Job Object + 双管道读线程 + offset/tail 缓冲 + 回车折叠
  │    └─ network.rs NetworkManager：只剩 HTTP 监听热切换（v2.1.3 删隧道/WS/中继）
  ├─ 系统托盘 (tray.rs, 原生 Win32 消息循环)
  │    └─ 打开面板 / 暂停云端写入 / 退出 Pylon / 新审批重建窗口
  └─ Tauri 2 无边框窗口 (src/tauri_ui.rs + ui/index.html，60px 单层顶栏 + 全宽单页)
       └─ 顶栏（权限三档）/ KPI / 审批横幅 / 全宽热力图 / 单行动态 / 任务卡 / 设置
  ▼
本地文件 (白名单文件夹, 如 C:\path\to\your\workspace)
本地进程 (cargo / git / python, 白名单内, 直接调进程, Windows 下进 Job Object)
```

## 1. 目录结构

```
local-bridge-rust/
├── Cargo.toml                  # 包名 muse-local-bridge, 版本 2.3.1, edition 2021
├── Cargo.lock
├── build.rs                    # Windows 下 tauri-build 编译上下文 + 经 tauri-winres 嵌 assets/icon.ico（v2.3.0 指定 window_icon_path）
├── start.bat                   # 一键启动 (纯 ASCII + CRLF, 见 §10)
├── bridge-config.example.json  # 配置模板 (不含真实 token)
├── bridge-config.json          # 真实配置 (不在交付包里, 覆盖解压不会丢)
├── .gitignore
├── README.md                   # 面向用户的使用说明
├── 交付说明.txt                # 每版改了什么、为什么 (中文流水账)
├── 对话协同说明.md             # 与 Muse / 其他 AI 的文件协同约定
├── assets/
│   ├── icon-v3-src.png         # v2.3.0：Cyber-Cat 1024 透明 Squircle 源图
│   ├── icon-v2-256.png         # v2.3.0：256×256 透明品牌 PNG（UI base64 源）
│   ├── icon.ico                # v2.3.0：Cyber-Cat 16/24/32/48/256（EXE / 托盘 / UI）
│   └── icon-v2.ico             # 同 icon.ico，历史命名兼容
├── docs/
│   ├── TECHNICAL.md            # 本文档
│   ├── Pylon使用指引.md        # 面向用户的操作指引 (另有一份在 119 根目录)
├── tests/
│   ├── icon_assets.rs          # v2.3.0：ICO 五档 + 256 PNG 完整性
│   └── fixtures/
│       ├── test-cert.pem       # v1.0.0：本地 TLS 往返测试用的自签证书 (SAN=localhost/127.0.0.1)
│       └── test-key.pem        # 同上, 私钥 (PKCS#8)
└── src/
    ├── main.rs                 # 入口: 读配置 -> 单实例锁 -> 起 NetworkManager -> 托盘 / 待机 (约 158 行)
    ├── config.rs               # 配置结构与读写 (约 486 行, 含 6 个单元测试)
    ├── bridge.rs               # HTTP 服务、审批、修补、检索、安全、三档权限、快照、回收站、Program Capsule (约 5600 行, 含 56 个单元测试)
    ├── network.rs              # v0.9：NetworkManager 热切换 (约 330 行, 含 3 个单元测试)
    ├── tasks.rs                # v2.2.1：异步长任务引擎（Job Object + 增量/tail 缓冲 + 并发上限 8 + 回车折叠，跨平台核心，含单元测试）
    ├── job.rs                  # Windows 专属: Job Object 进程树清理 (约 77 行, 含 1 个单元测试)
    ├── tray.rs                 # Windows 专属: 系统托盘消息循环 + 审批气泡 + 新审批重建窗口 + v2.3.1 第二实例 Event 唤醒监听 (约 380 行)
    └── tauri_ui.rs             # v2.3.1：Tauri 2 无边框窗口（decorations(false)+shadow、窗口控制命令、关窗销毁、async UI 桥、关窗几何恢复 LAST_WINDOW_GEOMETRY，Windows 专属）
├── ui/
│   └── index.html              # v2.3.2：单文件全宽单页 + 60px 单层一体化顶栏（新 Cyber-Cat base64、drag-spacer 拖拽区、—/□/×/⏻ 退出、任务日志弹窗、JS 错误横幅、安全绑定 on()）
├── tauri.conf.json             # v2.3.0：Tauri 上下文（frontendDist=ui，withGlobalTauri，version 2.3.1）
├── capabilities/default.json   # v2.3.0：主窗口 ACL（core:default + allow-start-dragging + allow-internal-toggle-maximize）
├── icons/icon.ico              # v2.3.0：与 assets/icon.ico 同款 Cyber-Cat（tauri-build 默认 fallback）
```

单元测试共 85 个（源码里的 `#[test]` 数，v2.3.1；Windows 实跑 84 个、Linux 实跑 82 个：2 个 Windows 专属测试与 junction 测试不在 Linux 编译，unix symlink 测试不在 Windows 编译；v2.3.1 新增 1 个跨平台测试 `ui_http_socket_blocked_but_internal_ok`）。行数以 `wc -l` 为准。

另有 Python 版备用实现 `~/workspace/local-bridge/local-bridge.py`（标准库零依赖，同一套接口，**没有** `/api/run`），Rust 版为主、Python 版为备。

## 2. 依赖

`Cargo.toml`（`[profile.release] opt-level = 2`；`[workspace]` 自声明独立包，不往上找 workspace，避免被上一级孤零零的 `Cargo.toml` 绊住）：

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| serde / serde_json | 1 | 配置与 HTTP body 的 JSON |
| uuid | 1 (features `v4`) | 生成 32 位 hex token 与任务 id |
| chrono | 0.4 (default-features=false, features `clock` + `std`) | 动态记录打本地日期+时间 YYYY-MM-DD HH:MM:SS（v1.0.3 起带日期，供热力图按天统计） |
| regex | 1 | v0.8：`/api/search` 的正则模式（纯 Rust，跨平台） |
| hmac / sha2 | 0.12 / 0.10 | v0.8：可选防重放签名 HMAC-SHA256（纯 Rust，跨平台） |
| windows-sys | 0.59 (仅 Windows) | v0.8：系统托盘（`Win32_UI_Shell` / `WindowsAndMessaging`）、Job Object（`Win32_System_JobObjects`）、提示音（`Win32_System_Diagnostics_Debug`）；v1.0.0：托盘气泡（`NIF_INFO`）、单实例锁（`CreateMutexW`）；v2.2.0：关窗后 `EmptyWorkingSet` 回落待机内存（`Win32_System_ProcessStatus`） |
| tauri | 2 (仅 Windows) | v2.2.0：Tauri 2 窗口 + 内嵌单文件 Shadcn 界面（替代 Slint）；关窗即销毁、托盘动态重建 |
| tauri-build | 2 (仅 Windows, build-dependencies) | v2.2.0：编译 `tauri.conf.json` 上下文 + 嵌 `icons/icon.ico`（取代 winres 管线） |
| rfd | 0.18 (仅 Windows) | 设置弹窗 [浏览…] 原生目录 / 文件选择器（独立线程打开） |
| arboard | 3 (仅 Windows) | 设置弹窗 [复制] Token 到剪贴板 |
| base64 | 0.23 | v2.1.3：`/api/read_base64` / `/api/write_base64` 编解码 |

v0.8 砍掉的：`gpui` 0.2.2（Linux 上因 `xattr` 传递依赖编不过）、`futures` 0.3（审批改用 std `mpsc`）、`rfd` 0.18（文件夹点选用 Web 面板的 `/ui/browse` 替代，不再弹原生对话框）。**v2.0.1 把 `rfd` 0.18 加回来了**（设置弹窗的 `[浏览…]` 要原生目录选择器，`FileDialog::pick_folder()`）；另新增 `raw-window-handle` 0.6（托盘唤醒要主窗口原生 HWND）。

**v2.1.0 换 Slint**：移除 `eframe` / `egui` / `skrifa`（dev-dep 也删）；新增 `slint` 1.12（特性 `backend-winit` + `renderer-femtovg` + `renderer-software` + `raw-window-handle-06`）、`raw-window-handle` 0.6（直接依赖，托盘唤醒取 HWND）、`arboard` 3（设置弹窗复制 Token 到剪贴板）；`[build-dependencies] slint-build` 1.12（`build.rs` 编译 `ui/main.slint`，样式 `fluent-light`）。Slint 1.18.1 经云端 `cargo check` 验证可编译（`cargo check` 首次约 9 分钟，`cargo test` 首次约 11 分钟，均因 Slint 依赖体量大）。

**v2.2.0 换 Tauri 2**：移除 `slint` / `slint-build` / `raw-window-handle` / `winres`（图标改由 tauri-build 嵌 `icons/icon.ico`）；新增 `tauri` 2 与 `tauri-build` 2（**均仅 Windows**：`[target.'cfg(windows)'.dependencies]` / `.build-dependencies`），`rfd` / `arboard` 也移入 Windows-only——Linux 云端只编译核心服务层（bridge/config/network/tasks），零系统库依赖。GUI 胶水 `src/tauri_ui.rs` 与 `src/tray.rs`、`src/job.rs` 均 `#[cfg(windows)]`。界面是单文件 `ui/index.html`（Shadcn Zinc 双主题，经 `tauri.conf.json` 的 `frontendDist` 内嵌进二进制，`withGlobalTauri` 注入 `window.__TAURI__`）；数据走 `invoke('ui_request')` 桥到 `dispatch_request`（本机 loopback + `X-Pylon-UI: 1`，`/api/*` 另注入当前 token），webview 不开 HTTP 端口、无 CORS。

**v2.2.1 不新增依赖**：单页 UI 仍是原生 HTML/CSS/JS，任务并发、`tail_bytes` 读取与主线程调度只用 std + 现有 `tauri` API；Linux 仍只编译核心层。

v0.9.0 新增依赖：**零**。WebSocket 客户端（`ws.rs`：握手、掩码帧、分片拼接、ping/pong、重连）与 relay_poll 客户端（`relay.rs`：最小 HTTP/1.1 客户端）都是纯 std 实现，不拉 `tungstenite` 也不拉 `iroh`（见 §9 的取舍说明）。

v1.0.0 新增依赖：**rustls + webpki-roots + sha1**（全是纯 Rust，零 C 依赖，Linux 云端与 Windows MSVC 统一编译）。注意：云端无法给 `x86_64-pc-windows-msvc` 目标做 check——`ring` 的构建脚本报 "GNU compiler is not supported for this target"（云端无 MSVC 交叉环境，非代码问题）；Windows 专属代码的裁决靠下 `.crate` 源码核对符号 + 真机 `cargo check`。

Windows 构建要求：Rust MSVC 工具链 + Visual Studio Build Tools C++ 工作负载（另要 Windows SDK 的 `rc.exe` 供 tauri-build 资源编译用）。**Linux 不需要任何系统库**，整包直接 `cargo check` / `cargo test`。

## 3. 核心服务

### 3.1 启动

`main()` 先找 `bridge-config.json`（v2.3.3 顺序：`--config` / `--config=` 启动参数 → `PYLON_CONFIG` 环境变量 → exe 同级目录 → **仅 debug 构建**才回退当前目录），读到配置后恢复 `pylon-activity.log` 末尾 50 条动态、构造 `ServerState`，再：

1. 起 `NetworkManager`（见 §9）：拉起 HTTP 监听（Tailscale 模式，`0.0.0.0:8765`），`main` 只负责建一个 `mpsc` 通知通道塞进 `ServerState.notify_tx`，配置一变就通知它重算；
2. Windows 上起托盘线程、主线程进 Tauri 事件循环（关窗即销毁，托盘 / 新审批按需重建）；Linux 上主线程原地待机（无头服务）。

服务与托盘共享一份 `ServerState`（`Arc`）：配置快照（v0.9 起是 `Arc<RwLock<BridgeConfig>>`，见 §3.5）、动态日志（上限 200 条）、审批队列、熔断开关、任务管理器。v1.0.2 起不再有动态 UI 密钥。

### 3.2 统一分发核心 `dispatch_request`

v0.8 的架构点：所有入口共用一个分发函数

```rust
pub fn dispatch_request(state, method, full_path, path, query, headers, body, peer_ip) -> HttpResponse
```

HTTP Listener 调它，Tauri 的 `ui_request` 命令也直接调它（peer 固定本机 + `X-Pylon-UI: 1`），不用重写接口。`dispatch_request_preauthed`（原 WS / relay 的预授权入口）v2.1.3 起只剩单测在用，标 `#[cfg(test)]`，且禁止 `/` 与 `/ui/*`（回 403）。分发顺序：

1. `GET /`：仅本机（`127.0.0.1` / `::1`），返回小 JSON 状态（v2.0 删网页面板后与 `/health` 同形）；外网 IP 直接 403；
2. `/ui/*`：仅本机 + 固定请求头 `X-Pylon-UI: 1`（v1.0.2 起废除动态密钥），缺头 401；
3. `/health` 与 `/api/*`：远程鉴权（见 §3.3）+ 熔断检查（见 §3.4）+ 三档权限门控（见 §3.5）。

### 3.3 鉴权（远程接口）

- `require_hmac=false`（默认）：只认 `X-Bridge-Token` 请求头，与配置里的 token **完全相等**，否则 401。token 只认请求头，不认 URL 参数。Token 值不记入记忆、不存仓库文件。
- `require_hmac=true`：改认 `X-Bridge-Timestamp`（Unix 秒）+ `X-Bridge-Signature`。签名是 `HMAC-SHA256(key=token, msg = timestamp + method + full_path + body)` 的小写 hex；时间差超过 ±60 秒直接 401。v2.3.3 起同一签名在 ±60 秒窗口内**只能用一次**（`ServerState::seen_signatures` 去重，重复即判重放），越界错误也不再回显服务端时间。此时光有 token 不够（token 是 key，不再当通行证）。拼接方式见 `bridge.rs` 的 `hmac_sha256_hex`，有单元测试覆盖。

### 3.4 熔断开关

`ServerState.kill_switch`（`AtomicBool`，只在内存里，重启即恢复 false）。开启时，`/api/write` / `/api/mkdir` / `/api/run` / `/api/patch` 一律 403（读与搜索不受影响）。面板接口不受熔断影响（不然没法在面板里解除）。开关入口：托盘右键菜单、面板「服务状态」卡。

### 3.5 实时配置与三档权限（v0.9）

`ServerState.cfg` 从 v0.8 的“启动时读一次的快照”改成 `Arc<RwLock<BridgeConfig>>`：`config()` 读快照、`update_config()` 换整份并通知 `NetworkManager`。面板 `/ui/save-config` 先写盘成功、再换内存配置，返回 `{"ok": true, "needs_restart": false, "applied": true}`——**保存后 0.5 秒内生效，不用重启**（见 §9）。

`bridge-config.json` 新增 `permission_mode` 字段（三档，见 §7.2），顶栏 Popover 切换即发 `/ui/save-config`（只发这一个字段，后端接受部分字段）：

| 模式 | 行为 |
| --- | --- |
| `ask_always` 请求批准 | `/api/write`、`/api/patch`、`/api/mkdir`、`/api/run` 一律挂起审批；只读接口（`health` / `list` / `read` / `search`）直接放行 |
| `smart_auto` 帮我批准（默认） | v0.8 行为：普通读写与白名单命令放行；命中写保护路径或审批表命令才挂起 |
| `full_access` 完全访问权限 | 除 `bridge-config.json` 本身外，写 / 修补全放行；跑命令放行白名单、审批表与**白名单外的其他开发命令**，不挂起 |

三条底线三档都一样（见 §7.2）：shell 命令（`cmd` / `powershell` / `pwsh` / `bash` / `sh`，剥路径、`.exe`、NTFS 尾点/尾空格后比对）永远 403；`bridge-config.json` 永远禁远程改（含 `::$DATA`、尾点、大小写变体）；命令 cwd 永远锁在白名单 root 内。

### 3.6 路径解析（`resolve_inside`）

所有文件操作先过同一套解析：

1. root 必须是配置里已声明的别名，否则 400；
2. 相对路径先**词法规范化**（处理 `..`），再判是否越界，越界 400；
3. 已存在的完整路径再 `canonicalize` 一次；目标不存在时，从路径向上找**最深已存在祖先** `canonicalize`，要求祖先仍在 `base_canon` 下，再拼回剩余组件（v2.2.1：拦 junction / 软链接父目录逃逸）。
4. v2.3.3：入口先拒绝含 `:`（NTFS 备用数据流 / 盘符）、含 NUL、以 `/` 或 `\` 开头的 path；祖先**全部解析失败时返回 Err**（旧实现 fail-open，返回未校验的词法路径）。

### 3.7 限制常量（`bridge.rs` / `tasks.rs`）

| 常量 | 值 | 含义 |
| --- | --- | --- |
| `MAX_TEXT_BYTES` | 2 MB | 读 / 写 / 修补单个文本文件的上限；搜索也跳过超过这个大小的文件 |
| `MAX_LIST_ENTRIES` | 500 | 一次列目录最多返回条数 |
| `MAX_RUN_OUTPUT_BYTES` | 512 KB | `/api/run` 每条流（stdout / stderr 各算）的上限，超了截断并标记 `truncated`，同时继续排空防管道堵死 |
| `DEFAULT_RUN_TIMEOUT_SECS` | 300 | `/api/run` 默认超时 |
| `MAX_RUN_TIMEOUT_SECS` | 1800 | 超时上限（请求里 `timeout_secs` 可改，clamp 到 1–1800） |
| `APPROVAL_TIMEOUT_SECS` | 60 | 人工审批默认等 60 秒，超时按拒绝处理 |
| `HMAC_MAX_SKEW_SECS` | 60 | HMAC 防重放允许的时间差（±60 秒） |
| `MAX_SEARCH_RESULTS` | 100 | `/api/search` 最多返回条数，超了标记 `truncated: true` |
| `MAX_CONCURRENT_TASKS` | 8 | `tasks.rs`：同时运行的后台任务上限，超出 `POST /api/tasks/start` 回 429 |
| `MAX_TASK_OUTPUT_BYTES` | 4 MB | `tasks.rs`：每任务每条流（stdout / stderr）的缓冲上限，超出截断并标记 |
| `MAX_PENDING_APPROVALS` | 16 | v2.3.3：挂起审批队列上限，满了回 429（不再无限入队 + 每项占一条线程 60s） |
| `MAX_SNAPSHOTS` / `MAX_SNAPSHOT_TOTAL_BYTES` | 30 条 / 32 MB | v2.3.3：快照环形队列条数上限（原有）+ 总字节上限（新增） |
| `AUTH_MAX_FAILURES` / `AUTH_LOCKOUT_SECS` | 10 次 / 60 s | v2.3.3：认证失败限流阈值与锁定时长（按来源 IP） |
| `HTTP_READ_TIMEOUT_SECS` / `HTTP_WRITE_TIMEOUT_SECS` | 20 s / 30 s | v2.3.3：连接读写超时 |
| `MAX_REQUEST_LINE_BYTES` / `MAX_HEADER_*` | 8 KB / 8 KB / 32 KB / 100 条 | v2.3.3：请求行、单头、头总量、头条数上限 |
| `MAX_CONCURRENT_CONNECTIONS` / `HTTP_REQUEST_DEADLINE_SECS` | 64 / 60 s | v2.3.3：并发连接上限（超 503）与整份请求总时限 |
| `MIN_TOKEN_LEN` | 12 | v2.3.3：设置面板保存 token 的最小长度（加载时只警告） |

### 3.8 动态记录（`log_activity`）

每条记录前面带本地时间（`HH:MM:SS`，chrono）。只记有意义的动作：**写文件、修补、建文件夹、跑命令**，以及**失败的请求**；读文件、列目录、health、搜索的成功不记，免得刷屏。审批的挂起 / 通过 / 拒绝 / 超时也记（`request_approval` 里直接记）。

v1.0.1 修了日志并发追加的脏行：`append_log_file_to` 全程持一把静态写锁（`writeln!` 底层是多次 write，多线程并发追加会把两行字串在一起）。实测：v1.0.0 时代的日志里有 17 个空行 + 64 行错行（如 `20:34:3520:34:35 GET /ui/activity -> 401`）；v1.0.1 重启后的 343 行里 0 空行、0 错行。

一个已知噪音（v1.0.2 已修）：失败的请求也记进日志，所以**一个过期密钥的面板轮询会把日志刷满**——2026-11-09 的 532 行日志里 484 行是 `/ui/* -> 401`（91%）。v1.0.2 起 `log_activity` 直接跳过所有 `/ui/*` 请求（唯一的例外是面板里点的跑命令 `POST /ui/run`，那是真实业务操作，照常记），轮询不再进日志。

## 4. HTTP API 参考

基地址 `http://<Tailscale IP>:8765`（或隧道域名）。远程接口都要鉴权（§3.3），返回一律 JSON，`ok` 字段表示成功与否。

### `GET /health`

```json
// 200
{"ok": true, "version": "2.2.1", "roots": ["work"]}
```

### `GET /api/list?root=<别名>&path=<相对路径>`

列目录。`path` 缺省为 `.`。条目按“目录在前、再按文件名”排序，最多 500 条。

```json
// 200
{"ok": true, "root": "work", "path": "local-bridge-rust",
 "entries": [{"name": "src", "type": "dir", "size": null, "mtime": 1762650000},
             {"name": "Cargo.toml", "type": "file", "size": 2445, "mtime": 1762650000}]}
```

不存在 → 404；不是目录 → 400。

### `GET /api/read?root=<别名>&path=<相对文件>`

```json
// 200
{"ok": true, "root": "work", "path": "local-bridge-rust/Cargo.toml", "size": 2445, "content": "..."}
```

超过 2 MB → 413；不是 UTF-8 文本 → 415；不存在 → 404。轮转日志 `pylon-activity.log.old` 与 `pylon-activity.log` 一样禁读（403，含大小写 / 尾部点号变体），`/api/search` 也按相对路径跳过（v2.2.1 回归覆盖）。

### `POST /api/write` — `{"root", "path", "content"}`

写 UTF-8 文本文件，父目录不存在会自动建。内容超 2 MB → 413；目标是目录 → 400。写保护见 §7.1（`.git` / `bridge-config.json` 硬禁 403，`build.rs` / `*.bat` 等走审批）。v1.0.0：落盘前自动拍快照（快照机制见本节 `/ui/snapshots` 与 `/ui/revert`），响应带 `snapshot_id`。

```json
// 200
{"ok": true, "root": "work", "path": "local-bridge-rust/src/main.rs", "size": 34720, "snapshot_id": "snap-0000000000000001"}
```

### `POST /api/mkdir` — `{"root", "path"}`

```json
// 200
{"ok": true, "root": "work", "path": "local-bridge-rust/assets"}
```

### `POST /api/run` — `{"root", "path", "argv", "timeout_secs"}`

白名单命令执行，详见 §6。成功（命令本身退出码非 0 时仍是 HTTP 200，看 `exit_code` 判断）：

```json
// 200
{"ok": true, "argv": ["cargo", "check"], "exit_code": 0,
 "stdout": "...", "stderr": "...", "timed_out": false, "truncated": false}
```

| 状态码 | 含义 |
| --- | --- |
| 200 | 命令跑完了（`exit_code` 可能是非 0） |
| 400 | 请求不对：`argv` 为空、路径越界、cwd 不存在 |
| 403 | 不在白名单 / 参数消毒拦截 / 审批被拒或超时 / 敏感文件（v2.3.3 起 403 不再回显放行表内容） |
| 408 | 超时（返回超时前已产出的部分输出，`timed_out: true`） |
| 500 | 进程起不来（如可执行文件不在 PATH 里） |

### `POST /api/patch` — `{"root", "path", "patches": [{"search", "replace"}]}`（v0.8 原语 1）

行级 / 块级热修补。CRLF / LF 无感：先统一归一化为 `\n` 做精确匹配定位，写回时保留原文件的换行符风格。每个 `search` 必须**恰好出现 1 次**：0 次 → 404 `Patch Target Not Found`；≥2 次 → 409 `Ambiguous Patch Match`（提示多给几行上下文）。全部 patches 在内存里按序应用成功后，原子写回（同目录临时文件 + rename，写一半断电也不会剩半截文件）。日志记 `修补 <path>（N 处修改）`。

```json
// 200
{"ok": true, "root": "work", "path": "src/main.rs", "patches_applied": 2, "size": 34720}
```

### `GET /api/search?root=<别名>&path=<相对路径>&query=<关键词>&is_regex=false&case_sensitive=false`（v0.8 原语 2）

在 `root/path` 下递归搜 UTF-8 文本文件。自动跳过 `.git` / `target` / `node_modules`、二进制（非 UTF-8）与超过 2 MB 的文件。返回文件相对路径、行号（1-based）、匹配行内容（单行截断 300 字符，超了末尾加 `…`），最多 100 条，超限标记 `truncated: true`。是线性扫描（无索引），小项目毫秒级。

```json
// 200
{"ok": true, "root": "work", "path": ".", "query": "run_operation",
 "results": [{"file": "src/bridge.rs", "line": 512, "text": "pub fn run_operation("}],
 "truncated": false}
```

`query` 为空 → 400；正则写错 → 400。

### `POST /api/trash` — `{"root", "path"}`（v1.0.0）

软删除：目标移入 `root` 下的 `.pylon_trash/<unix 秒>_<扁平化原名>`（路径里的 `/` 换成 `__`，同秒重名加 8 位随机后缀），**严禁物理删除**。移动前以 `moved_to=<回收站相对路径>` 拍快照，回滚即移回来。回收站本身、根目录、`.git`、`bridge-config.json` 硬禁 403。`ask_always` / `smart_auto` 下走审批，`full_access` 下直接移入。`/api/list` 与 `/api/search` 自动跳过 `.pylon_trash`。

```json
// 200
{"ok": true, "root": "work", "path": "old.txt", "trash_path": ".pylon_trash/1794226500_old.txt", "snapshot_id": "snap-0000000000000002"}
```

### `POST /api/rename` — `{"root", "from", "to"}`（v1.0.0）

重命名与移动。`from` 与 `to` 相同 → 400；回收站两侧（从回收站移出、移进回收站）都禁；`.git` / `bridge-config.json` 硬禁。**`to` 与 `from` 都过写保护门控**：`smart_auto` 下目标命中 `build.rs` / `*.bat` / `*.cmd` / `*.ps1` 等需要审批，不会 200 直接落盘；父目录自动创建时同样先判目标（v2.2.1 回归覆盖）。目标是文件时先快照再删后改名；目标是目录 → 400（拒绝覆盖）。源文件本身也拍快照（`moved_to=to`），回滚即移回去。

```json
// 200
{"ok": true, "root": "work", "from": "a.txt", "to": "b.txt", "snapshot_id": "snap-0000000000000003"}
```

### `GET /api/read_base64?root=<别名>&path=<相对文件>` 与 `POST /api/write_base64` — `{"root", "path", "content_base64"}`（v1.0.0）

二进制文件读写，上限 10 MB（`MAX_BASE64_BYTES`）。`write_base64` 走 `write` 同款门控（写保护 + 审批）与写前快照。

```json
// 200
{"ok": true, "root": "work", "path": "icon.ico", "size": 14093, "content_base64": "AAABAAEA..."}
```

### `GET /` 与 `/ui/*`（本地 GUI 桥；v2.0 删网页面板，v2.2.0 起走 invoke）

`GET /` 在 v2.0 改回小 JSON 状态（与 `/health` 同形），网页面板已不存在；`/ui/*` **处理函数保留**，由 Tauri 的 `ui_request` 命令直接调 `dispatch_request_internal`（peer 固定 `127.0.0.1` + `X-Pylon-UI: 1`，`/api/*` 另注入当前 token），不经 HTTP 监听、无 CORS。**v2.3.1 起 HTTP 侧收到 `/ui/*` 一律 403**（`dispatch_inner` 的 `internal` 门控）——本机第三方脚本再也不能带 `X-Pylon-UI: 1` 偷调管理接口；`/ui/*` 只认 Tauri 内部调用。下表为现行接口。

| 接口 | 说明 |
| --- | --- |
| `GET /` | 小 JSON 状态（`{ok, version, roots}`，与 `/health` 同形；网页面板已删，不再返回 HTML） |
| `GET /ui/state` | 状态：版本、监听地址、隧道状态、熔断开关、token、白名单 / 审批表、运行时间、`main_file`（v2.2.1 增，string 或 null） |
| `GET /ui/approvals` | 当前挂起的审批列表（id / kind / detail / age_secs / diff_preview，v1.0.0 加 Diff 预览） |
| `POST /ui/approve` — `{"id", "allow"}` | 允许 / 拒绝一项审批；id 已不在队列时 404 |
| `POST /ui/run` — `{"root", "path", "argv"}` | 本机点按跑命令（审批表里的命令直接跑，点按本身就是确认） |
| `GET /ui/activity` | 最近 50 条动态（最新在前；v1.0.0 起是对象数组 `{time, msg, snapshot_id}`，带快照的条目面板显示 ↶ 回滚按钮；v1.0.3 起加 `date` 字段，往日的条目面板显示“月-日 时间”） |
| `GET /ui/heatmap`（v1.0.3） | 操作热力图数据：读落盘日志（+轮转后的 `.old`）按天统计，返回 `{"ok": true, "days": {"2026-11-09": 12, ...}, "total": N}`；只认新格式行首的日期，旧格式行跳过 |
| `GET /ui/snapshots`（v1.0.0） | 当前内存快照列表（id / root / rel / action / created_at / existed） |
| `POST /ui/revert` — `{"id"}`（v1.0.0） | 回滚一张快照：原文件有字节 → 写回；新建文件 → 删掉；软删除的 → 从回收站移回；成功返回 `{"ok": true, "path": ...}` |
| `POST /ui/save-config` | 保存 token / roots / 权限模式 / HMAC 开关；**接受部分字段**（权限 Popover 只发 `{"permission_mode": ...}` 即可）；成功返回 `{"ok": true, "needs_restart": false, "applied": true}`（v0.9 热重载，保存后 0.5 秒内生效，不用重启） |
| `GET /ui/browse?path=<绝对路径>` | 列出某目录下的子目录（点选文件夹用）；path 为空时返回盘符（Windows） |
| `POST /ui/kill-switch` — `{"paused"}` | 开关熔断 |

### 错误码总表

| 码 | 含义 | 常见触发 |
| --- | --- | --- |
| 400 | Bad Request | 未知 root 别名、路径 `..` 逃逸、缺参数、body 不是合法 JSON、正则写错 |
| 401 | Unauthorized | 没带 token / 签名不对 / 时间差过大 / 本机 GUI 缺 `X-Pylon-UI: 1` |
| 403 | Forbidden | 不在白名单、参数消毒拦截、审批被拒或超时、写保护硬禁、已熔断、`/ui/*` 非本机访问 |
| 404 | Not Found | 文件 / 目录不存在、patch 目标没出现、审批 id 不在队列、未知接口 |
| 408 | Request Timeout | `/api/run` 超时 |
| 409 | Conflict | patch 的 search 出现 2 次以上（歧义）、`/api/program` 乐观锁哈希不匹配 |
| 413 | Payload Too Large | body 或文件超 2 MB |
| 415 | Unsupported Media Type | 文件不是 UTF-8 文本 |
| 429 | Too Many Requests | 后台任务并发已达 `MAX_CONCURRENT_TASKS`（8）；认证失败过多（v2.3.3，10 次 / 60s → 锁 60s）；审批队列已满（v2.3.3，上限 16） |

## 5. 配置文件参考

`bridge-config.json`（首次启动时若不存在，从 `bridge-config.example.json` 复制一份）：

```json
{
  "host": "0.0.0.0",
  "port": 8765,
  "token": "YOUR_32_CHAR_RANDOM_TOKEN",
  "roots": {"work": "C:\\path\\to\\your\\workspace"},
  "run_allowlist": [["cargo", "run"], ["cargo", "test"], ["cargo", "check"], ["cargo", "build"],
                    ["cargo", "--version"], ["rustc", "--version"]],
  "run_asklist": [["git", "push"], ["python"]],
  "permission_mode": "smart_auto",
  "main_file": null,
  "require_hmac": false
}
```

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `host` | `0.0.0.0` | 监听地址。0.0.0.0 表示所有网卡都可连（含 Tailscale 虚拟网卡）；只想本机用就改 `127.0.0.1` |
| `port` | `8765` | 监听端口 |
| `token` | （空） | 鉴权 token。面板「重新生成」出 32 位 hex（浏览器端 `crypto.getRandomValues`） |
| `roots` | （空） | 白名单文件夹：别名 → 绝对路径。所有文件接口的 `root` 参数只认这里的别名 |
| `run_allowlist` | 见下 | 白名单命令表（自动放行），见 §6 |
| `run_asklist` | `[["git","push"],["python"]]` | 需审批的命令表（远程调用时挂起等用户点允许），见 §6 |
| `permission_mode` | `smart_auto` | 三档权限（v0.9）：`ask_always` / `smart_auto` / `full_access`，见 §3.5 |
| `require_hmac` | `false` | 为 true 时远程接口改认时间戳 + HMAC 签名（见 §3.3） |

字段都带 serde 默认：**老配置缺字段时自动补默认**。`save_config` 现在整份 struct 原样写回（v0.8 以前面板保存只写几个字段会丢白名单的问题已修）。

### 当前白名单（13 条，v0.8）与审批表（2 条）

白名单：默认 6 条（cargo run / test / check / build、cargo --version、rustc --version）+ git --version / status / log --oneline -20 / diff / pull（5 条）+ python --version / python -m pip --version（2 条）= 13 条。

**v0.8 迁移注意**：v0.7 的 15 条白名单里本来有 `["git", "push"]` 和裸 `["python"]`，v0.8 把这两条从白名单挪到了审批表。否则白名单优先、审批永远触发不了。迁移后：

- `git push` 与 `python 脚本.py`：远程调用时挂起，等用户在面板点「允许」（60 秒超时按拒绝）；本机面板点按直接跑（点按本身就是确认）；
- `python --version`、`python -m pip --version`：不在默认白名单里（默认白名单只有 6 条 cargo/rustc），走审批表 `["python"]` 前缀，需面板点允许。

v0.9 起白名单与权限级别**改完保存即热生效**（权限门控读实时配置），不用重启；token 与 roots 也是保存即换。改配置不用改代码，面板「快捷操作」会自动多出对应行。

## 6. `/api/run`、参数消毒与 Job Object

`/api/run` 与面板「快捷操作」共用同一份执行入口 `bridge::run_operation(cfg, root, rel, argv, timeout_secs) -> RunOutcome`，HTTP 层只是薄壳（解析 body → 鉴权 → 调它 → 组 JSON），不会两边各一套。v0.7 时白名单检查在 `run_operation` 里，v0.8 挪到了 `run_command`（因为审批通过的命令本来就不在白名单里）。

### 匹配规则（`run_command`，v0.9 三档）

先过 shell 硬禁：`argv[0]` 是 `cmd` / `powershell` / `pwsh` / `bash` / `sh`（剥路径与 `.exe` 后比对）时三档一律 403（`is_shell_command`）。然后按 `permission_mode` 分支：

1. `ask_always`：先过参数消毒，再挂起审批（本机面板点按不挂起，直接跑）；
2. `smart_auto`（默认）：`run_allowlist`（逐 token 精确前缀）中了 → 自动放行；没中再过 `run_asklist`：中了 → 远程调用挂起审批，本机点按直接跑；都没中 → 403（v2.3.3 起不再回显放行 / 审批全表，只提示去设置面板看）；
3. `full_access`：不查 allow / ask 表、不做参数消毒、不挂起审批，直接调进程（cwd 仍锁在 root 内，shell 仍禁）。

### 参数消毒（`validate_argv_safety`，v0.8 漏洞 2）

前缀匹配通过后，再过一遍参数级消毒：

- `python`：禁 `-c`、`-m`（除非整条恰好是白名单里的 `python -m pip --version`）；第一个非 flag 参数必须是白名单内已存在的 `.py` 文件（相对运行目录解析）；
- `git`：禁 `--upload-pack` / `--receive-pack` / `--exec` / `--output`（含 `--output=...` 形式）/ `-c`；外加 `-C` / `--git-dir` / `--work-tree`（这三个能把 git 的工作目录指到白名单外，是任务书之外顺手补的加固）；
- 其他命令：不额外限制。

### 执行方式

- `Command` **直接调进程，不经过 shell**：`&&`、`|`、`>` 只会变成传给命令的字面参数，不会执行；stdin 为 null，stdout / stderr 各开读线程（各截断 512 KB，超了置 `truncated` 并继续排空，防子进程写满管道堵死）。
- cwd 用同一套 `resolve_inside` 锁在 root 内，且必须是已存在目录；`path` 缺省为 root 本身。
- 超时（默认 300 秒，上限 1800 秒）后 `kill()` 直接子进程 + `wait()`，返回 408 与已产出的部分输出。
- **Windows 下子进程进专属 Job Object**（`src/job.rs`，见 §7.3）：每个 run 单独建一个 Job（`CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），守卫活到本次执行结束；超时 / 退出 / Pylon 退出时，系统把 Job 里残留的孙进程一锅端。根治 v0.5 之前“超时只杀直接子进程，孙进程残留、文件被锁（os error 5）”的问题。
- 命令本身退出码非 0 时仍返回 HTTP 200，调用方看 `exit_code` 判断。

## 7. 安全模型

### 7.1 v0.8 补的 4 个漏洞

| 漏洞 | 修复 |
| --- | --- |
| 敏感路径写保护（防 `build.rs` / `.cargo` 提权） | `check_write_protected(rel)`：`.git/` 下一切与 `bridge-config.json` **硬禁**（远程改直接 403，不给审批机会）；`.cargo/`、任意层级的 `build.rs`、`*.bat` / `*.cmd` / `*.ps1` / `*.exe` / `*.dll` **走审批**（用户在面板看清路径点“允许”后才落盘）。`/api/write` 与 `/api/patch` 都过这一关 |
| 命令参数级消毒（防 `python -c` 与 `git --output` 注入） | `validate_argv_safety`，见 §6。`python -c`、`git diff --output=...`、`git -c ...` 都有单元测试覆盖 |
| Windows Job Object 进程树连根清理 | 每个 run 单独建 Job（`KILL_ON_JOB_CLOSE`），Pylon 退出即杀 |
| 可选防重放签名与紧急熔断 | `require_hmac`（HMAC-SHA256 + ±60 秒时间窗，见 §3.3）；熔断开关（托盘菜单 + 面板按钮，见 §3.4） |

### 7.2 三档权限与三条底线（v0.9）

门控入口：`guard_write`（write / patch）、`guard_mkdir`、`run_command`（见 §3.5 的表）。实现细节：

- `bridge-config.json` 三档硬禁（远程改直接 403，不给审批机会——任务书明文要求）；
- `.git/` 在非 `full_access` 时硬禁；
- `ask_always` 下除只读接口外其余全审批；`smart_auto` 下仅 `WriteProtection::NeedsApproval`（`.cargo/`、任意层级 `build.rs`、`*.bat` / `*.cmd` / `*.ps1` / `*.exe` / `*.dll`）审批；
- shell 硬禁先于一切分支：`is_shell_command` 在 `run_command` 最前面，三档都拦。**这是用户定的永久规则，优先于任务书“任意命令”的措辞**；
- 权限切换立即生效：门控每次都读 `Arc<RwLock<BridgeConfig>>` 的实时快照，不缓存模式。

### 7.3 人工审批（原语 3）

高危动作（审批表里的命令、写保护路径的写入 / 修补）触发时，HTTP 线程生成一个 `ApprovalRequest`（id / kind / detail / 创建时间 + `mpsc::Sender<bool>`）放进全局队列，挂起等（默认 60 秒）。面板每 2 秒轮询 `/ui/approvals`，有挂起项时置顶高亮显示并响一声 Windows 提示音（`MessageBeep`）；用户点「允许」则继续执行返回 200，点「拒绝」或超时则返回 403。审批队列与等待都用 std `mpsc`（`recv_timeout`），没有引入 async 运行时。

### 7.4 诚实地说：白名单防的不是“cargo 沙盒化”

- `cargo run / test / check / build` 本来就会执行白名单文件夹里的代码（`build.rs`、被跑的程序本身）；白名单防的是**跑别的工具**，不是把 cargo 沙盒化。
- 桥本来就能往 work 文件夹写任意文件；`/api/run` 新开的口子是让拿到 token 的人能**直接执行**。token 仍是唯一门槛。
- 没有 TLS：明文 HTTP 假设走 Tailscale 私网。不要把 8765 端口暴露到公网。
- 单一共享 token：没有分用户、分权限的 token；拿到 token 就等于全部权限。这是“专用工具”的定位，不是多租户服务。

### 7.5 v2.2.1 补的 4 处边界

| 边界 | 实现与验证 |
| --- | --- |
| `rename` 目标门控 | `POST /api/rename` 对 `to` 同样调用 `check_write_protected`：`smart_auto` 下目标命中 `build.rs` / `*.bat` 等必须挂起审批（不会 200 直接落盘），`.git` / `bridge-config.json` 硬禁；端点回归测试覆盖“目标文件已存在 / 父目录自动创建”两条路径 |
| 轮转日志禁读 | `check_read_protected` 覆盖 `pylon-activity.log.old`（含大小写、尾部点号变体），`GET /api/read` 回 403；`GET /api/search` 按相对路径跳过，不会把轮转日志内容搜出来 |
| junction 父目录校验 | `resolve_inside` 在词法校验后，对不存在的目标向上找**最深已存在祖先** `canonicalize`；祖先不在 `base_canon` 下即越界。`#[cfg(unix)]` 软链接测试 + `#[cfg(windows)]` junction 测试（`mklink /J` 不可用则跳过） |
| 任务并发上限 | `MAX_CONCURRENT_TASKS = 8`，`TaskManager` 先预占名额（RAII `TaskSlot`，Drop 归还）；超限 `POST /api/tasks/start` 回 429，文案带上限值，任务不入列；早期启动失败自动归还名额 |

### 7.6 其他实测教训（从 v0.7 继承）

- 给 struct 加新字段必须 grep 全仓所有 `StructName {` 字面量逐一补齐（v0.5.0 的 E0063 教训）；v0.8 的 `BridgeConfig` 新增字段全部带 serde 默认，老配置自动补。
- 单元测试里的路径别写死 `/tmp`：Windows 上 `/tmp` 不是绝对路径，load / save 校验直接报错；改用 `std::env::temp_dir().join(name)` 生成的测试目录。
- `.bat` 必须写纯 ASCII 英文 + CRLF 换行，绝不能放 UTF-8 中文（cmd 默认 GBK 936 会把中文多字节拆散、行断掉）；`chcp 65001` 救不了。中文说明放 txt / md 里。`.bat` 里 `if (...)` 括号代码块中的 echo 文本绝不能含圆括号（会被当成块结束符），最稳妥是全文件不用括号块、改用 goto 标签跳转；括号只允许出现在被引号括住的 `(x86)` 路径里。
- `where link` 找不到 link.exe 不等于编不了：MSVC 链接器默认不在 PATH，cargo 自己会找它；判 C++ 工作负载装没装，查 `VC\Tools\MSVC` 文件夹在不在。
- Windows 上 cargo 编译报 `failed to remove ....exe / os error 5`，是旧程序还在运行、exe 被锁住了：先关桥窗口或 `taskkill /F /IM muse-local-bridge.exe` 再编；`start.bat` 已在 `cargo run` 之前加 taskkill 自动杀旧进程。v0.8 的 Job Object 进一步兜底：超时 / 退出时孙进程也被系统清理。
- windows-sys 的 API 不要凭记忆写：去 crates.io 下 `.crate` grep 源码核对（v0.8 核对过 `Win32_System_JobObjects`、`Win32_UI_Shell` 的 `NOTIFYICONDATAW`、`Win32_UI_WindowsAndMessaging` 的窗口 / 菜单函数、`Win32_Foundation` 的 `CloseHandle`；句柄类全是 `*mut c_void` 的 type alias，`PCWSTR` 是 `*const u16`）。

## 8. 托盘与 Tauri 2 窗口（v2.3.0）

### 8.1 系统托盘（`src/tray.rs`，Windows 专属）

原生 Win32 消息循环（`windows-sys`，约 250 行）：注册窗口类 → 建消息专用窗口（`HWND_MESSAGE`，不显示、不进任务栏）→ `Shell_NotifyIconW` 挂图标（v2.3.0 的 Cyber-Cat `assets/icon.ico`，找不到再试 `icons/icon.ico` / 当前目录，最后用默认图标）→ `GetMessageW` 循环。右键菜单：

- **打开面板**（左键双击也是这个）；
- **暂停 / 恢复云端写入**（紧急熔断，菜单文字随状态变，托盘提示文字同步变）；
- **退出 Pylon**（删图标、退消息循环、进程退出；Job 句柄一关，残留子进程一并被系统杀掉）。

托盘代码只在 Windows 下编译（`#[cfg(windows)] mod tray`），Linux 下主线程原地待机。

**托盘唤醒、主线程建窗与关窗即销毁**：Slint 时代的「存 HWND + ShowWindow 强制拉起」已废——Tauri 窗口被 destroy 后 HWND 不存在，唤醒改为**动态重建**。v2.2.1 起 `TrayBridge::request_show()` 不在托盘线程直接建窗，而是克隆 `AppHandle` 后调 `app.run_on_main_thread(move || { ... })`：闭包内查 `get_webview_window("main")`，在则 `show()` + `set_focus()`；不在则用 `CREATING` 原子 compare_exchange 防重入 → `create_main_window(&app)` → `show()` + `set_focus()`。托盘双击 / 菜单「打开面板」/ **新审批入队**三条路径共用，删除托盘线程直接 `WebviewWindowBuilder::build` 的路径，避免跨线程创建 WebView 的隐患。**关窗即销毁**：`RunEvent::WindowEvent::CloseRequested` 里 `api.prevent_close()` + `window.destroy()`（不是 hide！WebviewWindow 与 msedgewebview2.exe 进程彻底释放）；`RunEvent::ExitRequested` 默认 `api.prevent_exit()` 驻留托盘，仅托盘「退出」/ 界面 [退出] 置位 `EXIT_FLAG` 后放行。destroy 后延迟 500ms 调 `EmptyWorkingSet` 把可分页页面交还 OS——真机实测待机工作集 **2.4MB**（Private 6.9MB），开窗时约 36MB。WebView2 用户数据目录经 `.data_directory(temp_dir().join("pylon-webview2"))` 重定向到 TEMP，exe 旁不生成 `EBWebView`（单文件纯净）。

### 8.2 Tauri 2 + 内嵌全宽单页界面（`src/tauri_ui.rs` + `ui/index.html`，v2.3.0）

**Slint 已在 v2.2.0 彻底移除**（`slint` / `slint-build` / `ui/main.slint` / `src/ui_slint.rs` 全删）；egui / Web 面板时代的描述见版本历史。v2.2.1 把侧栏多视图改成**全宽单页**：`body` 纵向流、容器 `max-width:1120px; margin:0 auto`，没有 `#sidebar` / 导航高亮；v2.3.1 砍掉 32px 假系统条，页面顶部只剩唯一一行 60px 单层顶栏（`position:sticky`），顺序为：单层顶栏 → 审批横幅 → KPI → 热力图 → 程序胶囊 → 任务 → 动态。界面仍是单文件 `ui/index.html`（Shadcn Zinc 设计体系：默认 Dark、可切 Light），经 `tauri.conf.json` 的 `frontendDist: "ui"` 内嵌进二进制——**无 Node / npm / CDN**，系统字体栈。

**无边框窗口（v2.3.1 单层顶栏）**：`create_main_window` 用 `.decorations(false).resizable(true).shadow(true)`——Windows 原生白色标题栏与边框全部移除，但保留 `WS_THICKFRAME` 边缘拖拽缩放与系统阴影；`capabilities/default.json` 给主窗口授予 `core:window:allow-start-dragging` 与 `core:window:allow-internal-toggle-maximize`（Tauri 2 ACL 下缺这两项时 `data-tauri-drag-region` 的拖拽/双击会被静默拒绝）。窗口控制不再依赖系统按钮：**唯一一行 60px Header**，整行空白挂 `data-tauri-drag-region`（Tauri 原生注入：按住拖动、双击最大化/还原），所有按钮显式 `data-tauri-drag-region="false"`；左侧 38×38 Cyber-Cat + Pylon + 版本徽章 + 运行状态胶囊，右侧同一水平线：三档权限分段切换器、[主题] [设置] 红底白字 [熔断]、竖线分隔、[ — ] `window_minimize`（最小化到任务栏）、[ □ ] `window_toggle_maximize`（最大化/还原）、[ × ] `window_close_destroy`（destroy + `trim_working_set_soon`，进程仍驻留托盘，悬停提示“隐藏到托盘（释放内存）”）、暗红边框 [⏻ 退出] `exit_app`（彻底退出进程）；另暴露 `window_start_drag` 供品牌区/自定义拖拽调用。`×` 不设 `EXIT_FLAG`，因此 `RunEvent::ExitRequested` 继续 `prevent_exit`，关窗只释放 WebView2、待机 ~5MB；[最小化到托盘] 按钮已在 v2.3.1 删除（与 × 重复），`minimize_to_tray` 命令仍保留注册。**关窗几何恢复（v2.3.1）**：`window_close_destroy` / `minimize_to_tray` / `CloseRequested` 销毁前把 `outer_position` / `inner_size` / `is_maximized` 存进 `LAST_WINDOW_GEOMETRY`，`create_main_window` 重建时恢复。**第二实例唤醒（v2.3.1）**：`main.rs` 单实例锁撞见已有实例时，先触发命名 Event `Local\PylonShowWindowEvent` 再退出；托盘线程另起监听线程，收到信号即 `TrayBridge::request_show()` 重建并置顶窗口。浅色主题下顶栏背景/按钮颜色跟随 `--background` / `--muted` 语义变量，关闭按钮 hover 固定 `#e81123` 红底白字。

**Cyber-Cat 品牌资产（v2.3.0）**：1024 透明 Squircle 源图 → Squircle 透明裁切 → `assets/icon-v2-256.png`（256×256）与 `assets/icon.ico`（16/24/32/48/256 五档，PIL 生成）；`build.rs` 通过 `tauri_build::try_build(Attributes::new().windows_attributes(WindowsAttributes::new().window_icon_path("assets/icon.ico")))` 交给 tauri-build 内置的 tauri-winres 嵌进 EXE（任务栏 / Alt+Tab / 资源管理器 / 窗口默认图标），不再单独引入 winres 避免双资源编译器冲突；`icons/icon.ico` 与 `assets/icon-v2.ico` 同步同款资源。UI 品牌区不再重复内嵌两份 base64，而是 `:root { --pylon-icon: url(data:image/png;base64,...) }` 单份复用：单层顶栏 38px。`tests/icon_assets.rs` 校验 ICO 五档与 PNG 256×256。

数据仍不走 HTTP：webview 里 `window.__TAURI__.core.invoke('ui_request', {method, path, body})` → Rust 命令直接调 `dispatch_request_internal`（v2.3.1 起 `/ui/*` 只认这条内部通道，HTTP Socket 一律 403；peer 固定 127.0.0.1 + `X-Pylon-UI: 1`；`/api/*` 另注入当前 token），复用全部 `/ui/*` 与 `/api/*` 逻辑，无 CORS、不依赖监听端口。**v2.2.1 `ui_request` 改 async**：先把 `Arc<ServerState>` 克隆出来，再将原同步逻辑整体移入 `tauri::async_runtime::spawn_blocking(move || { ... })` 后 `.await`，`JoinError` 转成 `{"ok":false,"error":...}`；`State` 不跨 await，文件 / 任务等阻塞操作不再占住 Tauri 主线程。轮询节奏：state/activity/tasks 2s、approvals 1s、program 5s、heatmap 10s。原生能力走命令：`browse_folder` / `browse_file`（rfd，独立线程）、`copy_text`（arboard）、`exit_app` / `minimize_to_tray`（destroy + 退出标志）。

| 区 | 内容 |
| --- | --- |
| 单层一体化顶栏（60px） | 左侧 38×38 Cyber-Cat + `Pylon` + 版本徽章 + 运行状态胶囊；整行空白 `data-tauri-drag-region`（拖拽移动 / 双击最大化）；右侧同一水平线：三档权限切换、[主题] [设置] [熔断]、竖线分隔、`—` 最小化、`□` 最大化/还原、`×` 隐藏到托盘（hover 红底白字）、暗红边框 [⏻ 退出]；`position:sticky; top:0`，背景 `hsl(var(--background))`，Dark/Light 同步 |
| 主顶栏 | 36px Cyber-Cat（CSS 变量单份 base64）+ `Pylon` + 动态版本 + 绿点 `运行中` / 红点 `已熔断` + `监听 <address>`；右侧三档权限分段切换（`每次都问` / `帮我批准` / `完全访问`，`full_access` 文字与边框 `#f97316`）、主题、[设置]、[最小化到托盘]、[退出]、[熔断]（熔断后文案变 `恢复写入`） |
| 权限快捷切换 | 点击即 POST `/ui/save-config` `{permission_mode}`（部分字段更新），成功后重拉 `/ui/state`；失败 toast 后端 error；不重启、立即生效 |
| KPI 四卡 | 今日操作 / 累计操作 / 运行时长 / 权限模式（full_access 橙色警示） |
| 审批置顶横幅 | `position:sticky; top:72px`（避开 60px 单层顶栏），仅队列非空显示（滑入）；队列总数 + kind + detail + Diff 红绿预览（删除行 `#f87171` / 新增行 `#4ade80`）+ 60 秒倒计时 + [允许] / [拒绝] |
| 程序胶囊卡 | 未绑定：显示原因 + [绑定主程序单文件]（`invoke('browse_file')` → `/ui/save-config {main_file}` → 刷新）；已绑定：basename / 总行数 / SHA-256 前 12 位 / 最近退出码 + [查看源码]（只读弹窗，`white-space:pre`，内容来自 `GET /api/program`）+ [立即运行]（用 `last_run.argv`；无记录且 `.py` 时用 `["python", basename]`，否则提示先指定命令；POST `/api/program` + `async:true`，409 提示已刷新） |
| 操作热力图 | 整行全宽卡片：53 列 × 7 行 CSS grid（`grid-auto-flow:column` + `grid-template-rows:repeat(7,1fr)` + `grid-auto-columns:minmax(0,1fr)`），月份标签同为 53 列流体网格，5 档绿 + title tooltip；不出现横向滚动条 |
| 后台任务卡 | 运行中 + 最近结束 20 条：id / argv / pid / 状态徽章 / 耗时 + [停止]（/ui/tasks/stop） |
| 操作动态 | 最新在前 50 条；**每行严格单行**（`white-space:nowrap; overflow:hidden; text-overflow:ellipsis`，`title` 悬停全文），有快照才显示 [↶ 回滚] |

**设置弹窗**：Token（显示/隐藏/复制 arboard/重新生成 crypto.randomUUID）/ 工作区（[浏览…] rfd 目录选择器）/ **main_file 绑定选择器**（[浏览…] rfd 文件选择器，打开时从 `/ui/state.main_file` 回显，保存时后端 `resolve_main_file` 校验）/ 权限三档单选（`.radio-item { gap:8px; }`、radio `margin:0`）/ HMAC 开关（打开时从 `/ui/state` 回显 `require_hmac`，保存时原样回传）/ [保存并热重载]（`/ui/save-config`，支持部分字段）。

**视觉**：Shadcn Zinc 双主题（默认 Dark，`data-theme=light` 切浅色），青绿 #10B981 唯一点缀色，圆角 0.75rem 卡片 + 1px border；中文走系统字体栈（Segoe UI / Microsoft YaHei）；全宽单页按 1120px 居中，窄窗口纵向流式堆叠。v2.3.1 单层顶栏（60px）与页面同底，Light 主题下顶栏/窗口按钮同步切浅色。**内存**：开窗约 36MB（含 WebView2），关窗 destroy + `EmptyWorkingSet` 后真机实测工作集约 2.5MB / Private 6.9MB（v2.2.0 起，v2.3.0 复测同量级）。

### 8.3 异步任务引擎与 Program Capsule（v2.2.0 起，v2.2.1 增强）

**`/api/tasks/*`**（`src/tasks.rs`，跨平台核心 + Windows Job 绑定）：

| 接口 | 行为 |
| --- | --- |
| `POST /api/tasks/start` | `{"root","path","argv","env"}`；复用 `authorize_run`（shell 永禁 + 三档门控 + 参数消毒）；后台线程 spawn + 专属 Job Object，立即 200 `{ok,task_id,pid}`；env ≤32 项、键 ≤128 字符不含 `=`/NUL、值 ≤1024 字符，与 UTF-8 默认注入合并；**并发已满（8）→ 429**（文案含 `MAX_CONCURRENT_TASKS`），任务不入列 |
| `GET /api/tasks/status?id=&offset=&stderr_offset=&tail_bytes=` | 基础字段：`{running,stopped,exit_code,elapsed_secs,stdout_chunk,stderr_chunk,next_offset,next_stderr_offset,stdout_bytes,stderr_bytes,truncated}`；两条流独立游标，每流缓冲上限 4MB（超出截断标记）。**v2.2.1 `tail_bytes`**：存在时改为取缓冲**末尾 N 字节**（向前对齐 UTF-8 起始字节），`next_offset` / `next_stderr_offset` = 当前总长度，响应多一个 `"tail_bytes": N`；`tail_bytes` clamp 1..=`MAX_TASK_OUTPUT_BYTES` |
| `POST /api/tasks/stop` | 置停止标志 + 取出 JobGuard drop（`KILL_ON_JOB_CLOSE` 连根清树）+ `child.kill()` 兜底；已结束任务幂等 ok |
| `GET /api/tasks/list` | 运行中全部 + 最近结束 20 条摘要；内存保留上限 64 条 |

**并发上限（v2.2.1）**：`MAX_CONCURRENT_TASKS = 8`。`TaskManager { tasks, running: Mutex<usize>, limit }`，`new()` 用 8、`with_limit(max)` 供测试；`spawn_task` 先预占名额，`SpawnError::LimitReached` → HTTP 429，`SpawnError::Failed` → 500。RAII `TaskSlot` 在 Drop 归还名额，成功启动后移入 watcher 线程直到任务退出 / 停止，任何早期失败自动归还。

**TaskBuffers 回车折叠与尾截取（v2.2.1）**：`append_capped` 按孤立 `\r`（后一字节非 `\n`）分段——非末段只保留到最后一个 `\n`，并把缓冲回退到该 `\n` 之后（丢弃被覆盖的半行），末段完整追加，`\r\n` 原样保留，最后统一执行 4MB cap。`stdout_tail(n)` / `stderr_tail(n)` 取末尾 N 字节，向前对齐 UTF-8 起始字节后 `from_utf8_lossy`（供 `tail_bytes` 使用）。

锁 ordering：`stop()` 持 tasks 锁再拿 child 锁；watcher 先放 child 锁再拿 tasks 锁——无环。watcher 100ms `try_wait`，结束/停止时更新记录、`last_run` 与动态日志。面板另有 `GET /ui/tasks` / `POST /ui/tasks/stop`（本机操作不查远程门控）。

**Program Capsule 增强**（`/api/program`）：GET 增 `sha256`（文件字节 SHA-256 hex）、`line_count`、`last_run`（`{argv,exit_code,timed_out,at_ms,task_id,async}`，同步 run / 改后即跑 / 异步任务都会更新）。POST 增 `expected_sha256`：与当前文件哈希不匹配 → **409** + `{current_sha256,content,size,mtime_ms,line_count}`（不写文件，防远端盲覆盖）；增 `run_argv` + `async`：**先 `authorize_run`（失败不写）→ guard_write + 快照 + 原子写 → 立即运行**（async 走任务引擎返回 `task_id`/`pid`，同步返回 `run:{exit_code,stdout,stderr,timed_out,truncated}`），单次网络往返闭环。`/ui/save-config` 自 v2.2.0 支持 `main_file` 字段（null/空串解绑，非空校验 `resolve_main_file`）。v2.2.1 单页 UI 直接复用这组接口：绑定 / 换绑、源码只读弹窗、[立即运行]（`async:true`）都不新增契约。

**日志单行化**：`sanitize_log_msg` 在 `push_log_snapshot` 入口把 `\r\n`/`\r`/`\n` 替换为 ` ↵ `、折叠连续空白、修剪首尾——多行命令不再让界面行溢出、不再劈裂按行解析的日志；`restore_log_file` 与 `count_heatmap_days` 共用 `log_timestamp_prefix`（完整 `YYYY-MM-DD HH:MM:SS` 校验），历史脏片段一律跳过。

## 9. 网络与热重载（v0.9 起；v2.1.3 只剩 Tailscale）

**v2.1.3：只保留 Tailscale 模式。**`cloudflare_tunnel` / `ws_reverse` / `iroh_p2p` 三种模式及 `src/ws.rs` / `src/relay.rs` / `src/tls.rs`、两个中继脚本已删除；`bridge-config.json` 里的 `network_mode` / `cf_tunnel_token` / `cloudflared_path` / `ws_url` / `ws_reconnect_secs` / `iroh_relay_url` / `device_node_id` 字段一并删除（老配置里残留这些字段时 serde 默认忽略，不报错）。监听固定绑 `host:port`（默认 `0.0.0.0:8765`），云端 Muse 经 Tailscale 私网直连。以下模式表仅作历史记录（v0.9–v2.1.2 曾实装，现已删除）：`cloudflare_tunnel`（`cloudflared tunnel run --token` 子进程）、`ws_reverse`（`ws.rs` 纯 std WebSocket 客户端 + `docs/cloud_ws_relay.py`）、`iroh_p2p`（`relay.rs` 长轮询中继 + `docs/cloud_relay_poll.py`，非原生 iroh/QUIC）。

v0.9 彻底重写网络生命周期：`network.rs` 的 `NetworkManager` 拿一个 `mpsc` 通知通道，配置一变（`/ui/save-config` 写盘成功后 `notify_config_changed`）就重算监听地址，只在地址变了时重启监听（token / 权限 / roots 变化不重启监听），**保存后 0.5 秒内生效**。监听用非阻塞 accept 50ms 轮询、绑失败重试 5 次；`generation` 计数，旧监听收到新 generation 即退出。

`dispatch_request`（§3.2）是统一分发核心：HTTP Listener 共用同一套业务逻辑。`dispatch_request_preauthed`（预授权入口）原供 WS / relay 调用，v2.1.3 起只剩单测在用，标 `#[cfg(test)]`。

## 10. 构建与运行

### 10.1 前置要求（Windows）

- Rust MSVC 工具链（https://rustup.rs/，装完新开终端跑 `cargo --version` 验证；当前实测 1.99.1）；
- Visual Studio Build Tools 2022，勾 **C++ 工作负载**（提供链接器与 Windows SDK 的 `rc.exe`）；
- （可选）Tailscale：云端 Muse 经私网连进来才稳定，见 README 的接 Tailscale 一节。

### 10.2 `start.bat` 一键启动（纯 ASCII + CRLF）

1. 查 `cargo` 在不在 PATH，没有就提示装 Rust；
2. 查链接器：先 `where link`，找不到就去查 VS 各版本（2022 / 2019 × BuildTools / Community / Professional / Enterprise × 两个 Program Files 根）的 `VC\Tools\MSVC` 文件夹，找到即继续；
3. 若上一级有个孤零零的 `Cargo.toml` 就警告（会绊住 cargo 找 workspace）；
4. 没有 `bridge-config.json` 时从 `bridge-config.example.json` 复制一份；
5. `taskkill /F /IM muse-local-bridge.exe`（杀掉旧进程，防 exe 被锁报 os error 5），再 `cargo build --release`；编译失败时 `pause` 看报错；编译成功后用 `start "" "%~dp0target\release\muse-local-bridge.exe"` 异步拉起程序，随后立即 `exit` 自动关闭当前 CMD 窗口。v1.0.0 起 exe 本身是 `windows_subsystem = "windows"`，运行时完全脱离控制台，只驻留右下角系统托盘。

首次编译会下载依赖，稍等。编译失败时把最后 20 行发给 Astro。

### 10.3 开发验证

```bat
cargo check        :: 最常用，编过即没错
cargo test         :: 83 个单元测试（Windows；config / bridge / network / tasks / job 五个模块 + icon_assets 资产测试，其中 2 个 Windows 专属；Linux 81 个）
cargo build --release
```

**Linux 云端现在能验整包**：v0.8 砍掉 GPUI 后，`cargo check --all-targets` 与 `cargo test` 在 Linux 上零警告全绿（托盘与 Job Object 代码用 `#[cfg(windows)]` 隔开，Linux 下完全不参与编译）。Windows 专属代码（托盘消息循环、Job Object、MessageBeep）仍以真机 `cargo check` / `cargo test` 为准——验证回路：经桥直写文件到用户电脑 → 经 `/api/run` 跑 `cargo check` / `cargo test` 在真机上验。v1.0.1 真机实测：`cargo check` 零警告、`cargo test` 62/62。

## 11. 运行环境

| 项 | 要求 |
| --- | --- |
| 操作系统 | Windows 11（64 位） |
| 工具链 | Rust MSVC（cargo / rustc），Visual Studio Build Tools（含 C++ 工作负载与 Windows SDK） |
| 网络 | Tailscale 私网，两端 Connected；桥监听 `0.0.0.0:8765` |
| 工作区 | 白名单 `work` 指向你指定的文件夹，桥只能动这一个文件夹 |

调用方式（经 Tailscale 直连）：`curl http://<你的电脑 Tailscale IP>:8765/...`，请求头带 `X-Bridge-Token`（只走安全渠道传，绝不写进命令行、不记入记忆、不存文件）。只走 TCP，ping 不通不能当下线；偶尔瞬断出 HTTP:000，重试即可。

## 12. 测试与验证

- 单元测试 **83 个**（Windows，`cargo test`）：config / bridge / network / tasks / job 五个模块 + v2.3.0 的 `tests/icon_assets.rs`；2 个既有 Windows 专属（Job 与增量停止），Linux 81 个。v2.3.0 新增 1 个跨平台图标资产测试（ICO 五档 + 256 PNG）；v2.2.1 的 13 项（每端实跑 12 项：unix symlink / windows junction 各 1 项平台专属）继续覆盖任务并发 429、`tail_bytes` 尾截取、`rename` 目标门控、`.old` 禁读、junction / symlink 父目录校验、回车折叠（含跨 chunk）、UTF-8 尾对齐、并发预占与 `main_file` 状态回显。
- 云端冒烟（release 二进制实跑，`cargo build --release` + 本机 curl）：`/health` 无 token 401、有 token 200；`GET /` 本机 200、密钥已注入（无占位符泄漏，含 `perm-pop` / `csel-net` / `v1.0.1` 标记）；`/ui/state` 无密钥 401、有密钥 200；`POST /ui/save-config {"permission_mode":"full_access"}` 回 `{"applied": true, "needs_restart": false}` 且 state 立即变 full_access；`/api/search` 命中；`/api/patch` 写回；`.git/config` 远程写 403。
- 端到端中继实测（云端 Linux，2026-11-09）：WS 中继（桥 18766 + `cloud_ws_relay.py` 8766，`ws_reverse` 模式）：health / run `cargo --version` / 写文件经中继全通，错 token 被中继断开；relay_poll（桥 18767 + `cloud_relay_poll.py` 8767，`iroh_p2p` 模式）：health 与 run 经中继全通，`device_node_id` 自动生成写回配置；热切换（桥 18769）：tailscale 模式 health 200 → save-config 切 `ws_reverse` + `full_access` 回 `applied: true` → 经中继发白名单外 `cargo --help` 回 exit 0 → 切回 tailscale 后 health 再次 200、中继侧桥断开。
- **v2.1.2 WS 反向冒烟（云端 Linux，2026-11-13）**：真二进制 `ws_reverse` 模式连 `cloud_ws_relay.py`（8766）：握手 101 + Sec-WebSocket-Accept 严格校验通过，鉴权帧 version = 2.1.2；经中继 GET /health 回 200；经中继 POST /api/program 行级修补回 200（`patches_applied: 1`，带 snapshot_id），文件已更新；GET /api/program 回 200（content 一致）；杀中继后桥日志「WS 连接失败：对端关闭了连接，3 秒后重连」→ 每 3 秒重试「Connection refused」；重启中继后桥自动重连「WS 已连接 -> ws://127.0.0.1:8766/bridge」。
- 真机验证（经桥 `/api/run`，2026-11-09）：`cargo check` 零警告、`cargo test` 62/62（含 Windows 专属的 Job 测试）、`cargo --version`（`cargo 1.99.1`）。
- v1.0.0 验收（2026-11-09）：用户重启后 health 报 1.0.0；`/api/trash` 实战：10 个探针文件全进 `.pylon_trash/`（每个都带 `trash_path` 与 `snapshot_id`），119 根目录只剩 `local-bridge-rust` 与 `Pylon 使用指引.md`，`.pylon_trash` 在列表里正确隐藏，读回 `_probe4k.txt` 4000 字节无损；快照实测：`local-bridge-rust/_snapcheck.txt` 写 v1 再写 v2，第二次响应带 `snapshot_id`、读回是 v2（留给用户在面板动态里点 ↶ 回滚验 v1 回来）；日志落盘实测：`target/release/pylon-activity.log` 正常落盘。验收同时抓到两处毛病（→ v1.0.1）：面板重启后成串 401（GET / 没发 Cache-Control，浏览器缓存了注入旧密钥的页面；142 行日志里 114 行是 401）；日志行互相咬合（`writeln!` 底层多次 write，多线程并发追加会把两行字串在一起）。
- v1.0.1 验收（2026-11-09）：用户重启后 health 报 1.0.1；响应头实测带 `Cache-Control: no-store`；重启后的 343 行日志里 0 空行、0 错行（写锁生效）；但面板 401 还在继续——屏幕上的 Edge 窗口仍是旧版 v1.0.0 页面（见 §8.2 的边界说明），需用户在面板窗口按一次 F5，普通 F5 不行就 Ctrl+F5 或关窗口重开；全日志复核（532 行）：`/ui/*` 的 200 是 0 次（面板一次都没成功加载过），401 轮询前台全速、后台每分钟 1 轮（浏览器后台节流，不是 bug），除 401 外无任何 403 / 500 / 408 / 400。
- v1.0.2 根因复核与验收（2026-11-09）：用户质疑“第一次打开没有旧缓存，为什么连第一次也是 401”——端到端实测证明**鉴权链路本身没有 bug**：云端实机（真 socket）GET / 返回的 HTML 里密钥已正确注入（占位符残留 0），用页面里提取的密钥调 `/ui/state` 回 200；用户机上跑探针脚本同样：GET / 200、占位符 0、无 header 401、有页面密钥时 `/ui/state`、`/ui/activity`、`/ui/approvals` 全是 200。根因是屏幕上的页面是 v0.9.1 时代留下来的旧页面（带旧进程的密钥），重启和 21:04 那次加载都没把它换掉；但设计本身确实有错——每次重启变一次密钥，等于保证已经开着的面板必然失联一轮。v1.0.2 云端验证：`cargo check` 零警告、`cargo test` 62/62（新增端到端 `ui_end_to_end_browser_flow` 通过）；实机冒烟（真 socket）：GET / 里 `UI_SECRET` 出现 0 次，`/ui/state` 不带 header 401、带 `X-Pylon-UI: 1` 回 200，`/ui/activity` 与 `/ui/approvals` 带 header 回 200，`/favicon.ico` 回 204，错值 header 回 401。真机验证：`cargo check` 零警告、`cargo test` 63/63（含 Windows 专属的 Job 测试与新增端到端测试）。重启后待验：health 报 1.0.2，面板不用刷新直接恢复，日志里不再有 `/ui/*` 轮询记录，随后把被 484 条 401 污染的旧日志清掉。

## 13. 版本历史

| 版本 | 日期 | 内容 |
| --- | --- | --- |
| v0.1.0 Python | 2026-11-09 | 标准库零依赖桥：health / list / read / write / mkdir；白名单 roots + token + 防穿越；单文件 2 MB 上限；无删除、无 shell |
| v0.1–v0.2 Rust | 2026-11-09 | Rust + GPUI 原生版骨架：黑底只读面板 → Win11 浅色左对齐 |
| v0.3 | 2026-11-09 | 界面内三步配置（生成 Token / 选择文件夹 / 保存并启动当场起服务） |
| v0.4–v0.4.4 | 2026-11-09 | 只剩两张卡（首次配置 + 状态）；`start.bat` 一键启动；逐轮修 .bat 的 GBK 中文、括号块、link.exe 误报、E0373（`run` 闭包加 `move`） |
| v0.5.0 | 2026-11-09 | 新增 `POST /api/run` 白名单命令执行 |
| v0.5.1 | 2026-11-09 | 修 E0063（`BridgeConfig` 加字段漏了 `main.rs` 的字面量）；`start.bat` 加 taskkill（exe 被锁 os error 5） |
| v0.6.0 | 2026-11-09 | 第三张卡「操作」：白名单命令一行一个，▸ 展开，点「运行」后台跑、结果显示在行里 |
| v0.7.0 | 2026-11-09 | 改名 Muse Bridge；窗口 720×800 + 滚轮滚动；图标蓝底白桥（`assets/icon.ico` + winres）；第四张卡「动态」（写/建/跑带时间倒序排）；白名单加 git / python（15 条） |
| v0.7.1 | 2026-11-09 | 品牌定名 Pylon：窗口标题、界面标题、文档标题、`start.bat` 标题全改；功能无变化；包名与 exe 文件名不变 |
| v0.8.0 | 2026-11-09 | **架构重构**：砍 GPUI，改系统托盘 + 内置 Web 面板（5 张卡）；补 3 个 Codex 原语（`POST /api/patch`、毫秒级 `GET /api/search`、人工审批）；堵 4 个安全漏洞（写保护、参数消毒、Job Object、HMAC + 熔断）；多模网络（Tailscale 保底 + Cloudflare Tunnel + WS 反向骨架）；`git push` 与 `python` 从白名单挪到审批表；Linux 云端 `cargo check` / `cargo test` 25/25 全绿 |
| v0.8.1 | 2026-11-09 | **UI 视觉重构**：`src/ui.html` 全面迁移至 `satnaing/shadcn-admin` 设计体系（Zinc 双主题、默认 Dark、Sidebar + Header + 主视图骨架、4-Card KPI 顶栏、双栏 Bento 工作台、审批置顶横幅 + 60 秒倒计时、Settings 表单精致化）；后端接口零改动；版本号三处同步（Cargo.toml / `bridge.rs` 的 `VERSION` 常量 / 面板品牌徽章） |
| v0.9.0 | 2026-11-09 | **三档权限 + 四模网络全实装 + 热重载**：`permission_mode` 三档（ask_always / smart_auto 默认 / full_access，顶栏 Popover 切换，立即生效；shell 三档永禁）；`NetworkManager` 动态热切换（保存配置 0.5 秒内生效，删掉重启提示）；`ws_reverse` 纯 std WebSocket 客户端（鉴权帧 + 请求/响应帧 + 3 秒重连，`docs/cloud_ws_relay.py`）；`iroh_p2p` 按任务书备选条款实现为 `relay_poll` 长轮询中继（`docs/cloud_relay_poll.py`，`device_node_id` 自动生成）；`cloudflare_tunnel` 加 `cloudflared_path` 定位与热切换杀进程；UI 再重做（Sidebar 240px、权限 Popover、自定义下拉全消灭原生 `<select>`、KPI 第二张改权限徽章、保存 Toast）；云端 42/42、真机 43/43 |
| v0.9.1 | 2026-11-09 | **回归修复**：`listener_loop` 把监听 socket 设非阻塞后，Windows 上 accept 的连接继承非阻塞，`handle_client` 读到 WouldBlock 静默断连——大 body 写入几乎必挂、小请求偶尔能过（Linux 测不出来）。修复 = accept 后加一行 `stream.set_nonblocking(false)`；加回归测试 `slow_headers_still_get_response`；云端 46/46、真机 43/43；4KB 大文件写入一次成功、读回无误 |
| v1.0.0 | 2026-11-09 | **出站 TLS + 写前快照回滚 + 审批 Diff 与托盘气泡 + 文件操作闭环 + 日志落盘 + 去黑窗**：`src/tls.rs`（rustls 0.23 + webpki-roots，`wss://` / `https://`，阻塞握手 + 15 秒总时限，Sec-WebSocket-Accept 严格校验）；快照环形队列（上限 30 条，`/ui/snapshots` + `/ui/revert`，面板 ↶ 回滚）；审批 Diff 预览（Unified Diff 截断 4000 字符）+ 托盘气泡（`Shell_NotifyIconW` + `NIF_INFO`，点气泡开面板）+ 单实例锁（`CreateMutexW`）；`/api/trash`（软删除进 `.pylon_trash/`）+ `/api/rename` + `/api/read_base64` 与 `/api/write_base64`（上限 10MB）；`pylon-activity.log` 落盘（超 5MB 轮转 `.old`，启动读回末尾 50 条）；启动去黑窗（`windows_subsystem = "windows"` + 子进程 `CREATE_NO_WINDOW` + `start.bat` 编译后后台脱钩启动自动关窗）；云端 59/59、真机 60/60，联网冒烟测试（https://crates.io + wss://ws.postman-echo.com/raw）在真机上实跑通过 |
| v1.0.1 | 2026-11-09 | **验收补丁**：面板重启后成串 401——GET / 没发 Cache-Control，浏览器缓存了注入旧密钥的页面；修法：所有响应带 `Cache-Control: no-store`，面板 JS 在 401 时自动刷新一次拿新密钥（只刷一次）；日志并发追加脏行——`append_log_file_to` 加静态写锁；云端 61/61、真机 62/62；重启后验证：health 报 1.0.1、响应头实测带 no-store、重启后 343 行日志 0 空行 0 错行；但屏幕上的旧版页面仍需一次手动 F5（普通 F5 不一定冲掉缓存，见 §8.2） |
| v1.0.2 | 2026-11-09 | **根治 401 风暴**：废除每次重启随机变化的动态 `UI_SECRET`，`/ui/*` 改认本机 IP + 固定请求头 `X-Pylon-UI: 1`（ServerState 的 `ui_secret` 字段与 main.rs 的生成代码全删，GET / 不再注入）；内联 SVG favicon + `/favicon.ico` 直接 204；`log_activity` 跳过所有 `/ui/*` 请求（只留面板里点的 `POST /ui/run`）；新增端到端测试 `ui_end_to_end_browser_flow`（GET / 拿页面 → 从页面 JS 提取前端实际发的 header → 调 `/ui/state` 必须 200，没 header 401、非本机 403）；修了 `ui()` 的 `Object.assign` 隐患（opts 自带 headers 会把默认头整个盖掉，现改为合并）；云端 62/62、真机 63/63，实机冒烟全过；重启后待验 health 1.0.2 与面板无缝恢复 |
| v1.0.3 | 2026-11-09 | **面板体验版**：动态改回最新在前（根因：服务端已按新在前返回，面板 JS 又 `.reverse()` 翻回旧在前，新条目沉到底部看不到也不跟进——删掉 `.reverse()` 即修好；往日的条目显示“月-日 时间”）；删掉命令快速执行台（整块卡 + `renderOps` / `runOp` / `opState` 全删；服务端 `/ui/run` 接口保留）；新增操作热力图（GitHub 贡献图风格：53 周 × 7 天，月份/星期标签，5 档绿，悬停显示“某日：N 次操作”；`GET /ui/heatmap` 读落盘日志 + `.old` 按天统计，60 秒一轮 + 动态变化时刷新）；日志行格式改成 `YYYY-MM-DD HH:MM:SS 消息`（`LogEntry` 加 `date` 字段，`restore_log_file` 认新格式、旧格式行按今天算，`count_heatmap_days` 纯函数单测）；`log_file_concurrent_appends_stay_whole` 单测补了启动清文件 + 新格式断言；云端 63/63、真机 64/64，实机冒烟全过；重启后待验 health 1.0.3、热力图出图、动态最新在前 |
| v2.3.0 | 2026-11-14 | **Cyber-Cat 新图标 + 无边框一体化顶栏**：全新独立奶油蜜桃赛博猫（头顶双桥塔天线、胸前桥塔徽章、双爪环抱发光青绿数字桥、无线信号尾巴、珊瑚粉 Squircle）；1024 透明源 → 256 PNG + 16/24/32/48/256 ICO，tauri-build 内置 tauri-winres 嵌 `assets/icon.ico`（EXE/任务栏/Alt+Tab/资源管理器），托盘与 UI（CSS 变量单份 base64）同步；`decorations(false)` 去原生白色标题栏，保留 `resizable(true)` 边缘缩放与 `shadow(true)` 系统阴影；32px sticky 系统条（18px 图标 + Pylon + `data-tauri-drag-region` 拖拽/双击最大化 + `—/□/×`，关闭 hover 红底）+ 无缝主控制栏；新增 `window_minimize` / `window_toggle_maximize` / `window_close_destroy` / `window_start_drag`，capabilities 授予 drag region ACL；`×` destroy 到托盘、待机 ~5MB；Light 主题同步；Windows 83 / Linux 81（新增 ICO/PNG 资产测试） |
| v2.2.1 | 2026-11-14 | **全宽单页 + 线程/安全补全**：UI 去侧栏改 1120px 单页（顶栏三档权限分段切换、full_access 橙色警示、审批置顶横幅、整行全宽热力图、程序胶囊绑定/源码/立即运行、严格单行动态）；托盘 `request_show` 经 `run_on_main_thread` 主线程重建窗口，`ui_request` 改 async + `spawn_blocking`；任务并发上限 8（超限 429）、`status` 增 `tail_bytes`（UTF-8 末尾对齐）、`TaskBuffers` 回车进度折叠；安全回归：`rename` 的 `to` 审批门控、`pylon-activity.log.old` 禁读、`resolve_inside` 最深已存在父目录 canonicalize（junction 父目录校验）；Windows 82 / Linux 80（源码新增 13 项，每端实跑 12 项；unix symlink / windows junction 各 1 项平台专属） |
| v2.2.0 | 2026-11-14 | **四模块架构升级**：① 日志单行化（`push_log_snapshot` 入口 `\r\n`/`\r`/`\n`→` ↵ ` + 折叠空白；`restore_log_file`/`count_heatmap_days` 跳过无完整时间戳前缀的脏行）；② 异步长任务引擎 `/api/tasks/start|status|stop|list`（每任务专属 Job Object，`offset`/`stderr_offset` 增量轮询，stop 关句柄 OS 清树；复用三档门控与 UTF-8 注入；面板任务卡一键停止）；③ Program Capsule：GET 增 `sha256`/`line_count`/`last_run`，POST 增 `expected_sha256` 乐观锁（409+最新内容）与 `run_argv`+`async` 改后即跑（先授权后写再跑）；④ 移除 Slint 换 Tauri 2 + 内嵌单文件 Shadcn Zinc 界面（关窗即销毁、`ExitRequested` 默认 prevent_exit、托盘/新审批动态重建、WebView2 缓存重定向 TEMP、destroy 后 EmptyWorkingSet 待机 2.4MB）；GUI 依赖全 Windows-only，Linux 零系统库；网络测试改临时端口+重试（修 WSL/Windows 共享 localhost 的 TIME_WAIT 撞端口与并行抖动）；真机 `cargo check` 零警告、`cargo test` 70/70（并行 8 连跑全绿），WSL Linux 68/68 零警告；真机冒烟：关窗 WebView2 进程 1→0、托盘双击 1s 重建、优雅退出通过 |
| v2.1.3 | 2026-11-13 | **只保留 Tailscale 模式**：删 Cloudflare / WS 反向 / iroh P2P 三种模式及 `src/ws.rs` / `src/relay.rs` / `src/tls.rs` 与两个中继脚本；设置弹窗删网络模式单选与 ws/cf/iroh 控件；`network.rs` 只剩 HTTP 监听热切换；删 `sha1` / `rustls` / `webpki-roots`，base64 改走 `base64` crate；云端 `cargo check` 零警告、`cargo test` 60/60，真机待验 |
| v2.1.2 | 2026-11-13 | **修复 v2.1.1 真机问题**：窗口透明穿透（恢复 femtovg 整帧渲染 + 全窗实心背景 + 唤醒后 InvalidateRect）；热力图斜边（列索引改 `Math.floor(i/7)`，Slint 的 `/` 是浮点除法；星期标签宽 24px；卡片锁高 170px；外层 padding 16px）；设置弹窗补主程序单文件（`main_file`）与 iroh 节点 ID 展示+复制、ws_url 格式校验；WS 状态进动态日志 + 网络卡片详情行；落地「文件即是程序」：`GET/POST /api/program`（整文件覆写 + 行级热修补）与 `GET /api/changes`（增量变更感知）；配置/日志收敛 exe 同级目录、无配置时内存默认+随机 Token 启动；云端 `cargo check` 零警告、`cargo test` 73/73；WS 反向端到端冒烟通过，真机待验 |
| v2.1.1 | 2026-11-13 | **修复 v2.1.0 真机问题**：托盘最小化失效（`window.run()` → `show()` + `run_event_loop_until_quit()`，点 X/最小化统一 `hide()` + Win32 `SW_HIDE`）；图标缺失（Slint `icon: @image-url` + 顶栏猫脸 Logo，build.rs 嵌图标成功/失败均打印）；热力图全灰（索引访问不随模型替换更新，改 `for` 直接迭代 + 绝对定位 53×7 网格，9px 格总宽 581px，补月份/星期标签）；动态行高 34px + 回滚预留区、活动卡 `vertical-stretch: 1`；危险按钮自定义；设置弹窗选项选中态；rfd 改独立线程；默认软件渲染（`winit-software`，不唤醒 RTX 独显）；审批横幅带队列总数；云端 `cargo check` 零警告、`cargo test` 67/67，真机待验 |
| v2.1.0 | 2026-11-13 | **界面换代：Slint 声明式窗口替代 eframe/egui**：`ui/main.slint`（`fluent-light` 主题，`backend-winit` + `renderer-femtovg` + `renderer-software` 软渲染兜底）+ `src/ui_slint.rs`（Timer 300ms 同步、回调接线）；`eframe`/`egui`/`skrifa` 全部移除，`src/ui_egui.rs` 与 `examples/minwin.rs` 已删；关闭窗口拦截为隐藏到托盘（`HideWindow`），托盘经 `invoke_from_event_loop` + Win32 强制拉起；目标常驻内存 15–25MB（待真机实测）；修复重启后动态历史丢失（`restore_log_file` 改读主文件 + `.old`，启动日志带恢复条数）；补敏感文件读保护（`read`/`search`/`read_base64` 禁读 `bridge-config.json` 与 `pylon-activity.log`）；云端 `cargo check` 零警告、`cargo test` 66/66，真机待验 |
| v2.0.0 | 2026-11-11 | **界面换代：egui 单页窗口替代 Edge 网页面板**：网页面板彻底删除（`src/ui.html`、`GET /` 的 HTML 返回、`/ui/*` 面板接口全部移除，`GET /` 改回小 JSON 状态）；`src/ui_egui.rs`（eframe 0.37.0）：单页纵向布局（状态条 / KPI 四宫格 / 审批拦截横幅 / 热力图 / 实时动态 / 底部），配置区砍掉改成右上角 `[⚙ 设置]` 弹窗；托盘保留（独立线程，点 X 只隐藏到托盘）；窗口默认 760×860；设计语言卡视觉（暖白纸底 #FAFAF8、白卡片大圆角、青绿 #10B981 唯一点缀色）；中文用微软雅黑（msyh.ttc + msyhbd.ttc，`FontFamily::Name("cjk-bold")` 必须同时注册进 `font_data` 与 `families`）；图标为 AI 猫脸版；字体子集化（1210 字，省 33.7MB）做完但复测基本没降，空窗口探针证实内存大头在 eframe/glow/winit/OpenGL 驱动层、下限约 98MB，60MB 到不了，用户拍板 97MB 就这样用；云端 65/65、真机 66/66 |
| v2.0.1 | 2026-11-13 | **界面重构 + 底层隐患摸底修复**：恢复系统全量中文字体（废除子集字体，`assets/fonts/` 已删）；设置弹窗（Token 显示/隐藏/复制/重新生成、工作区 `[浏览…]` 原生目录选择器 rfd、网络模式展开对应输入框、保存并热重载）；动态列表重画（滚动条 VisibleWhenNeeded、底部留白、行对齐、超长截断+悬停）；热力图补月份/星期标签与图例；full_access 警示橙 #F97316、熔断/退出危险色；5 处隐患摸底：写保护大小写/尾部点号绕过（逐段归一化）、子进程 UTF-8 环境变量（PYTHONIOENCODING/PYTHONUTF8/LANG）、回滚前自动拍保护快照（可二次反悔）、托盘唤醒存原生 HWND（ShowWindow + SetForegroundWindow）、listener_loop 已确认无问题（v0.9.1 已修）；新增单测 `write_protection_table` 绕过用例 + `revert_takes_protection_snapshot`；云端 `cargo check` 零警告、`cargo test` 65/65，真机 `cargo test` 66/66 |

## 14. 已知限制与后续方向

- **面板重启后 401 已根治（v1.0.2）**：动态密钥废除后，重启不再产生失联窗口——`/ui/*` 只认本机 IP + 固定头，页面不用刷新。旧教训留档：v1.0.1 的自愈（401 自动刷新一次）只对新加载的页面生效，屏幕上已经打开的旧页面里没有自刷新代码，重启后照样用旧密钥轮询；2026-11-09 实测：21:04–21:05 有一次页面加载（`GET /favicon.ico -> 404` 为证），之后 401 还在全速继续，普通 F5 没冲掉缓存。全日志复核：532 行里 `/ui/*` 的 200 是 0 次，面板一次都没成功加载过；401 轮询前台全速、后台每分钟 1 轮（浏览器后台节流，不是 bug）；除 401 外无任何 403 / 500 / 408 / 400。**但鉴权链路本身没有 bug**：云端与用户机双端到端实测（GET / 拿页面 → 提取页面里的密钥 → 调 `/ui/state`）全是 200，header 大小写、占位符、fetch 传头三处嫌疑全部排除。
- **日志噪音已修（v1.0.2）**：`log_activity` 跳过所有 `/ui/*` 请求（只留面板里点的 `POST /ui/run`），轮询不再进 `pylon-activity.log` 与动态面板。被 484 条 401 污染的旧日志在 v1.0.2 重启验证后清掉（`/api/trash` 移进回收站，留档备查）。
- 删除只走软删除（`/api/trash` 进 `.pylon_trash/`），没有物理删除接口；读 / 写只做 UTF-8 文本、单文件 2 MB 上限，二进制走 `/api/read_base64` 与 `/api/write_base64`（上限 10MB）。
- 出站 TLS 已在 v2.1.3 随 WS/Cloudflare 模式一起删除；监听端是明文 HTTP，假设走 Tailscale 私网，不要把 8765 暴露到公网。
- 单一共享 token，没有分用户、分权限；拿到 token 即全部权限。`require_hmac` 只能防重放，不能分权限。
- `/api/run` 超时杀直接子进程 + Job 清理孙进程（Windows）；Linux 下仍是只杀直接子进程。
- 后台任务并发上限 8（v2.2.1）：超出 `POST /api/tasks/start` 回 429，等运行中的任务结束再启动；并发名额按进程启动预占，早期失败自动归还。
- `cargo run` 不在面板里直接跑（会再开一个桥窗口）；面板里的行只跑白名单里写死的那条完整命令。
- 快照只在内存里（上限 30 条），重启即丢；回滚前自动拍保护快照，支持二次反悔（v2.0.1 已实现：`revert_takes_protection_snapshot` 单测覆盖）。
- 动态日志已落盘（`pylon-activity.log`，超 5MB 轮转 `.old`，启动读回末尾 50 条）；只记写 / 修补 / 建 / 跑，读与 health 的成功不记。
- `iroh_p2p` 是 `relay_poll` 长轮询中继，**不是原生 iroh/QUIC P2P**：传输明文 HTTP，中继只能跑在可信内网（Tailscale / 本机）或前置 TLS 反代；`Device-Node-ID` 是中继里的信箱地址，不是公钥节点 ID。
- `cloudflare_tunnel` 未做真实 Cloudflare 端到端实测（无真实 tunnel token 环境）：代码是 v0.8 已真机跑过的拉起逻辑 + 路径定位 + 热切换杀进程，靠逻辑复用背书。
- 审批默认等 60 秒；面板 2 秒轮询一次，挂起项最多晚 2 秒出现；托盘气泡每秒轮询审批序号，新审批入队时弹一次。
- 后续方向（等用户一句话）：原生 iroh/QUIC P2P（替代 relay_poll 中继）、白名单再加命令直接改 `bridge-config.json` 即可，不用改代码。

---

*本文档与代码同仓，改代码时顺手改文档。交付包 `local-bridge-rust-v2.2.1.zip`（不含 `target/`、`env-check.bat` 与 `bridge-config.json`）。*
