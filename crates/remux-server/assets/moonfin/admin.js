// Moonfin admin page. Plain DOM building only: every value that came from the
// server (user names, theme JSON, settings) is inserted with textContent.
(function () {
  'use strict';

  var TOKEN_KEY = 'moonfin_admin_token';
  var DEVICE_KEY = 'moonfin_admin_device_id';
  // The Remux dashboard keeps its login here (same origin).
  var DASHBOARD_KEY = 'jellyfin_credentials';
  var VERSION = '1.0';

  var token = '';
  var defaultsOriginal = '';
  var blockedKeys = [];
  var coresTimer = null;
  var selectedUser = null;

  function $(id) { return document.getElementById(id); }

  function el(tag, props, children) {
    var node = document.createElement(tag);
    if (props) {
      Object.keys(props).forEach(function (k) {
        if (k === 'text') node.textContent = props[k];
        else if (k === 'class') node.className = props[k];
        else node.setAttribute(k, props[k]);
      });
    }
    (children || []).forEach(function (c) { if (c) node.appendChild(c); });
    return node;
  }

  function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

  function store(kind, key, value) {
    try {
      var s = kind === 'session' ? window.sessionStorage : window.localStorage;
      if (value === undefined) return s.getItem(key);
      if (value === null) s.removeItem(key); else s.setItem(key, value);
    } catch (e) { /* storage unavailable */ }
    return null;
  }

  function deviceId() {
    var id = store('local', DEVICE_KEY);
    if (!id) {
      id = (window.crypto && crypto.randomUUID) ? crypto.randomUUID()
        : String(Date.now()) + Math.random().toString(16).slice(2);
      store('local', DEVICE_KEY, id);
    }
    return id;
  }

  function authHeader(withToken) {
    var h = 'MediaBrowser Client="Moonfin Admin", Device="Browser", DeviceId="' +
      deviceId() + '", Version="' + VERSION + '"';
    if (withToken && token) h += ', Token="' + token + '"';
    return h;
  }

  // An admin already signed in to the Remux dashboard has a token here.
  function dashboardToken() {
    try {
      var raw = store('local', DASHBOARD_KEY);
      if (!raw) return '';
      var servers = JSON.parse(raw).Servers || [];
      for (var i = 0; i < servers.length; i++) {
        var s = servers[i];
        var origin = '';
        try { origin = s.ManualAddress ? new URL(s.ManualAddress, location.href).origin : location.origin; }
        catch (e) { origin = ''; }
        if (origin === location.origin && typeof s.AccessToken === 'string' && s.AccessToken) {
          return s.AccessToken;
        }
      }
    } catch (e) { /* ignore a malformed entry */ }
    return '';
  }

  // ---- API -------------------------------------------------------------

  function ApiError(status, message, body) {
    this.status = status; this.message = message; this.body = body;
  }

  function api(method, path, body, raw) {
    var headers = { 'Authorization': authHeader(true) };
    var init = { method: method, headers: headers, cache: 'no-store' };
    if (body !== undefined) {
      init.body = body;
      headers['Content-Type'] = raw || 'application/json';
    }
    return fetch(path, init).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        if (!res.ok) {
          var msg = (data && (data.error || data.Error || data.message)) || res.statusText || 'Request failed';
          var e = new ApiError(res.status, msg, data);
          e.path = path;
          throw e;
        }
        clearFailure(path);
        return data;
      });
    });
  }

  // Error banner state, keyed by request path, so a later successful call to
  // the same endpoint clears the stale message.
  var failures = {};
  var failureShown = false;

  function renderFailures() {
    var keys = Object.keys(failures);
    if (keys.length) {
      showBanner(failures[keys[keys.length - 1]], true);
      failureShown = true;
    } else if (failureShown) {
      showBanner('', false);
    }
  }

  function clearFailure(path) {
    if (!(path in failures)) return;
    delete failures[path];
    renderFailures();
  }

  function handleError(err) {
    if (err && err.status === 401) { logout('Your session expired. Sign in again.'); return; }
    var msg = (err && err.status === 403)
      ? 'This account is not a Remux administrator. Sign in with an admin account.'
      : ((err && err.message) || 'Request failed');
    failures[(err && err.path) || '_'] = msg;
    renderFailures();
  }

  // ---- shell -----------------------------------------------------------

  function showBanner(text, bad) {
    failureShown = false;
    var b = $('banner');
    b.textContent = text;
    b.className = bad ? 'banner bad' : 'banner';
    b.hidden = !text;
  }

  function showLogin(message) {
    $('app').hidden = true;
    $('logout').hidden = true;
    $('login').hidden = false;
    showBanner(message || '', false);
  }

  function showApp() {
    failures = {};
    $('login').hidden = true;
    $('app').hidden = false;
    $('logout').hidden = false;
    showBanner('', false);
    loadAll();
  }

  function logout(message) {
    failures = {};
    token = '';
    store('session', TOKEN_KEY, null);
    stopCoresPolling();
    showLogin(message);
  }

  function confirmDialog(text) {
    return new Promise(function (resolve) {
      var dlg = $('confirm');
      $('confirm-text').textContent = text;
      function done(v) {
        $('confirm-ok').onclick = null; $('confirm-cancel').onclick = null;
        if (dlg.open) dlg.close();
        resolve(v);
      }
      $('confirm-ok').onclick = function () { done(true); };
      $('confirm-cancel').onclick = function () { done(false); };
      dlg.oncancel = function () { done(false); };
      if (typeof dlg.showModal === 'function') dlg.showModal();
      else done(window.confirm(text));
    });
  }

  function fmtTime(v) {
    if (v === null || v === undefined || v === '') return 'never';
    var d = typeof v === 'number' ? new Date(v) : new Date(String(v));
    return isNaN(d.getTime()) ? String(v) : d.toLocaleString();
  }

  function fmtBytes(n) {
    if (typeof n !== 'number') return '';
    if (n < 1024) return n + ' B';
    if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
    return (n / 1048576).toFixed(1) + ' MB';
  }

  function preview(v) {
    var s = typeof v === 'string' ? JSON.stringify(v) : JSON.stringify(v);
    if (s === undefined) s = String(v);
    return s.length > 120 ? s.slice(0, 117) + '...' : s;
  }

  function loadAll() {
    loadStatus(); loadDefaults(); loadUsers(); loadThemes();
  }

  // ---- status ----------------------------------------------------------

  function tile(k, v, cls) {
    return el('div', { class: 'tile' }, [
      el('div', { class: 'k', text: k }),
      el('div', { class: 'v ' + (cls || ''), text: v })
    ]);
  }

  function loadStatus() {
    return api('GET', '/moonfin/admin/status').then(function (s) {
      var body = $('status-body');
      clear(body);
      var ping = s.ping || {}, games = s.games || {}, meta = s.metadata || {};
      var lb = meta.launchbox || {}, lbi = lb.index || {};
      var t = function (k, b) { body.appendChild(tile(k, b ? 'Yes' : 'No', b ? 'yes' : 'no')); };
      body.appendChild(tile('Plugin', (ping.serverName || 'Remux') + ' ' + (ping.version || '')));
      t('Plugin installed', ping.installed);
      t('Settings sync', ping.settingsSyncEnabled);
      t('Recommendations', ping.recommendationsSupported);
      t('Games enabled', games.enabled);
      t('Games active', games.active);
      body.appendChild(tile('Game roots', String(games.rootsCount || 0)));
      body.appendChild(tile('Games found', String(games.gameCount || 0)));
      t('Libretro metadata', meta.libretro);
      t('LaunchBox enabled', lb.enabled);
      t('LaunchBox indexed', lbi.indexed);
      t('LaunchBox updating', lbi.updating);
      t('MDBList key configured', s.mdblistConfigured);
      t('Web app bundle present', s.webAppPresent);
      body.appendChild(tile('Themes', String(s.themesCount || 0)));
      body.appendChild(tile('Default settings keys', String(s.defaultsKeyCount || 0)));
      if (s.webAppPresent) {
        var link = el('a', { href: '/Moonfin/Web/', text: 'Open /Moonfin/Web/' });
        body.appendChild(el('div', { class: 'tile' }, [el('div', { class: 'k', text: 'Web app' }), link]));
      }
      t('EmulatorJS CDN fallback', s.emulatorjsCdnFallback);
      renderCores(s.cores || {});
      renderCdnNote(s);
    }).catch(handleError);
  }

  function renderCdnNote(s) {
    var note = $('cdn-note');
    clear(note);
    var installed = !!(s.cores && s.cores.installed);
    note.className = 'muted';
    if (installed) {
      note.appendChild(document.createTextNode('The web player uses the installed cores; the CDN fallback is not used.'));
    } else if (s.emulatorjsCdnFallback) {
      note.appendChild(document.createTextNode('Cores are not installed, so the web player loads EmulatorJS from cdn.emulatorjs.org. The CDN scripts run on this origin, next to stored sign-in tokens, so a compromised CDN could steal them. Install the cores to turn this off.'));
    } else {
      note.className = 'bad';
      note.appendChild(document.createTextNode("The web player won't work until the cores are installed. Install them below. (GAMES_EMULATORJS_CDN_FALLBACK=true would load them from the CDN instead, which is not recommended.)"));
    }
  }

  function renderCores(c) {
    var body = $('cores-body');
    clear(body);
    var line = 'State: ' + (c.state || 'unknown') + (c.installed ? ' (installed, ' + (c.filesInstalled || 0) + ' files)' : ' (not installed)');
    body.appendChild(el('div', { text: line }));
    if (c.error) body.appendChild(el('div', { class: 'bad', text: c.error }));
    if (c.downloading) {
      var pct = c.totalBytes ? Math.min(100, Math.round(100 * (c.bytesDownloaded || 0) / c.totalBytes)) : 0;
      var fill = el('div');
      fill.style.width = pct + '%';
      body.appendChild(el('div', { class: 'bar-progress' }, [fill]));
      body.appendChild(el('div', { class: 'muted', text: fmtBytes(c.bytesDownloaded) + (c.totalBytes ? ' / ' + fmtBytes(c.totalBytes) : '') }));
      startCoresPolling();
    } else {
      stopCoresPolling();
    }
    $('cores-install').disabled = !!c.downloading;
    $('cores-upload').disabled = !!c.downloading;
  }

  function startCoresPolling() {
    if (coresTimer) return;
    coresTimer = setInterval(function () {
      api('GET', '/moonfin/games/cores/status').then(function (c) {
        renderCores(c);
        if (!c.downloading) loadStatus();
      }).catch(function (e) { stopCoresPolling(); handleError(e); });
    }, 1500);
  }

  function stopCoresPolling() {
    if (coresTimer) { clearInterval(coresTimer); coresTimer = null; }
  }

  function coresAction(promise) {
    promise.then(function (c) { renderCores(c); startCoresPolling(); }).catch(handleError);
  }

  // ---- defaults --------------------------------------------------------

  function pretty(obj) { return JSON.stringify(obj, null, 2); }

  function loadDefaults() {
    return api('GET', '/moonfin/admin/defaults').then(function (d) {
      blockedKeys = d.blockedKeys || [];
      var b = $('blocked');
      b.hidden = !blockedKeys.length;
      b.textContent = 'Not allowed as defaults (managed by Remux user settings): ' + blockedKeys.join(', ');
      defaultsOriginal = pretty(d.settings || {});
      $('defaults-text').value = defaultsOriginal;
      $('defaults-meta').textContent = d.updatedAt ? 'Last saved ' + fmtTime(d.updatedAt) : 'Nothing saved yet';
      setErrors('defaults-errors', [], '');
      renderDefaultsTable(d.settings || {});
    }).catch(handleError);
  }

  function renderDefaultsTable(settings) {
    var tbody = $('defaults-table').querySelector('tbody');
    clear(tbody);
    var keys = Object.keys(settings).sort();
    if (!keys.length) {
      tbody.appendChild(el('tr', null, [el('td', { text: 'No defaults set.', colspan: '3' })]));
      return;
    }
    keys.forEach(function (k) {
      var del = el('button', { type: 'button', class: 'danger', text: 'Delete' });
      del.addEventListener('click', function () {
        confirmDialog('Remove the default for "' + k + '"?').then(function (ok) {
          if (!ok) return;
          var next = Object.assign({}, settings);
          delete next[k];
          saveDefaults(next);
        });
      });
      tbody.appendChild(el('tr', null, [
        el('td', null, [el('code', { text: k })]),
        el('td', { text: preview(settings[k]) }),
        el('td', null, [del])
      ]));
    });
  }

  function setErrors(id, errors, okText) {
    var ul = $(id);
    clear(ul);
    ul.className = 'errors' + (errors.length ? '' : ' ok');
    var items = errors.length ? errors : (okText ? [okText] : []);
    items.forEach(function (e) { ul.appendChild(el('li', { text: e })); });
    ul.hidden = !items.length;
  }

  // Returns the parsed object, or null after showing why it is not valid.
  function validateDefaultsLocal() {
    var obj;
    try { obj = JSON.parse($('defaults-text').value || '{}'); }
    catch (e) { setErrors('defaults-errors', ['Invalid JSON: ' + e.message]); return null; }
    if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) {
      setErrors('defaults-errors', ['Settings must be a JSON object.']); return null;
    }
    var bad = Object.keys(obj).filter(function (k) { return blockedKeys.indexOf(k) >= 0; });
    if (bad.length) {
      setErrors('defaults-errors', ['These keys are managed by Remux user settings and cannot be defaults: ' + bad.join(', ')]);
      return null;
    }
    var hs = 'homeSections' in obj, ho = 'homeRowOrder' in obj;
    if (hs !== ho) {
      setErrors('defaults-errors', ['homeSections and homeRowOrder are defaults that must be set together as a pair.']);
      return null;
    }
    return obj;
  }

  function saveDefaults(obj) {
    return api('PUT', '/moonfin/admin/defaults', JSON.stringify({ settings: obj })).then(function (d) {
      $('defaults-text').value = pretty(d.settings || {});
      defaultsOriginal = $('defaults-text').value;
      $('defaults-meta').textContent = 'Last saved ' + fmtTime(d.updatedAt);
      renderDefaultsTable(d.settings || {});
      setErrors('defaults-errors', [], 'Saved.');
      loadStatus();
    }).catch(function (err) {
      if (err && err.status === 400) setErrors('defaults-errors', [err.message]);
      else handleError(err);
    });
  }

  // ---- users -----------------------------------------------------------

  function loadUsers() {
    return api('GET', '/moonfin/admin/users').then(function (d) {
      var tbody = $('users-table').querySelector('tbody');
      clear(tbody);
      var items = d.items || [];
      if (!items.length) tbody.appendChild(el('tr', null, [el('td', { text: 'No users.', colspan: '3' })]));
      items.forEach(function (u) {
        var profiles = (u.profiles || []).map(function (p) {
          return p.profile + ' (' + fmtTime(p.updatedAt) + ')';
        });
        var devices = (u.devices || []).map(function (dv) {
          return (dv.name || dv.id) + ' - ' + (dv.appName || '') + ' ' + (dv.appVersion || '') + ', last active ' + fmtTime(dv.lastActivityAt);
        });
        var tr = el('tr', { class: 'click' + (selectedUser === u.userId ? ' sel' : ''), tabindex: '0' }, [
          el('td', { 'data-label': 'User', text: u.name || u.userId }),
          el('td', { 'data-label': 'Profiles', text: profiles.length ? profiles.join('; ') : 'none' }),
          el('td', { 'data-label': 'Moonfin devices', text: devices.length ? devices.join('\n') : 'none', class: 'devs' })
        ]);
        var open = function () { openUser(u.userId, u.name); };
        tr.addEventListener('click', open);
        tr.addEventListener('keydown', function (e) { if (e.key === 'Enter') open(); });
        tbody.appendChild(tr);
      });
    }).catch(handleError);
  }

  function jsonDetails(title, obj) {
    var d = el('details');
    d.appendChild(el('summary', { text: title }));
    d.appendChild(el('pre', { text: pretty(obj) }));
    return d;
  }

  function openUser(id, name) {
    selectedUser = id;
    api('GET', '/moonfin/admin/users/' + encodeURIComponent(id) + '/settings').then(function (v) {
      $('user-detail').hidden = false;
      $('user-title').textContent = v.name || name || id;
      var box = $('user-profiles');
      clear(box);
      ['global', 'desktop', 'mobile', 'tv'].forEach(function (p) {
        var stored = (v.profiles || {})[p];
        var resolved = (v.resolved || {})[p];
        var head = el('div', { class: 'head' }, [
          el('h3', { text: p + (stored ? ' (updated ' + fmtTime(stored.updatedAt) + ')' : ' (no stored settings)') })
        ]);
        if (stored) {
          var reset = el('button', { type: 'button', class: 'danger', text: 'Reset ' + p });
          reset.addEventListener('click', function () {
            confirmDialog('Reset the ' + p + ' profile for ' + (v.name || id) + '? Their stored settings for it are deleted.').then(function (ok) {
              if (ok) resetUser(id, p);
            });
          });
          head.appendChild(reset);
        }
        box.appendChild(head);
        if (stored) box.appendChild(jsonDetails('Stored settings', stored.settings));
        box.appendChild(jsonDetails('Resolved settings (defaults + stored)', resolved || {}));
      });
      $('user-reset-all').onclick = function () {
        confirmDialog('Reset ALL Moonfin profiles for ' + (v.name || id) + '?').then(function (ok) {
          if (ok) resetUser(id, null);
        });
      };
      loadUsers();
    }).catch(handleError);
  }

  function resetUser(id, profile) {
    var q = profile ? '?profile=' + encodeURIComponent(profile) : '';
    api('DELETE', '/moonfin/admin/users/' + encodeURIComponent(id) + '/settings' + q).then(function (r) {
      showBanner('Deleted ' + r.deleted + ' stored profile(s).', false);
      openUser(id);
    }).catch(handleError);
  }

  // ---- themes ----------------------------------------------------------

  function loadThemes() {
    return api('GET', '/moonfin/admin/themes').then(function (d) {
      var tbody = $('themes-table').querySelector('tbody');
      clear(tbody);
      var items = d.items || [];
      if (!items.length) tbody.appendChild(el('tr', null, [el('td', { text: 'No themes uploaded.', colspan: '5' })]));
      items.forEach(function (t) {
        var del = el('button', { type: 'button', class: 'danger', text: 'Delete' });
        del.addEventListener('click', function () {
          confirmDialog('Delete theme "' + (t.displayName || t.id) + '"?').then(function (ok) {
            if (!ok) return;
            api('DELETE', '/moonfin/admin/themes/' + encodeURIComponent(t.id)).then(loadThemes).then(loadStatus).catch(handleError);
          });
        });
        tbody.appendChild(el('tr', null, [
          el('td', { text: t.displayName || '' }),
          el('td', null, [el('code', { text: t.id })]),
          el('td', { text: fmtBytes(t.sizeBytes) }),
          el('td', { text: fmtTime(t.uploadedAtUtc) }),
          el('td', null, [del])
        ]));
      });
    }).catch(handleError);
  }

  function uploadTheme() {
    var text = $('theme-text').value.trim();
    if (!text) { setErrors('theme-errors', ['Choose a file or paste theme JSON first.']); return; }
    api('POST', '/moonfin/admin/themes', text).then(function (r) {
      setErrors('theme-errors', [], 'Uploaded "' + ((r.item && r.item.displayName) || 'theme') + '".');
      $('theme-text').value = '';
      $('theme-file').value = '';
      loadThemes(); loadStatus();
    }).catch(function (err) {
      if (err && err.status === 401) { handleError(err); return; }
      if (err && err.status === 403) { handleError(err); return; }
      var list = (err.body && err.body.errors) || [];
      setErrors('theme-errors', [err.message].concat(list));
    });
  }

  function readFile(input, target) {
    var f = input.files && input.files[0];
    if (!f) return;
    var r = new FileReader();
    r.onload = function () { target.value = String(r.result || ''); };
    r.readAsText(f);
  }

  // ---- login -----------------------------------------------------------

  function login(e) {
    e.preventDefault();
    var user = $('login-user').value, pw = $('login-pass').value;
    fetch('/Users/AuthenticateByName', {
      method: 'POST',
      headers: { 'Authorization': authHeader(false), 'Content-Type': 'application/json' },
      body: JSON.stringify({ Username: user, Pw: pw }),
      cache: 'no-store'
    }).then(function (res) {
      if (!res.ok) throw new ApiError(res.status, res.status === 401 ? 'Wrong username or password.' : 'Sign-in failed.');
      return res.json();
    }).then(function (data) {
      token = data.AccessToken;
      $('login-pass').value = '';
      // Only keep the token and show the shell for a Remux administrator.
      return api('GET', '/moonfin/admin/status').then(function () {
        store('session', TOKEN_KEY, token);
        showApp();
      }).catch(function (err) {
        token = '';
        store('session', TOKEN_KEY, null);
        if (err && err.status === 403) {
          showLogin('This account is not a Remux administrator. Sign in with an admin account.');
          $('banner').className = 'banner bad';
        } else {
          throw err;
        }
      });
    }).catch(function (err) { showBanner((err && err.message) || 'Sign-in failed.', true); });
  }

  // ---- wiring ----------------------------------------------------------

  function init() {
    $('login-form').addEventListener('submit', login);
    $('logout').addEventListener('click', function () { logout(''); });
    $('status-refresh').addEventListener('click', loadStatus);
    $('users-refresh').addEventListener('click', loadUsers);
    $('themes-refresh').addEventListener('click', loadThemes);
    $('user-close').addEventListener('click', function () { selectedUser = null; $('user-detail').hidden = true; loadUsers(); });

    $('cores-install').addEventListener('click', function () {
      coresAction(api('POST', '/moonfin/games/cores/install'));
    });
    $('cores-upload').addEventListener('change', function (e) {
      var f = e.target.files && e.target.files[0];
      if (!f) return;
      var p = fetch('/moonfin/games/cores/upload', {
        method: 'POST',
        headers: { 'Authorization': authHeader(true), 'Content-Type': 'application/octet-stream' },
        body: f
      }).then(function (res) {
        return res.json().catch(function () { return {}; }).then(function (data) {
          if (!res.ok) throw new ApiError(res.status, data.Error || data.error || 'Upload failed');
          return data;
        });
      });
      e.target.value = '';
      coresAction(p);
    });

    $('defaults-validate').addEventListener('click', function () {
      if (validateDefaultsLocal()) setErrors('defaults-errors', [], 'Looks valid.');
    });
    $('defaults-save').addEventListener('click', function () {
      var obj = validateDefaultsLocal();
      if (obj) saveDefaults(obj);
    });
    $('defaults-revert').addEventListener('click', function () {
      $('defaults-text').value = defaultsOriginal;
      setErrors('defaults-errors', [], '');
    });

    $('theme-file').addEventListener('change', function (e) { readFile(e.target, $('theme-text')); });
    $('theme-upload').addEventListener('click', uploadTheme);

    token = store('session', TOKEN_KEY) || dashboardToken();
    if (token) {
      // Probe with the status call so a stale token falls back to the login form.
      api('GET', '/moonfin/admin/status').then(function () {
        store('session', TOKEN_KEY, token);
        showApp();
      }).catch(function (err) {
        token = '';
        store('session', TOKEN_KEY, null);
        showLogin(err && err.status === 403
          ? 'This account is not a Remux administrator. Sign in with an admin account.'
          : '');
        if (err && err.status === 403) $('banner').className = 'banner bad';
      });
    } else {
      showLogin('');
    }
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
