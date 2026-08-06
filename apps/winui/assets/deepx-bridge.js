// DeepX WinUI shell bridge — re-creates `window.deepx` over WebView2 WebMessage.
// Injected into the renderer output (index.html) by apps/winui/scripts/patch-renderer.mjs.
// In Electron, the preload already defines `window.deepx`; this script is a no-op there.
(function () {
  if (window.deepx) return;
  var pending = new Map();
  var subs = new Map();
  var seq = 0;
  // Renderer 异常上报：winui 侧写日志（诊断用，不影响功能）。
  function reportError(msg) {
    try {
      window.chrome.webview.postMessage({ type: 'log', level: 'error', msg: String(msg) });
    } catch (_) {}
  }
  window.addEventListener('error', function (e) { reportError(e.message + ' @' + (e.filename || '') + ':' + (e.lineno || '')); });
  window.addEventListener('unhandledrejection', function (e) {
    var r = e && e.reason;
    reportError('unhandledrejection: ' + (r && r.message ? r.message : String(r)));
  });
  function emit(kind, payload) {
    var set = subs.get(kind);
    if (!set) return;
    set.forEach(function (fn) { try { fn(payload); } catch (e) { console.error('[bridge] listener', e); } });
  }
  window.chrome.webview.addEventListener('message', function (e) {
    var msg = e.data;
    if (typeof msg === 'string') { try { msg = JSON.parse(msg); } catch (_) { return; } }
    if (!msg || typeof msg !== 'object') return;
    if (msg.type === 'response') {
      var p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) { p.resolve(msg.value); } else { p.reject(new Error(msg.error || 'bridge error')); }
    } else if (msg.type === 'event') {
      if (msg.kind === 'shell.navigate') {
        reportError('[bridge] shell.navigate received: ' + JSON.stringify(msg.payload));
      }
      emit(msg.kind, msg.payload);
    }
  });
  function invoke(method, params) {
    return new Promise(function (resolve, reject) {
      var id = ++seq;
      pending.set(id, { resolve: resolve, reject: reject });
      // Pass an object: WebView2 serializes it; WebMessageAsJson then yields
      // the object itself (a JSON *string* arg would come back double-encoded).
      window.chrome.webview.postMessage({ type: 'invoke', id: id, method: method, params: params || {} });
    });
  }
  function sub(kind, fn) {
    var set = subs.get(kind);
    if (!set) { set = new Set(); subs.set(kind, set); }
    set.add(fn);
    return function () { set.delete(fn); };
  }
  window.__DEEPX_XAML_SIDEBAR__ = true; // 原生侧栏接管：renderer 隐藏 web 侧栏（可回退）
  // P-3 统一 flag（WORKFLOW §6.1）：新组件查询 __DEEPX_XAML__.<component>；
  // 旧 __DEEPX_XAML_SIDEBAR__ 保留兼容已上线代码。
  window.__DEEPX_XAML__ = { sidebar: true, header: true, home: true, settings: true, info: true };
  window.deepx = {
    backend: {
      connect: function () { return invoke('backend.connect'); },
      request: function (method, params) { return invoke('backend.request', { method: method, params: params }); },
      restart: function () { return invoke('backend.restart'); },
      attach: function (seed) { return invoke('backend.attach', { seed: seed }); },
      detach: function (seed) { return invoke('backend.detach', { seed: seed }); },
      status: function () { return invoke('backend.status'); },
      onMessage: function (l) { return sub('backend.message', l); },
      onStatus: function (l) { return sub('backend.status', l); }
    },
    ringing: {
      status: function () { return invoke('ringing.status'); },
      bootstrap: function (seed) { return invoke('ringing.bootstrap', { seed: seed }); },
      snapshot: function (seed, channel) { return invoke('ringing.snapshot', { seed: seed, channel: channel }); },
      command: function (seed, channel, envelope) { return invoke('ringing.command', { seed: seed, channel: channel, envelope: envelope }); },
      query: function (path, params) { return invoke('ringing.query', { path: path, params: params }); },
      onBatch: function (l) { return sub('ringing.batch', l); },
      onStatus: function (l) { return sub('ringing.status', l); },
      onSnapshot: function (l) { return sub('ringing.snapshot', l); }
    },
    timeline: {
      activate: function (seed) { return invoke('timeline.activate', { seed: seed }); },
      status: function () { return invoke('timeline.status'); },
      onEntry: function (l) { return sub('timeline.entry', l); },
      onSnapshot: function (l) { return sub('timeline.snapshot', l); },
      onStatus: function (l) { return sub('timeline.status', l); }
    },
    shell: {
      // XAML 侧栏导航事件（host → renderer 单向）：
      //   { view: "home"|"chat"|"skills"|"settings", seed?: string }
      onNavigate: function (l) { return sub('shell.navigate', l); },
      // XAML 标题栏（P0）：Web 状态投影 → 壳 TitleBar（rev 驱动，同侧栏模式）
      setHeader: function (state) { return invoke('shell.setHeader', state || {}); },
      // 壳点击标题栏动作回传（host → renderer 事件）：
      //   { action: "workspace"|"location"|"console"|"info"|"stats"|"undo"|"compact"|"pet", path?: string }
      onHeaderAction: function (l) { return sub('shell.headerAction', l); },
      // 主题同步（P-5 三态）：light | dark | dark-gray | system
      setTheme: function (mode) { return invoke('shell.setTheme', { mode: mode }); },
      // 壳系统主题变化（host → renderer）：{ mode: "light"|"dark" }
      onThemeChanged: function (l) { return sub('shell.themeChanged', l); },
      // XAML 设置页（P2）：Web 初始投影（theme/lang/permission/workspaceMode）
      // → 壳设置页数据源（P-3 模式，同 setHeader）。
      setSettings: function (state) { return invoke('shell.setSettings', state || {}); },
      // 壳设置页动作回传（host → renderer 事件）：
      //   { action: "lang"|"theme"|"permission", lang?|mode?|level? }
      onSettingsAction: function (l) { return sub('shell.settingsAction', l); }
    },
    desktop: {
      openDialog: function (o) { return invoke('desktop.openDialog', o || {}); },
      openImageDialog: function () { return invoke('desktop.openImageDialog'); },
      readFileBase64: function (p) { return invoke('desktop.readFileBase64', { path: p }); },
      readTextFile: function (p) { return invoke('desktop.readTextFile', { path: p }); },
      confirm: function (m, o) { return invoke('desktop.confirm', Object.assign({ message: m }, o || {})); },
      openPath: function (t) { return invoke('desktop.openPath', { target: t }); },
      togglePet: function () { return invoke('desktop.togglePet'); },
      getPetStatus: function () { return invoke('desktop.getPetStatus'); },
      checkUpdate: function () { return invoke('desktop.checkUpdate'); },
      stageUpdate: function (s) { return invoke('desktop.stageUpdate', { source: s }); },
      applyUpdate: function (p) { return invoke('desktop.applyUpdate', { path: p }); },
      openDevTools: function () { return invoke('desktop.openDevTools'); },
      setBackgroundMaterial: function (m) { return invoke('desktop.setBackgroundMaterial', { material: m }); },
      onUpdateAvailable: function (l) { return sub('desktop.updateAvailable', l); },
      onUpdateFailed: function (l) { return sub('desktop.updateFailed', l); }
    }
  };
  console.info('[bridge] window.deepx installed (winui shell)');
  // Pre-connect: the shell owns the daemon lease (mirrors Electron main
  // connecting at startup); renderer subscriptions alone don't trigger it.
  setTimeout(function () { invoke('backend.connect').catch(function () {}); }, 0);
})();
