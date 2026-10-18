// Moonfin admin page: the "Server settings" panel.
//
// Edits the database overrides behind GET/PUT /moonfin/admin/server-settings
// (web app URLs, WebRTC scan, MDBList key) and offers the MDBList key test and
// cache clear. The MDBList key is write-only: it is sent in a request body,
// never put in a URL, never stored in the page, and the input is emptied after
// every save or clear. Everything from the server goes in through textContent.
//
// Loaded before admin.js. admin.js calls init() with its shared helpers.
(function () {
  'use strict';

  var H = null; // helpers from admin.js: $, el, clear, api, confirmDialog, handleError
  var view = null;

  var SOURCE_TEXT = {
    database: 'Set here (database)',
    environment: 'From the server configuration',
    none: 'Not set'
  };

  function say(id, text, bad) {
    var p = H.$(id);
    p.textContent = text || '';
    p.className = bad ? 'bad' : 'muted';
    p.hidden = !text;
  }

  // Drop every transient message in the panel (stale success/failure notes).
  function clearMessages() {
    say('ss-message', '');
    say('ss-cache-result', '');
    var box = H.$('ss-key-result');
    box.hidden = true;
    H.clear(box);
  }

  var messageTimer = null;
  function sayFor(id, text) {
    say(id, text);
    if (messageTimer) clearTimeout(messageTimer);
    messageTimer = setTimeout(function () { say(id, ''); }, 8000);
  }

  function setBadge(id, source, hasValue, envLabel) {
    var b = H.$(id);
    var key = source === 'database' ? 'database' : (hasValue ? 'environment' : 'none');
    var text = SOURCE_TEXT[key];
    if (key === 'environment' && envLabel) text = envLabel;
    b.textContent = text;
    b.className = 'badge' + (key === 'database' ? ' set' : '');
  }

  function paint(v) {
    view = v || {};
    var src = view.sources || {};
    H.$('ss-default-url-input').value = view.webDefaultServerUrl || '';
    H.$('ss-forced-url-input').value = view.webForcedServerUrl || '';
    H.$('ss-webrtc-input').checked = view.webEnableWebrtcScan === true;
    setBadge('ss-default-url-source', src.webDefaultServerUrl, !!view.webDefaultServerUrl);
    setBadge('ss-forced-url-source', src.webForcedServerUrl, !!view.webForcedServerUrl);
    setBadge('ss-webrtc-source', src.webEnableWebrtcScan, true, 'From the server configuration (or its default)');

    var state = H.$('ss-key-state');
    if (view.mdblistKeyConfigured) {
      state.textContent = 'MDBList key: configured (' + (view.mdblistKeySource === 'database' ? 'database' : 'environment') + ').';
    } else {
      state.textContent = 'MDBList key: not set.';
    }
    H.$('ss-key-clear').disabled = view.mdblistKeySource !== 'database';
    updateKeyButtons();
  }

  function updateKeyButtons() {
    H.$('ss-key-save').disabled = H.$('ss-key-input').value.trim() === '';
  }

  function load() {
    return H.api('GET', '/moonfin/admin/server-settings').then(paint).catch(H.handleError);
  }

  // PUT one or more fields; a null value clears the override.
  function put(body, doneMessage) {
    clearMessages();
    return H.api('PUT', '/moonfin/admin/server-settings', JSON.stringify(body)).then(function (v) {
      paint(v);
      sayFor('ss-message', doneMessage);
      H.loadStatus();
    }).catch(function (err) {
      if (err && err.status === 400) say('ss-message', err.message, true);
      else H.handleError(err);
    });
  }

  function wireUrl(field, name, label) {
    var input = H.$('ss-' + name + '-input');
    H.$('ss-' + name + '-save').addEventListener('click', function () {
      var value = input.value.trim();
      var body = {};
      body[field] = value === '' ? null : value;
      put(body, value === '' ? label + ' cleared.' : label + ' saved.');
    });
    H.$('ss-' + name + '-clear').addEventListener('click', function () {
      var body = {};
      body[field] = null;
      put(body, label + ' cleared.');
    });
  }

  function limitsLine(label, value) {
    return H.el('div', { class: 'tile' }, [
      H.el('div', { class: 'k', text: label }),
      H.el('div', { class: 'v', text: value })
    ]);
  }

  function num(v) { return typeof v === 'number' && isFinite(v); }

  function showKeyResult(r) {
    var box = H.$('ss-key-result');
    H.clear(box);
    box.hidden = false;
    box.className = 'ss-key-result';
    if (!r || r.ok !== true) {
      box.appendChild(H.el('div', { class: 'bad', text: 'Key test failed: ' + ((r && r.error) || 'unknown error') }));
      return;
    }
    box.appendChild(H.el('div', { class: 'yes', text: 'The key works.' }));
    var l = r.limits || {};
    var rl = l.rateLimit || {};
    if (num(rl.remaining) || num(rl.limit)) {
      var text = (num(rl.remaining) ? rl.remaining : '?') + ' of ' + (num(rl.limit) ? rl.limit : '?') + ' remaining';
      if (num(rl.reset)) text += ' (reset: ' + rl.reset + ')';
      box.appendChild(limitsLine('Rate limit', text));
    }
    if (num(l.apiRequestsCount) || num(l.apiRequests)) {
      var reqText = num(l.apiRequestsCount)
        ? l.apiRequestsCount + (num(l.apiRequests) ? ' of ' + l.apiRequests : '') + ' used'
        : 'plan allows ' + l.apiRequests;
      box.appendChild(limitsLine('Requests', reqText));
    }
    if (l.plan !== undefined && l.plan !== null) {
      var plan = typeof l.plan === 'object' ? JSON.stringify(l.plan) : String(l.plan);
      box.appendChild(limitsLine('Plan', plan));
    }
  }

  function testKey() {
    var candidate = H.$('ss-key-input').value.trim();
    var btn = H.$('ss-key-test');
    clearMessages();
    btn.disabled = true;
    var box = H.$('ss-key-result');
    box.hidden = false;
    H.clear(box);
    box.appendChild(H.el('div', { class: 'muted', text: 'Testing...' }));
    var body = candidate ? JSON.stringify({ apiKey: candidate }) : undefined;
    H.api('POST', '/moonfin/admin/mdblist/test', body).then(showKeyResult).catch(function (err) {
      box.hidden = true;
      if (err && err.status === 400) showKeyResult({ ok: false, error: err.message });
      else H.handleError(err);
    }).then(function () { btn.disabled = false; });
  }

  function clearCache() {
    H.confirmDialog('Clear the MDBList ratings cache? Cached ratings are deleted and fetched again from MDBList when needed, which uses your MDBList request quota.').then(function (ok) {
      if (!ok) return;
      clearMessages();
      H.api('POST', '/moonfin/admin/mdblist/clear-cache').then(function (r) {
        r = r || {};
        sayFor('ss-cache-result', 'Cleared ' + (r.clearedRatings || 0) + ' cached ratings, ' +
          (r.clearedFailures || 0) + ' remembered failures and ' + (r.clearedRateLimits || 0) + ' rate-limit waits.');
      }).catch(H.handleError);
    });
  }

  function init(helpers) {
    H = helpers;
    wireUrl('webDefaultServerUrl', 'default-url', 'Default server URL');
    wireUrl('webForcedServerUrl', 'forced-url', 'Forced server URL');
    H.$('ss-webrtc-save').addEventListener('click', function () {
      var on = H.$('ss-webrtc-input').checked;
      put({ webEnableWebrtcScan: on }, 'WebRTC scan ' + (on ? 'enabled' : 'disabled') + '.');
    });
    H.$('ss-webrtc-clear').addEventListener('click', function () {
      put({ webEnableWebrtcScan: null }, 'WebRTC scan setting cleared.');
    });

    ['ss-default-url-input', 'ss-forced-url-input', 'ss-key-input'].forEach(function (id) {
      H.$(id).addEventListener('input', clearMessages);
    });
    H.$('ss-webrtc-input').addEventListener('change', clearMessages);
    H.$('ss-key-input').addEventListener('input', updateKeyButtons);
    H.$('ss-key-save').addEventListener('click', function () {
      var input = H.$('ss-key-input');
      var key = input.value.trim();
      if (!key) return;
      input.value = '';
      updateKeyButtons();
      H.$('ss-key-result').hidden = true;
      put({ mdblistApiKey: key }, 'MDBList key saved.');
    });
    H.$('ss-key-clear').addEventListener('click', function () {
      H.confirmDialog('Remove the stored MDBList key? The key from the server configuration is used instead, if there is one.').then(function (ok) {
        if (!ok) return;
        H.$('ss-key-input').value = '';
        H.$('ss-key-result').hidden = true;
        put({ mdblistApiKey: null }, 'Stored MDBList key cleared.');
      });
    });
    H.$('ss-key-test').addEventListener('click', testKey);
    H.$('ss-cache-clear').addEventListener('click', clearCache);
    H.$('ss-refresh').addEventListener('click', load);
  }

  window.MoonfinAdminServerSettings = { init: init, load: load };
})();
