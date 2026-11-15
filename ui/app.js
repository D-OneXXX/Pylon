/* ============================================================
   Pylon UI 逻辑：全部数据经 invoke('ui_request') 桥到核心服务层
   （本机 loopback + X-Pylon-UI），不开 HTTP 端口、无 CORS。
   ============================================================ */
"use strict";
(function () {
  const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  // v2.3.2：安全绑定——元素不存在时静默跳过，单个绑定失败绝不拖垮整个脚本。
  const on = (id, ev, fn) => { const el = $(id); if (el) el.addEventListener(ev, fn); };

  if (!invoke) {
    // Tauri 注入可能晚于脚本执行：短暂重试 1 秒（每 100ms 探一次），
    // 就绪后整页重载一次让主逻辑重新跑；仍无则显示提示。
    let n = 0;
    const t = setInterval(() => {
      const iv = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
      if (iv || ++n >= 10) {
        clearInterval(t);
        if (iv) location.reload();
        else document.body.innerHTML = '<div style="padding:40px;font-family:sans-serif">此界面需在 Pylon 窗口内打开（Tauri invoke 不可用）。</div>';
      }
    }, 100);
    return;
  }

  /* ---- v2.3.2：窗口控制最先绑定（try/catch 隔离）——数据接口挂了也不影响关窗/退出 ---- */
  try {
    on("exit-btn", "click", () => invoke("exit_app"));
    const winMin = $("win-min"), winMax = $("win-max"), winClose = $("win-close");
    if (winMin) winMin.addEventListener("click", () => invoke("window_minimize"));
    if (winMax) winMax.addEventListener("click", () => invoke("window_toggle_maximize"));
    if (winClose) winClose.addEventListener("click", () => invoke("window_close_destroy"));
  } catch (e) { /* 窗口控制绑定失败不阻塞后续初始化 */ }

  async function api(method, path, body) {
    return await invoke("ui_request", { method, path, body: body === undefined ? null : body });
  }

  let toastTimer = null;
  function toast(msg) {
    const t = $("toast");
    t.textContent = msg;
    t.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.remove("show"), 2800);
  }

  /* ---- 主题 ---- */
  function applyTheme(theme) {
    document.documentElement.setAttribute("data-theme", theme);
    try { localStorage.setItem("pylon-theme", theme); } catch (e) {}
  }
  on("theme-btn", "click", () => {
    applyTheme(document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark");
  });
  try { applyTheme(localStorage.getItem("pylon-theme") || "dark"); } catch (e) { applyTheme("dark"); }

  /* ---- 状态轮询（2s） ---- */
  const PERM_TEXT = { ask_always: "每次都问", smart_auto: "帮我批准", full_access: "完全访问" };
  let currentMode = "smart_auto";
  let killed = false;
  let modeBusy = false;

  function fmtUptime(s) {
    if (s == null) return "—";
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
    return h > 0 ? (h + "h " + m + "m") : (m + "m " + (s % 60) + "s");
  }

  function setSeg(mode) {
    document.querySelectorAll(".seg-btn").forEach(b => {
      b.classList.toggle("active", b.getAttribute("data-mode") === mode);
    });
    const seg = $("perm-seg");
    if (seg) seg.classList.toggle("mode-full", mode === "full_access");
  }

  async function pollState() {
    try {
      const v = await api("GET", "/ui/state");
      if (!v || !v.ok) return;
      $("version-badge").textContent = "v" + v.version;
      $("listen-addr").textContent = "监听 " + v.address;
      killed = !!v.kill_switch;
      $("status-dot").className = "dot " + (killed ? "bad" : "ok");
      $("status-text").textContent = killed ? "已熔断" : "运行中";
      $("kill-btn").textContent = killed ? "恢复写入" : "熔断";
      currentMode = v.permission_mode || "smart_auto";
      setSeg(currentMode);
      const pt = PERM_TEXT[currentMode] || currentMode;
      const kp = $("kpi-perm");
      kp.textContent = pt;
      kp.className = "value perm" + (currentMode === "full_access" ? " warn" : "");
      $("kpi-perm-sub").textContent = currentMode === "full_access"
        ? "完全放行 · 仅配置文件受保护"
        : "三档门控 · 实时生效";
      $("kpi-uptime").textContent = fmtUptime(v.uptime_secs);
      // v2.3.3：审批超时由服务端下发（APPROVAL_TIMEOUT_SECS），
      // 界面不再硬编码 60 秒——服务端改了时限，倒计时跟着变。
      if (v.approval_timeout_secs) approvalTimeoutSecs = v.approval_timeout_secs;
    } catch (e) { /* 窗口重建后无缝恢复 */ }
  }

  document.querySelectorAll(".seg-btn").forEach(b => {
    b.addEventListener("click", async () => {
      const mode = b.getAttribute("data-mode");
      if (modeBusy || mode === currentMode) return;
      modeBusy = true;
      try {
        const v = await api("POST", "/ui/save-config", { permission_mode: mode });
        if (v && v.ok) {
          toast("权限已切换：" + (PERM_TEXT[mode] || mode));
          await pollState();
        } else {
          toast("切换失败：" + ((v && v.error) || "未知错误"));
        }
      } catch (e) {
        toast("切换失败");
      } finally {
        modeBusy = false;
      }
    });
  });

  on("kill-btn", "click", async () => {
    try {
      const v = await api("POST", "/ui/kill-switch", { paused: !killed });
      if (v && v.ok) {
        toast(killed ? "熔断已解除" : "熔断已开启：云端写入暂停");
        pollState();
      } else {
        toast("操作失败：" + ((v && v.error) || "未知错误"));
      }
    } catch (e) { toast("操作失败"); }
  });

  /* ---- 审批（1s，仅挂起时显示） ---- */
  let approvalId = null;
  let approvalCreatedAt = 0;
  // v2.3.3：默认值与服务端 APPROVAL_TIMEOUT_SECS 一致，拿到 /ui/state 后以服务端为准。
  let approvalTimeoutSecs = 60;

  function renderDiff(text) {
    return text.split("\n").map(l => {
      const cls = l.startsWith("-") ? "del" : l.startsWith("+") ? "add" : "";
      return cls ? '<span class="' + cls + '">' + esc(l) + "</span>" : esc(l);
    }).join("\n");
  }

  async function pollApprovals() {
    try {
      const v = await api("GET", "/ui/approvals");
      const list = (v && v.approvals) || [];
      const banner = $("approval-banner");
      if (list.length === 0) {
        banner.classList.remove("show");
        approvalId = null;
        return;
      }
      const a = list[0];
      approvalId = a.id;
      approvalCreatedAt = a.created_at;
      banner.classList.add("show");
      $("approval-kind").textContent = a.kind;
      $("approval-detail").textContent = a.detail;
      $("approval-queue").textContent = "队列共 " + list.length + " 项，当前第 1 项";
      const diff = $("approval-diff");
      if (a.diff_preview) {
        diff.style.display = "block";
        diff.innerHTML = renderDiff(a.diff_preview);
      } else {
        diff.style.display = "none";
      }
    } catch (e) {}
  }

  setInterval(() => {
    if (!approvalId) return;
    const remain = Math.max(0, approvalTimeoutSecs - (Math.floor(Date.now() / 1000) - approvalCreatedAt));
    $("approval-countdown").textContent = remain + "s";
  }, 1000);

  async function resolveApproval(allow) {
    if (!approvalId) return;
    try {
      const v = await api("POST", "/ui/approve", { id: approvalId, allow: allow });
      if (v && v.ok) toast(allow ? "已允许" : "已拒绝");
      else toast((v && v.error) || "该审批已不在队列");
      pollApprovals();
    } catch (e) { toast("操作失败"); }
  }
  on("approve-btn", "click", () => resolveApproval(true));
  on("deny-btn", "click", () => resolveApproval(false));

  /* ---- 热力图（10s，53 列 × 7 行流体网格） ---- */
  function heatColor(n) {
    if (n === 0) return "var(--heat-0)";
    if (n <= 2) return "#9BE9A8";
    if (n <= 5) return "#40C463";
    if (n <= 9) return "#30A14E";
    return "#216E39";
  }
  function dateKey(d) {
    const p = (x) => String(x).padStart(2, "0");
    return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate());
  }

  async function pollHeatmap() {
    try {
      const v = await api("GET", "/ui/heatmap");
      if (!v || !v.ok) return;
      const days = v.days || {};
      $("kpi-total").textContent = v.total || 0;
      const today = dateKey(new Date());
      $("kpi-today").textContent = days[today] || 0;
      $("kpi-today-date").textContent = today;
      // 53 周 × 7 天，按周优先（index = week*7 + weekday，weekday 0 = 周日）。
      const now = new Date();
      const lastSunday = new Date(now);
      lastSunday.setDate(now.getDate() - now.getDay());
      const start = new Date(lastSunday);
      start.setDate(lastSunday.getDate() - 52 * 7);
      const grid = $("heatmap");
      const months = $("heatmap-months");
      grid.innerHTML = "";
      months.innerHTML = "";
      let prevMonth = -1;
      for (let w = 0; w < 53; w++) {
        const wd = new Date(start);
        wd.setDate(start.getDate() + w * 7);
        const m = wd.getMonth();
        const span = document.createElement("span");
        if (m !== prevMonth) {
          span.textContent = String(m + 1);
          span.title = (m + 1) + " 月";
        }
        prevMonth = m;
        months.appendChild(span);
        for (let d = 0; d < 7; d++) {
          const cur = new Date(start);
          cur.setDate(start.getDate() + w * 7 + d);
          const key = dateKey(cur);
          const n = days[key] || 0;
          const cell = document.createElement("div");
          cell.className = "cell";
          cell.style.background = heatColor(n);
          cell.title = key + "：" + n + " 次操作";
          grid.appendChild(cell);
        }
      }
    } catch (e) {}
  }

  /* ---- 动态（2s，严格单行 + title 悬停全文） ---- */
  async function pollActivity() {
    try {
      const v = await api("GET", "/ui/activity");
      const logs = (v && v.logs) || [];
      const box = $("activity-list");
      if (logs.length === 0) {
        box.innerHTML = '<div class="empty">暂无动态</div>';
        return;
      }
      box.innerHTML = logs.map(e => {
        const full = (e.date ? e.date + " " : "") + (e.msg || "");
        return '<div class="act-row">' +
          '<span class="act-time">' + esc(e.time) + "</span>" +
          '<span class="act-msg" title="' + esc(full) + '">' + esc(e.msg) + "</span>" +
          (e.snapshot_id ? '<button type="button" class="btn sm act-revert" data-revert="' + esc(e.snapshot_id) + '">↶ 回滚</button>' : "") +
          "</div>";
      }).join("");
      box.querySelectorAll("[data-revert]").forEach(b => {
        b.addEventListener("click", async () => {
          // v2.3.3：连点保护——后端也会拒绝「同一张快照回滚两次」，
          // 这里先置灰，避免用户以为第一次没生效而反复点。
          if (b.disabled) return;
          b.disabled = true;
          try {
            const r = await api("POST", "/ui/revert", { id: b.getAttribute("data-revert") });
            toast(r && r.ok ? "已回滚" : ("回滚失败：" + ((r && r.error) || "")));
            pollActivity();
          } catch (e) { toast("回滚失败"); b.disabled = false; }
        });
      });
    } catch (e) {}
  }

  /* ---- 后台任务（2s，运行中可一键停止） ---- */
  async function pollTasks() {
    try {
      const v = await api("GET", "/ui/tasks");
      const tasks = (v && v.tasks) || [];
      $("tasks-count").textContent = tasks.length;
      const body = $("tasks-body");
      if (tasks.length === 0) {
        body.innerHTML = '<tr><td colspan="6" class="empty">暂无后台任务（POST /api/tasks/start 启动长任务）</td></tr>';
        return;
      }
      body.innerHTML = tasks.map(t => {
        const badge = t.running ? '<span class="tbadge running">运行中</span>'
          : t.stopped ? '<span class="tbadge stopped">已停止</span>'
          : '<span class="tbadge exited">exit ' + (t.exit_code == null ? "?" : esc(t.exit_code)) + "</span>";
        return "<tr>" +
          '<td class="mono argv">' + esc(t.id) + "</td>" +
          '<td class="argv" title="' + esc((t.argv || []).join(" ")) + '">' + esc((t.argv || []).join(" ")) + "</td>" +
          "<td>" + esc(t.pid) + "</td>" +
          "<td>" + badge + "</td>" +
          "<td>" + esc(t.elapsed_secs) + "s</td>" +
          '<td class="act-cell">' +
            '<button type="button" class="btn sm" data-log="' + esc(t.id) + '">查看输出</button>' +
            (t.running ? '<button type="button" class="btn sm danger" data-stop="' + esc(t.id) + '">停止</button>' : "") +
          "</td>" +
          "</tr>";
      }).join("");
      body.querySelectorAll("[data-log]").forEach(b => {
        b.addEventListener("click", () => {
          const id = b.getAttribute("data-log");
          const row = tasks.find(t => t.id === id);
          openTaskLog(id, row ? row.argv : []);
        });
      });
      body.querySelectorAll("[data-stop]").forEach(b => {
        b.addEventListener("click", async () => {
          try {
            const r = await api("POST", "/ui/tasks/stop", { id: b.getAttribute("data-stop") });
            toast(r && r.ok ? "已停止（Job Object 连根清理）" : ("停止失败：" + ((r && r.error) || "")));
            pollTasks();
          } catch (e) { toast("停止失败"); }
        });
      });
    } catch (e) {}
  }

  /* ---- v2.3.1 后台任务日志弹窗：每 1 秒拉 tail_bytes=16384 刷尾部输出 ---- */
  let taskLogTimer = null;
  let taskLogId = null;
  async function refreshTaskLog() {
    if (!taskLogId) return;
    try {
      const v = await api("GET", "/api/tasks/status?id=" + encodeURIComponent(taskLogId) + "&tail_bytes=16384");
      if (!v || !v.ok) return;
      const out = v.stdout_chunk || "";
      const err = v.stderr_chunk || "";
      const body = $("tasklog-body");
      let html = "";
      if (out) html += '<div><span class="log-head">── stdout（末尾 ' + esc(v.tail_bytes) + ' 字节，共 ' + esc(v.stdout_bytes) + ' 字节）──</span>\n' + esc(out) + "</div>";
      if (err) html += '<div><span class="log-err">── stderr（末尾 ' + esc(v.tail_bytes) + ' 字节，共 ' + esc(v.stderr_bytes) + ' 字节）──</span>\n' + esc(err) + "</div>";
      if (!html) html = '<span class="log-empty">暂无输出（任务刚启动或尚未产生输出）。</span>';
      body.innerHTML = html;
      body.scrollTop = body.scrollHeight;
      $("tasklog-sub").textContent = (v.running ? "运行中" : "已结束") + " · PID " + v.pid + " · 已运行 " + v.elapsed_secs + "s" + (v.exit_code != null ? " · exit " + v.exit_code : "");
    } catch (e) {}
  }
  function openTaskLog(id, argv) {
    taskLogId = id;
    $("tasklog-title").textContent = "任务输出 · " + id;
    $("tasklog-sub").textContent = (argv || []).join(" ");
    $("tasklog-body").innerHTML = '<span class="log-empty">正在拉取输出…</span>';
    $("tasklog-overlay").classList.add("show");
    clearInterval(taskLogTimer);
    refreshTaskLog();
    taskLogTimer = setInterval(refreshTaskLog, 1000);
  }
  function closeTaskLog() {
    clearInterval(taskLogTimer);
    taskLogTimer = null;
    taskLogId = null;
    $("tasklog-overlay").classList.remove("show");
  }

  /* ---- 程序胶囊（5s） ---- */
  let programCache = null;
  let programBusy = false;

  function basename(p) {
    const parts = String(p || "").split(/[\\/]/);
    return parts[parts.length - 1] || String(p || "");
  }

  function renderProgramUnbound(reason) {
    programCache = null;
    $("program-state").textContent = "未绑定";
    $("program-state").className = "pill";
    $("program-body").innerHTML =
      '<div class="prog-empty">' +
      '<div class="empty">' + esc(reason) + "</div>" +
      '<button type="button" class="btn sm" data-act="bind">绑定主程序单文件</button>' +
      "</div>";
  }

  function renderProgram(v) {
    const name = basename(v.path);
    const sha = String(v.sha256 || "").slice(0, 12);
    const lr = v.last_run;
    let exitText = "从未运行";
    let runTitle = "绑定后尚未运行";
    if (lr) {
      const argv = Array.isArray(lr.argv) ? lr.argv.join(" ") : "";
      exitText = lr.exit_code == null ? "exit …" : ("exit " + lr.exit_code);
      if (lr.timed_out) exitText += "（超时）";
      if (lr.async) exitText += "（后台）";
      runTitle = "上次命令：" + argv + " → " + exitText;
    }
    $("program-state").textContent = "已绑定";
    $("program-state").className = "pill on";
    $("program-body").innerHTML =
      '<div class="prog-top">' +
      '<div class="prog-file" title="' + esc(v.path) + '">' + esc(name) + "</div>" +
      '<div class="prog-meta">' +
      '<span class="chip">行数 <b>' + esc(v.line_count) + "</b></span>" +
      '<span class="chip">SHA <b class="mono">' + esc(sha) + "</b></span>" +
      '<span class="chip" title="' + esc(runTitle) + '">最近 <b>' + esc(exitText) + "</b></span>" +
      "</div>" +
      '<div class="prog-actions">' +
      '<button type="button" class="btn sm" data-act="source">查看源码</button>' +
      '<button type="button" class="btn sm primary" data-act="run" title="' + esc(runTitle) + '">立即运行</button>' +
      "</div>" +
      "</div>" +
      '<div class="prog-sub" title="' + esc(v.root + " / " + v.path) + '">' + esc(v.root) + " / " + esc(v.path) + " · " + esc(v.size) + " 字节</div>";
  }

  async function pollProgram() {
    try {
      const v = await api("GET", "/api/program");
      if (!v || !v.ok) {
        renderProgramUnbound((v && v.error) || "未绑定主程序文件");
        return;
      }
      programCache = v;
      renderProgram(v);
    } catch (e) {}
  }

  async function bindProgram() {
    try {
      const p = await invoke("browse_file");
      if (!p) return;
      const v = await api("POST", "/ui/save-config", { main_file: p });
      if (v && v.ok) {
        toast("已绑定主程序文件");
        pollProgram();
        pollState();
      } else {
        toast("绑定失败：" + ((v && v.error) || "未知错误"));
      }
    } catch (e) { toast("绑定失败"); }
  }

  async function runProgram() {
    if (programBusy) return;
    const p = programCache;
    if (!p) { toast("尚未绑定主程序文件"); return; }
    let argv;
    if (p.last_run && Array.isArray(p.last_run.argv) && p.last_run.argv.length > 0) {
      argv = p.last_run.argv.slice();
    } else if (/\.py$/i.test(p.path || "")) {
      argv = ["python", basename(p.path)];
    } else {
      toast("尚无运行记录，请先通过 API 指定命令");
      return;
    }
    programBusy = true;
    try {
      const r = await api("POST", "/api/program", {
        content: p.content,
        expected_sha256: p.sha256,
        run_argv: argv,
        async: true
      });
      if (r && r.ok) {
        if (r.task_id) toast("已启动后台任务 " + r.task_id);
        else if (r.run_error) toast("已写入，但运行失败：" + r.run_error);
        else toast("已写入并启动");
        pollProgram();
        pollTasks();
      } else if (r && (r.current_sha256 || /sha256/i.test(r.error || ""))) {
        toast("文件已被本地修改（409），已刷新最新内容，请重试");
        pollProgram();
      } else {
        toast("运行失败：" + ((r && r.error) || "未知错误"));
      }
    } catch (e) { toast("运行失败"); }
    finally { programBusy = false; }
  }

  function openSource() {
    if (!programCache) { toast("尚未绑定主程序文件"); return; }
    const name = basename(programCache.path);
    $("source-meta").textContent = name + " · " + programCache.line_count + " 行 · SHA " + String(programCache.sha256 || "").slice(0, 12) + " · 只读";
    $("source-pre").textContent = programCache.content || "";
    $("source-overlay").classList.add("show");
  }

  on("program-body", "click", (e) => {
    const btn = e.target.closest("[data-act]");
    if (!btn) return;
    const act = btn.getAttribute("data-act");
    if (act === "bind") bindProgram();
    else if (act === "source") openSource();
    else if (act === "run") runProgram();
  });

  /* ---- 设置 ---- */
  function openSettings() {
    api("GET", "/ui/state").then(v => {
      if (!v || !v.ok) return;
      $("cfg-token").value = v.token || "";
      $("cfg-token").type = "password";
      $("token-eye").textContent = "显示";
      $("cfg-work").value = (v.roots_map && v.roots_map.work) || "";
      $("cfg-hmac").checked = !!v.require_hmac;
      document.querySelectorAll("input[name=perm]").forEach(r => {
        r.checked = r.value === v.permission_mode;
        const item = r.closest(".radio-item");
        if (item) item.classList.toggle("selected", r.checked);
      });
      // v2.3.0：main_file 优先读 /ui/state.main_file（另一个智能体新增的字段）；
      // 字段尚不存在时，回退到 GET /api/program 的绑定路径。
      const hasMainField = Object.prototype.hasOwnProperty.call(v, "main_file");
      if (hasMainField) {
        $("cfg-mainfile").value = v.main_file || "";
      } else {
        api("GET", "/api/program").then(p => {
          $("cfg-mainfile").value = (p && p.ok) ? (p.path || "") : "";
        }).catch(() => {});
      }
      $("settings-msg").textContent = "";
      $("settings-msg").className = "";
      $("settings-overlay").classList.add("show");
    }).catch(() => toast("读取配置失败"));
  }
  on("settings-btn", "click", openSettings);
  on("settings-cancel", "click", () => $("settings-overlay").classList.remove("show"));
  on("settings-overlay", "click", (e) => {
    if (e.target === $("settings-overlay")) $("settings-overlay").classList.remove("show");
  });
  on("source-close", "click", () => $("source-overlay").classList.remove("show"));
  on("source-overlay", "click", (e) => {
    if (e.target === $("source-overlay")) $("source-overlay").classList.remove("show");
  });
  // v2.3.1：任务日志弹窗关闭（按钮 / 点遮罩 / Esc，关闭时停掉 1s 轮询）。
  on("tasklog-close", "click", closeTaskLog);
  on("tasklog-overlay", "click", (e) => {
    if (e.target === $("tasklog-overlay")) closeTaskLog();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      $("settings-overlay").classList.remove("show");
      $("source-overlay").classList.remove("show");
      closeTaskLog();
    }
  });

  document.querySelectorAll("input[name=perm]").forEach(r => {
    r.addEventListener("change", () => {
      document.querySelectorAll(".radio-item").forEach(x => x.classList.remove("selected"));
      const item = r.closest(".radio-item");
      if (item) item.classList.add("selected");
    });
  });
  on("token-eye", "click", () => {
    const i = $("cfg-token");
    i.type = i.type === "password" ? "text" : "password";
    $("token-eye").textContent = i.type === "password" ? "显示" : "隐藏";
  });
  on("token-copy", "click", async () => {
    const ok = await invoke("copy_text", { text: $("cfg-token").value });
    toast(ok ? "已复制到剪贴板" : "剪贴板不可用");
  });
  on("token-regen", "click", () => {
    const hex = Array.from(crypto.getRandomValues(new Uint8Array(16))).map(b => b.toString(16).padStart(2, "0")).join("");
    $("cfg-token").value = hex;
    $("cfg-token").type = "text";
    $("token-eye").textContent = "隐藏";
  });
  on("work-browse", "click", async () => {
    const p = await invoke("browse_folder");
    if (p) $("cfg-work").value = p;
  });
  on("mainfile-browse", "click", async () => {
    const p = await invoke("browse_file");
    if (p) $("cfg-mainfile").value = p;
  });
  on("settings-save", "click", async () => {
    const msg = $("settings-msg");
    const mode = (document.querySelector("input[name=perm]:checked") || {}).value || "smart_auto";
    const body = {
      token: $("cfg-token").value.trim(),
      roots: { work: $("cfg-work").value.trim() },
      permission_mode: mode,
      require_hmac: $("cfg-hmac").checked,
      main_file: $("cfg-mainfile").value.trim() || null
    };
    if (!body.token) { msg.textContent = "token 不能为空"; msg.className = "err"; return; }
    if (!body.roots.work) { msg.textContent = "工作区不能为空"; msg.className = "err"; return; }
    try {
      const v = await api("POST", "/ui/save-config", body);
      if (v && v.ok) {
        msg.textContent = "已保存，0.5 秒内热生效";
        msg.className = "ok";
        toast("配置已保存并热重载");
        setTimeout(() => $("settings-overlay").classList.remove("show"), 700);
        pollState();
        pollProgram();
      } else {
        msg.textContent = (v && v.error) || "保存失败";
        msg.className = "err";
      }
    } catch (e) {
      msg.textContent = "保存失败";
      msg.className = "err";
    }
  });

/* ---- 轮询启动 ---- */
  pollState();
  pollApprovals();
  pollHeatmap();
  pollActivity();
  pollTasks();
  pollProgram();
  setInterval(pollState, 2000);
  setInterval(pollApprovals, 1000);
  setInterval(pollActivity, 2000);
  setInterval(pollTasks, 2000);
  setInterval(pollHeatmap, 10000);
  setInterval(pollProgram, 5000);
})();
