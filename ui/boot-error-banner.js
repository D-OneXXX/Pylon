(function () {
  function showJsError(msg) {
    var b = document.getElementById("js-error-banner");
    if (!b) return;
    b.style.display = "block";
    var t = document.getElementById("js-error-text");
    if (t) t.textContent = msg;
  }
  window.addEventListener("error", function (e) {
    showJsError((e.message || e.error) + (e.filename ? "（" + e.filename + ":" + e.lineno + ":" + e.colno + "）" : ""));
  });
  window.addEventListener("unhandledrejection", function (e) {
    var r = e.reason;
    showJsError("未处理的异步错误：" + (r && (r.message || r) || r));
  });
  window.__pylonShowJsError = showJsError;
  // v2.3.3：关闭按钮改用事件绑定（CSP 下内联 onclick 会被拦）。
  // 这段脚本在横幅元素之后加载，元素一定已存在；仍做一次空值保护。
  var closeBtn = document.getElementById("js-error-close");
  if (closeBtn) {
    closeBtn.addEventListener("click", function () {
      var b = document.getElementById("js-error-banner");
      if (b) b.style.display = "none";
    });
  }
})();
