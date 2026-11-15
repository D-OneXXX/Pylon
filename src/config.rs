//! 配置：和 Python 版 `local-bridge.py` 用同一份 `bridge-config.json` 格式，
//! 两边可以互换，Python 先跑通、Rust 再接手时不用重填。
//!
//! v0.8 新增字段（全部带 serde default，老配置缺了自动补）：
//! ```json
//! {
//!   "run_asklist": [["git", "push"], ["python"]],
//!   "ws_endpoint": null,
//!   "require_hmac": false
//! }
//! ```
//!
//! v0.9 新增字段（同样全部带 serde default）：
//! ```json
//! {
//!   "permission_mode": "smart_auto",
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// v2.3.3：用户自设 token 的最小长度（默认 token 是 128 bit 随机串，远长于此）。
/// 放在 config 层是因为「加载」和「保存」两处都要用同一把尺子。
pub const MIN_TOKEN_LEN: usize = 12;

/// v0.9：三档全局权限级别（对齐 Codex 权限弹窗）。
/// - AskAlways：所有写 / 修补 / 建目录 / 跑命令都要人工批准；
/// - SmartAuto（默认）：只对风险操作（写保护路径、审批表命令）要批准；
/// - FullAccess：除 bridge-config.json 本身外全部直接放行，跑命令不查白名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    AskAlways,
    SmartAuto,
    FullAccess,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::SmartAuto
    }
}

/// 词法规范化：不依赖路径存在，把 `.` 去掉、`..` 逐级弹出。
/// 只靠 `canonicalize()` 不够——目标不存在时它会失败，
/// 而 `Path::starts_with` 是按组件比较的，不会自己消掉 `..`，
/// 所以 `base/../etc/passwd` 之前会被误判为“在白名单内”。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub roots: BTreeMap<String, PathBuf>,
    /// v0.5：/api/run 的白名单，argv 必须以其中一条逐 token 精确开头。
    /// 老配置里没有这一项时用默认表；界面保存时会原样保留这一项。
    #[serde(default = "default_run_allowlist")]
    pub run_allowlist: Vec<Vec<String>>,
    /// v0.8：需人工审批的命令表。argv 先过 run_allowlist（自动放行），
    /// 没中再过这一张表，中了就挂起等用户在 Web 面板点「允许」。
    /// 默认：git push（会往外发代码）、python（能跑任意脚本）。
    /// 注意：`["python"]` 这一条是前缀，`python 脚本.py` 会命中它；
    /// `python --version` 在 run_allowlist 里有更具体的条目，先中白名单、直接放行。
    #[serde(default = "default_run_asklist")]
    pub run_asklist: Vec<Vec<String>>,
    /// v0.9：三档全局权限级别（见 PermissionMode）。动态修改、无需重启，
    /// 每次请求都读实时配置，面板 Popover 一改就生效。
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// v2.1.2：绑定的主程序单文件（「文件即是程序」）。
    /// 可选；为空表示未绑定。路径可以是相对路径（相对 work 根目录，
    /// 如 "app.html"）或绝对路径（必须落在某个白名单根目录内）。
    /// 远端 Meta 调 GET /api/program 直接读它、POST /api/program 直接热修补它，
    /// 不用先 list 找目录、再拼路径。
    /// 老配置缺这一项时自动补 None（不绑定），不影响现有功能。
    #[serde(default)]
    pub main_file: Option<String>,
    /// v0.8：为 true 时，/api/* 不再只认 X-Bridge-Token，
    /// 改认 X-Bridge-Timestamp + X-Bridge-Signature（HMAC-SHA256 防重放，见 bridge.rs）。
    #[serde(default)]
    pub require_hmac: bool,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8765
}

/// 默认只放行 cargo 的四个子命令 + 两个 --version（只读）。
/// 注意：cargo run / test / check / build 都会执行白名单文件夹里的
/// 代码（build.rs、被跑的程序本身），所以白名单防的是“跑别的工具”，
/// 不是把 cargo 本身沙盒化；token 仍是唯一门槛，别外泄。
pub fn default_run_allowlist() -> Vec<Vec<String>> {
    vec![
        vec!["cargo".to_string(), "run".to_string()],
        vec!["cargo".to_string(), "test".to_string()],
        vec!["cargo".to_string(), "check".to_string()],
        vec!["cargo".to_string(), "build".to_string()],
        vec!["cargo".to_string(), "--version".to_string()],
        vec!["rustc".to_string(), "--version".to_string()],
    ]
}

/// v0.8 默认需审批的表：git push 与 python。
/// 注意 v0.7 的 15 条白名单里本来有 `["git", "push"]` 和裸 `["python"]`，
/// v0.8 迁移时要把这两条从 run_allowlist 里拿掉、挪到这里，
/// 否则白名单优先、审批永远触发不了。
pub fn default_run_asklist() -> Vec<Vec<String>> {
    vec![
        vec!["git".to_string(), "push".to_string()],
        vec!["python".to_string()],
    ]
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            token: "test-token".to_string(),
            roots: BTreeMap::new(),
            run_allowlist: default_run_allowlist(),
            run_asklist: default_run_asklist(),
            permission_mode: PermissionMode::default(),
            main_file: None,
            require_hmac: false,
        }
    }
}

impl BridgeConfig {
    /// 从文件加载。
    ///
    /// v2.3.3 更正文档：`main.rs` 在文件**不存在**时走「内存默认配置 + 随机 token」
    /// 启动，从不调用本函数的建示例分支；那个分支只剩单测在用（历史行为：
    /// 生成示例配置并让用户先填）。这里保留是为了配置损坏时给出一条明确的
    /// 修复路径，不要再把它当成生产启动路径。
    pub fn load_or_create_example(path: &Path) -> Result<BridgeConfig, String> {
        if !path.exists() {
            let example = serde_json::json!({
                "host": "0.0.0.0",
                "port": 8765,
                "token": "YOUR_32_CHAR_RANDOM_TOKEN",
                "roots": {
                    "work": "C:\\path\\to\\your\\workspace"
                },
                "run_allowlist": default_run_allowlist(),
                "run_asklist": default_run_asklist(),
                "permission_mode": "smart_auto",
                "main_file": null,
                "require_hmac": false,
            });
            std::fs::write(
                path,
                serde_json::to_string_pretty(&example).unwrap(),
            )
            .map_err(|e| format!("写示例配置失败：{e}"))?;
            return Err(format!(
                "已生成示例配置 {}，请填好 roots 和 token 再启动。",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读配置失败 {}：{e}", path.display()))?;
        let cfg: BridgeConfig = serde_json::from_str(&text)
            .map_err(|e| format!("配置不是合法 JSON：{e}"))?;
        if cfg.token.is_empty()
            || cfg.token.starts_with("换成")
            || cfg.token == "YOUR_32_CHAR_RANDOM_TOKEN"
        {
            return Err("请先把 bridge-config.json 里的 token 换成你自己的长随机串。".to_string());
        }
        // v2.3.3：弱 token 只警告不拒绝——手改配置的用户可能已有短 token 在跑，
        // 直接拒绝启动会把人挡在门外；但要让他在启动日志里看见风险。
        if cfg.token.chars().count() < MIN_TOKEN_LEN {
            eprintln!(
                "[bridge] 警告：token 只有 {} 个字符（建议 ≥ {MIN_TOKEN_LEN}，最好是 32 位随机串）——\
                 短 token 更容易被在线爆破，请尽快在设置面板重新生成。",
                cfg.token.chars().count()
            );
        }
        for (alias, p) in &cfg.roots {
            if !p.is_absolute() {
                return Err(format!("roots.{alias} 必须是绝对路径：{}", p.display()));
            }
            if !p.exists() {
                eprintln!("[bridge] 警告：roots.{alias} 现在不存在：{}", p.display());
            }
        }
        Ok(cfg)
    }

    /// v0.8：Web 面板保存配置。整份 struct 原样写回（字段齐全，
    /// 不会像 v0.5 之前那样只写几个字段就把白名单丢了）。
    ///
    /// v2.3.3 修复（审查报告 P2「配置写回非原子、并发保存无互斥」）：
    /// - 先写同目录临时文件再 `rename` 覆盖，截断窗口消失——Python 版 /
    ///   外部进程同时读这份 JSON 时不会再读到半截内容（读到半截会拒绝启动）；
    /// - 全程持一把静态锁，两个并发 `/ui/save-config` 不会互相把对方刚写的
    ///   内容覆盖成自己的旧快照（lost update）。
    ///
    /// 注意：保存后**不需要重启**——`/ui/save-config` 会把新配置换进实时
    /// `RwLock` 并通知 NetworkManager 热切换监听；白名单 / 权限级别 / token
    /// 每次请求都现读。（旧注释写「要重启才生效」，与运行时行为不符，已删。）
    pub fn save_config(path: &Path, cfg: &BridgeConfig) -> Result<(), String> {
        static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = match SAVE_LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败：{e}"))?;
        let tmp = path.with_file_name(format!(
            ".{}.{}.tmp",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "bridge-config.json".to_string()),
            std::process::id()
        ));
        std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("写配置失败：{e}"))?;
        // v2.3.3（审查报告 P2）：先尝试直接 rename（Windows 的 MoveFileEx 带
        // MOVEFILE_REPLACE_EXISTING，能原子覆盖）；只有失败时才退回
        // 「删旧 + 改名」。旧实现无条件 remove_file 再 rename，
        // 中间那一瞬间文件不存在，并发读到的进程会直接报错。
        if let Err(first) = std::fs::rename(&tmp, path) {
            if path.exists() && std::fs::remove_file(path).is_ok() {
                if let Err(e) = std::fs::rename(&tmp, path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("替换配置失败：{e}（首次尝试：{first}）"));
                }
            } else {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("替换配置失败：{first}"));
            }
        }
        Ok(())
    }

    /// v2.1.3：只保留 Tailscale 模式，监听地址固定为 host:port
    /// （默认 0.0.0.0:8765，Tailscale 私网直连；其余模式已删除）。
    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// v2.1.2：解析绑定的主程序单文件（main_file）。
    /// 返回 (root 别名, 相对路径, 绝对路径)。
    /// - main_file 为空 -> Err（未绑定）；
    /// - 相对路径 -> 相对 work 根目录（work 不存在时用字典序第一个根）；
    /// - 绝对路径 -> 必须落在某个白名单根目录内（防越界），相对路径按该根算。
    /// 相对路径里的 `..` / 软链接逃逸同样由 resolve_inside 拦。
    pub fn resolve_main_file(&self) -> Result<(String, String, PathBuf), String> {
        let mf = self
            .main_file
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        if mf.is_empty() {
            return Err("未绑定主程序文件（在设置里填 main_file，或先在 bridge-config.json 里配）".to_string());
        }
        let mf_path = Path::new(&mf);
        // 绝对路径判定：is_absolute() + 盘符 + 以 / 或 \ 开头。
        // Windows 上 "/etc/passwd" 的 is_absolute() 是 false（没盘符），
        // 但它是以根开头的 rooted 路径，join 时会脱离 work 根，必须按绝对路径处理
        // （否则相对分支会把它拼进 work 根、误判为合法）。
        let is_abs = mf_path.is_absolute()
            || (mf.len() >= 2 && mf.as_bytes()[1] == b':')
            || mf.starts_with('/')
            || mf.starts_with('\\');
        if is_abs {
            // 绝对路径：找包含它的根。
            let canon = mf_path.canonicalize().unwrap_or_else(|_| mf_path.to_path_buf());
            let canon_norm = normalize_lexically(&canon);
            for (alias, base) in &self.roots {
                let base_canon = base.canonicalize().unwrap_or_else(|_| base.clone());
                if canon_norm.starts_with(&base_canon) {
                    let rel = canon_norm
                        .strip_prefix(&base_canon)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if rel.is_empty() {
                        return Err("main_file 不能是根目录本身".to_string());
                    }
                    return Ok((alias.clone(), rel, canon_norm));
                }
            }
            return Err("main_file 必须落在某个白名单根目录内".to_string());
        }
        // 相对路径：优先 work 根，其次字典序第一个根。
        let alias = if self.roots.contains_key("work") {
            "work".to_string()
        } else {
            self.roots
                .keys()
                .next()
                .cloned()
                .ok_or_else(|| "没有配置任何白名单根目录".to_string())?
        };
        let abs = self.resolve_inside(&alias, &mf)?;
        Ok((alias, mf, abs))
    }

    /// alias + 相对路径 -> 绝对路径，并强制留在白名单根目录内。
    /// 拒绝绝对路径、盘符、UNC；`..` / 软链接逃逸解析后也会被拦。
    ///
    /// v2.3.3（审查报告 P0 / P1 #11）：额外拒绝三类输入，堵住「判定用的字符串」
    /// 与「实际 I/O 的路径」不是同一个东西的根因：
    /// - `:`（NTFS 备用数据流 `file::$DATA`、`file:stream`，以及盘符）——
    ///   归一化黑名单永远追不上流语法，直接在入口拒；
    /// - NUL（`%00` 解码后进 `std::fs` 会被截断成另一个路径）；
    /// - 以 `/` 或 `\` 开头的 rooted 路径（Windows 上 `is_absolute()` 为 false，
    ///   但 `join` 后会脱离根目录）。
    pub fn resolve_inside(&self, root_alias: &str, rel: &str) -> Result<PathBuf, String> {
        let base = self
            .roots
            .get(root_alias)
            .ok_or_else(|| format!("未知的 root 别名：{root_alias}，可用：{:?}", self.roots.keys().collect::<Vec<_>>()))?;
        if rel.contains('\0') {
            return Err("path 不能包含 NUL 字节".to_string());
        }
        if rel.contains(':') {
            return Err(
                "path 不能包含 ':'（NTFS 备用数据流 / 盘符分隔符一律拒绝）".to_string(),
            );
        }
        if rel.starts_with('/') || rel.starts_with('\\') {
            return Err("path 必须是相对路径，不能以根目录开头".to_string());
        }
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err("path 必须是相对路径，不能是绝对路径".to_string());
        }
        // Windows 盘符 / UNC 在 Path::is_absolute 之外也要拦
        if rel.len() >= 2 && rel.as_bytes()[1] == b':' {
            return Err("path 必须是相对路径，不能带盘符".to_string());
        }
        if rel.starts_with("\\\\") {
            return Err("path 必须是相对路径，不能是 UNC 路径".to_string());
        }
        // base 本身能不能 canonicalize 决定尾段走哪条兜底：
        // 根目录不存在（首次建目录的场景）时没有真实路径可比对，只能信词法结果。
        let base_canon_ok = base.canonicalize().ok();
        let base_canon = base_canon_ok.clone().unwrap_or_else(|| base.clone());
        let joined = if rel.is_empty() || rel == "." {
            base_canon.clone()
        } else {
            base_canon.join(rel)
        };
        // 先词法规范化再判越界：不存在的目标 canonicalize 会失败，
        // 不能把带 `..` 的原始路径直接拿去 starts_with。
        let normalized = normalize_lexically(&joined);
        if !normalized.starts_with(&base_canon) {
            return Err("path 越界：不允许离开白名单根目录（.. / 软链接逃逸都会被拦）".to_string());
        }
        // 目标已存在时再 canonicalize 一次，拦软链接逃逸。
        let target_canon = match normalized.canonicalize() {
            Ok(c) => {
                if !c.starts_with(&base_canon) {
                    return Err("path 越界：不允许离开白名单根目录（.. / 软链接逃逸都会被拦）".to_string());
                }
                c
            }
            // v2.2.1：目标不存在（新建文件 / 建目录 / rename 的新名字）时只比值词不够——
            // 中间段可能是指向根外的软链接 / junction，write 与 create_dir_all 会顺着
            // 链接把内容落到根外。向上找最深已存在祖先 canonicalize：祖先跑出 base_canon
            // 即判越界；否则把剩余组件拼回祖先的真实路径返回。
            Err(_) => {
                if base_canon_ok.is_none() {
                    // 根目录本身不存在：它下面不可能已有链接，保持词法结果。
                    normalized
                } else {
                    resolve_via_existing_ancestor(&normalized, &base_canon)?
                }
            }
        };
        Ok(target_canon)
    }
}

/// v2.2.1：目标还不存在时的兜底解析（resolve_inside 的尾段）。
/// 从目标逐级向上找第一个能 canonicalize 的祖先，要求它仍在 `base_canon` 内
/// （拦住中间段的软链接 / junction 指向根外）；返回「祖先真实路径 + 剩余组件」。
///
/// v2.3.3 修复（审查报告 P1 #11b，fail-open）：一个祖先都解析不出来时**必须报错**。
/// 旧实现 `Ok(normalized.to_path_buf())` 属于「没校验成功却返回成功」——剩余用户
/// 组件从未 canonicalize，下游直接拿去做 fs I/O，白名单形同虚设。宁可误杀（调用方
/// 看到 400 再重试）也不能放行未校验的路径。
fn resolve_via_existing_ancestor(
    normalized: &Path,
    base_canon: &Path,
) -> Result<PathBuf, String> {
    let mut rest: Vec<OsString> = Vec::new();
    let mut cur = normalized.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(c) => {
                if !c.starts_with(base_canon) {
                    return Err(
                        "path 越界：不允许离开白名单根目录（.. / 软链接逃逸都会被拦）".to_string(),
                    );
                }
                let mut out = c;
                for name in rest.iter().rev() {
                    out.push(name);
                }
                return Ok(out);
            }
            Err(_) => {
                let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
                    break;
                };
                rest.push(name);
                // pop 到盘符 / 根仍失败就停：理论上只有文件系统异常会走到这。
                if !cur.pop() {
                    break;
                }
            }
        }
    }
    Err(format!(
        "path 无法解析（祖先目录全部不可访问）：{}",
        normalized.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_allowlist_defaults() {
        // 老配置里没有 run_allowlist 时，用默认表（用户现有配置就是这种情况）。
        // 根目录用测试目录本身：它在 Linux 和 Windows 上都是绝对路径且存在，
        // 写死 "/tmp" 在 Windows 上不是绝对路径，load 会直接报错。
        let dir = test_dir("bridge-default-test");
        let path = dir.join("bridge-config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "host": "0.0.0.0",
                "port": 8765,
                "token": "t",
                "roots": {"work": dir},
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = BridgeConfig::load_or_create_example(&path).unwrap();
        assert_eq!(cfg.run_allowlist, default_run_allowlist());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_fields_default() {
        // v0.8：老配置缺新字段时自动补默认，不能报错。
        let dir = test_dir("bridge-v08-default-test");
        let path = dir.join("bridge-config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "host": "0.0.0.0",
                "port": 8765,
                "token": "t",
                "roots": {"work": dir},
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = BridgeConfig::load_or_create_example(&path).unwrap();
        assert_eq!(cfg.run_asklist, default_run_asklist());
        assert!(cfg.run_asklist.contains(&vec!["git".to_string(), "push".to_string()]));
        assert!(cfg.run_asklist.contains(&vec!["python".to_string()]));
        assert_eq!(cfg.bind_addr(), "0.0.0.0:8765");
        assert_eq!(cfg.permission_mode, PermissionMode::SmartAuto);
        assert!(!cfg.require_hmac);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_mode_parses() {
        // 三档权限：ask_always / smart_auto / full_access；写错直接报错。
        let dir = test_dir("bridge-perm-test");
        let path = dir.join("bridge-config.json");
        for (text, want) in [
            ("ask_always", PermissionMode::AskAlways),
            ("smart_auto", PermissionMode::SmartAuto),
            ("full_access", PermissionMode::FullAccess),
        ] {
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&serde_json::json!({
                    "host": "0.0.0.0", "port": 8765, "token": "t",
                    "roots": {"work": dir}, "permission_mode": text,
                }))
                .unwrap(),
            )
            .unwrap();
            let cfg = BridgeConfig::load_or_create_example(&path).unwrap();
            assert_eq!(cfg.permission_mode, want);
        }
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "host": "0.0.0.0", "port": 8765, "token": "t",
                "roots": {"work": dir}, "permission_mode": "yolo",
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(BridgeConfig::load_or_create_example(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_addr_is_host_port() {
        // v2.1.3：只保留 Tailscale 模式，监听地址恒为 host:port。
        let mut cfg = BridgeConfig::default();
        assert_eq!(cfg.bind_addr(), "0.0.0.0:8765");
        cfg.port = 9999;
        assert_eq!(cfg.bind_addr(), "0.0.0.0:9999");
        cfg.host = "127.0.0.1".to_string();
        assert_eq!(cfg.bind_addr(), "127.0.0.1:9999");
    }

    #[test]
    fn main_file_resolves_relative_and_absolute() {
        // v2.1.2：main_file 相对路径走 work 根；绝对路径必须落在根内；
        // 未绑定时报错；老配置缺字段自动补 None。
        let dir = test_dir("bridge-mainfile-test");
        let work = dir.join("work");
        let _ = std::fs::create_dir_all(&work);
        std::fs::write(work.join("app.html"), "<h1>hi</h1>").unwrap();
        let path = dir.join("bridge-config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "host": "0.0.0.0", "port": 8765, "token": "t",
                "roots": {"work": work},
                "main_file": "app.html",
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = BridgeConfig::load_or_create_example(&path).unwrap();
        let (alias, rel, abs) = cfg.resolve_main_file().unwrap();
        assert_eq!(alias, "work");
        assert_eq!(rel, "app.html");
        // Windows 上 canonicalize() 返回 \\?\ 前缀的 UNC 路径，
        // 期望值也要先 canonicalize 再比（Linux 上两者相同）。
        let expected = work
            .join("app.html")
            .canonicalize()
            .unwrap_or_else(|_| work.join("app.html"));
        assert_eq!(abs, expected);
        // 绝对路径（在根内）也行。
        let mut cfg2 = cfg.clone();
        cfg2.main_file = Some(work.join("app.html").to_string_lossy().into_owned());
        let (alias2, rel2, _) = cfg2.resolve_main_file().unwrap();
        assert_eq!(alias2, "work");
        assert_eq!(rel2, "app.html");
        // 绝对路径越界 -> 拒绝。
        let mut cfg3 = cfg.clone();
        cfg3.main_file = Some("/etc/passwd".to_string());
        assert!(cfg3.resolve_main_file().is_err());
        // 未绑定 -> 报错。
        let mut cfg4 = cfg.clone();
        cfg4.main_file = None;
        assert!(cfg4.resolve_main_file().is_err());
        // 老配置缺 main_file 自动补 None。
        let mut cfg5 = cfg.clone();
        cfg5.main_file = None;
        assert_eq!(cfg5.main_file, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_config_roundtrip() {
        // v0.8：整份 struct 写回再读回，字段一个不能丢。
        let dir = test_dir("bridge-save-test");
        let path = dir.join("bridge-config.json");
        let mut cfg = BridgeConfig::default();
        cfg.token = "newtoken".to_string();
        cfg.roots.insert("work".to_string(), dir.clone());
        cfg.require_hmac = true;
        BridgeConfig::save_config(&path, &cfg).unwrap();
        let reloaded = BridgeConfig::load_or_create_example(&path).unwrap();
        assert_eq!(reloaded.token, "newtoken");
        assert!(reloaded.require_hmac);
        assert_eq!(reloaded.run_allowlist, default_run_allowlist());
        assert_eq!(reloaded.run_asklist, default_run_asklist());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_inside_uses_existing_ancestor() {
        // v2.2.1：目标不存在时用「最深已存在祖先的真实路径 + 剩余组件」拼回；
        // 已存在的目标仍返回真实路径；`..` 词法逃逸照旧拒绝。
        let dir = test_dir("bridge-resolve-ancestor-test");
        let work = dir.join("work");
        let sub = work.join("sub");
        let _ = std::fs::create_dir_all(&sub);
        let mut cfg = BridgeConfig::default();
        cfg.roots.insert("work".to_string(), work.clone());
        let base_canon = work.canonicalize().unwrap();
        let got = cfg.resolve_inside("work", "sub/new.txt").unwrap();
        assert_eq!(got.parent().unwrap(), sub.canonicalize().unwrap());
        assert_eq!(got.file_name().unwrap(), "new.txt");
        assert!(got.starts_with(&base_canon), "解析结果必须留在根内：{got:?}");
        // 已存在目录本身 = 真实路径。
        assert_eq!(
            cfg.resolve_inside("work", "sub").unwrap(),
            sub.canonicalize().unwrap()
        );
        // 词法逃逸依旧被拦。
        assert!(cfg.resolve_inside("work", "../escape.txt").is_err());
        assert!(cfg.resolve_inside("work", "sub/../../escape.txt").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_inside_blocks_symlink_escape() {
        // v2.2.1：不存在的目标穿过软链接时，祖先 canonical 落在根外 -> 拒绝；
        // 指向根内的软链接照常放行（返回解析后的真实路径）。
        let dir = test_dir("bridge-resolve-symlink-test");
        let work = dir.join("work");
        let outside = dir.join("outside");
        let inner = work.join("inner");
        let _ = std::fs::create_dir_all(&work);
        let _ = std::fs::create_dir_all(&outside);
        let _ = std::fs::create_dir_all(&inner);
        let escape = work.join("jump");
        let inside_link = work.join("alias");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        std::os::unix::fs::symlink(&inner, &inside_link).unwrap();
        let mut cfg = BridgeConfig::default();
        cfg.roots.insert("work".to_string(), work.clone());
        let r = cfg.resolve_inside("work", "jump/new.txt");
        assert!(r.is_err(), "软链接逃逸没拦住：{r:?}");
        let got = cfg.resolve_inside("work", "alias/new.txt").unwrap();
        assert_eq!(got.parent().unwrap(), inner.canonicalize().unwrap());
        let _ = std::fs::remove_file(&escape);
        let _ = std::fs::remove_file(&inside_link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_inside_blocks_junction_escape() {
        // v2.2.1：junction（目录重解析点）也要拦住。`mklink /J` 不需要管理员权限；
        // 个别环境建不出来（策略限制 / 非 NTFS）就直接跳过，不让测试红灯。
        let dir = test_dir("bridge-resolve-junction-test");
        let work = dir.join("work");
        let outside = dir.join("outside");
        let _ = std::fs::create_dir_all(&work);
        let _ = std::fs::create_dir_all(&outside);
        let jump = work.join("jump");
        let _ = std::fs::remove_dir(&jump);
        let status = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&jump)
            .arg(&outside)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!("跳过：mklink /J 在本机不可用");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut cfg = BridgeConfig::default();
        cfg.roots.insert("work".to_string(), work.clone());
        // 经 junction 新建文件：最深已存在祖先（junction）canonical 到 outside -> 拒绝。
        let r = cfg.resolve_inside("work", "jump/new.txt");
        assert!(r.is_err(), "junction 逃逸没拦住：{r:?}");
        // 根内普通子目录仍正常。
        std::fs::create_dir_all(work.join("sub")).unwrap();
        assert!(cfg.resolve_inside("work", "sub/new.txt").is_ok());
        // 只删 junction 本身（remove_dir 不跟随重解析点），再清目录。
        let _ = std::fs::remove_dir(&jump);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
