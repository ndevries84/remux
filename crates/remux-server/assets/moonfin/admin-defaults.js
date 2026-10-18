// Moonfin admin page: default settings forms and "Apply to users".
//
// The forms are generated from /moonfin/admin/default-options.json (derived
// from Moonbase, see NOTICE). They edit one settings object that the raw JSON
// panel shows and edits too, so the two always agree. Everything that came
// from the server or from stored settings is untrusted and only ever goes
// into the DOM through textContent / setAttribute, never as markup.
//
// Loaded before admin.js. admin.js calls init() with its shared helpers.
(function () {
  'use strict';

  var H = null; // helpers from admin.js: $, el, clear, api, confirmDialog, ...

  var schema = null;     // { groups, controls, excluded } or null when it failed to load
  var settings = {};     // the working copy every control edits
  var stored = {};       // what the server has
  var blockedKeys = [];
  var rows = [];         // one entry per rendered control
  var tabButtons = {};   // group -> tab button
  var panels = {};       // group -> panel element
  var activeGroup = '';
  var query = '';
  var built = false;
  var jsonValid = true;
  var users = [];

  var has = function (o, k) { return Object.prototype.hasOwnProperty.call(o, k); };
  function clone(v) { return v === undefined ? undefined : JSON.parse(JSON.stringify(v)); }
  function same(a, b) { return JSON.stringify(a) === JSON.stringify(b); }
  function isObj(v) { return v !== null && typeof v === 'object' && !Array.isArray(v); }
  function pretty(v) { return JSON.stringify(v, null, 2); }
  function allKeys(def) { return [def.key].concat(def.alsoKeys || []); }

  // ---- settings access -------------------------------------------------

  // The control's current value, or undefined when none of its keys is set.
  function getVal(def) {
    var ks = allKeys(def);
    for (var i = 0; i < ks.length; i++) {
      if (has(settings, ks[i]) && settings[ks[i]] !== null) return settings[ks[i]];
    }
    return undefined;
  }

  // True when the keys that share this control hold different values.
  function keysDiffer(def) {
    var ks = allKeys(def), first;
    for (var i = 0; i < ks.length; i++) {
      var v = has(settings, ks[i]) ? settings[ks[i]] : undefined;
      if (i === 0) first = v; else if (!same(v, first)) return true;
    }
    return false;
  }

  function setVal(def, v) {
    allKeys(def).forEach(function (k) { settings[k] = clone(v); });
  }

  function unsetVal(def) {
    allKeys(def).forEach(function (k) { delete settings[k]; });
  }

  function fits(def, v) {
    switch (def.type) {
      case 'bool': return typeof v === 'boolean';
      case 'enum': return (def.options || []).some(function (o) { return o.value === v; });
      case 'number':
        return def.storeAs === 'string' ? (typeof v === 'string' && v.trim() !== '' && isFinite(Number(v)))
          : (typeof v === 'number' && isFinite(v));
      case 'string': return typeof v === 'string';
      case 'color': return typeof v === 'string' && /^#[0-9a-fA-F]{6}$/.test(v);
      case 'list': return Array.isArray(v) && v.every(function (x) { return typeof x === 'string'; });
      default: return true;
    }
  }

  // ---- control widgets -------------------------------------------------
  // Each builder returns { node, sync(value), focusable } where sync() paints
  // the widget from a value that is known to fit (or undefined for unset).

  function boolWidget(def, id, edit) {
    var cb = H.el('input', { type: 'checkbox', id: id });
    cb.addEventListener('change', function () { edit(cb.checked); });
    return {
      node: cb,
      sync: function (v) {
        cb.indeterminate = v === undefined;
        cb.checked = v === true;
      }
    };
  }

  function enumWidget(def, id, edit, unset) {
    var sel = H.el('select', { id: id });
    sel.appendChild(H.el('option', { value: '', text: 'Not set (app default)' }));
    (def.options || []).forEach(function (o, i) {
      sel.appendChild(H.el('option', { value: String(i), text: o.label }));
    });
    sel.addEventListener('change', function () {
      if (sel.value === '') unset(); else edit(def.options[Number(sel.value)].value);
    });
    return {
      node: sel,
      sync: function (v) {
        var idx = -1;
        (def.options || []).forEach(function (o, i) { if (o.value === v) idx = i; });
        sel.value = idx < 0 ? '' : String(idx);
      }
    };
  }

  function numberWidget(def, id, edit, unset, warn) {
    var input = H.el('input', { type: 'number', id: id, inputmode: 'decimal' });
    if (def.min !== undefined) input.min = String(def.min);
    if (def.max !== undefined) input.max = String(def.max);
    if (def.step !== undefined) input.step = String(def.step);
    if (def.default !== undefined) input.placeholder = String(def.default);
    input.addEventListener('input', function () {
      var raw = input.value.trim();
      if (raw === '') { warn(''); unset(); return; }
      var n = Number(raw);
      if (!isFinite(n) || (def.min !== undefined && n < def.min) || (def.max !== undefined && n > def.max)) {
        warn('Enter a number from ' + def.min + ' to ' + def.max + '. The last valid value is kept.');
        return;
      }
      warn('');
      edit(def.storeAs === 'string' ? String(n) : n);
    });
    return {
      node: input,
      sync: function (v) { input.value = v === undefined ? '' : String(v); }
    };
  }

  function stringWidget(def, id, edit, unset) {
    var input = H.el('input', { type: 'text', id: id, autocomplete: 'off', spellcheck: 'false' });
    input.addEventListener('input', function () {
      if (input.value.trim() === '') unset(); else edit(input.value);
    });
    return { node: input, sync: function (v) { input.value = v === undefined ? '' : v; } };
  }

  function colorWidget(def, id, edit) {
    var input = H.el('input', { type: 'color', id: id });
    input.addEventListener('input', function () { edit(input.value); });
    return { node: input, sync: function (v) { input.value = v === undefined ? '#000000' : v; } };
  }

  function jsonWidget(def, id, edit, unset, warn) {
    var ta = H.el('textarea', { id: id, rows: '5', spellcheck: 'false', 'aria-label': def.label });
    ta.addEventListener('change', function () {
      var raw = ta.value.trim();
      if (raw === '') { warn(''); unset(); return; }
      try { edit(JSON.parse(raw)); warn(''); }
      catch (e) { warn('Not valid JSON: ' + e.message + '. The last valid value is kept.'); }
    });
    return { node: ta, sync: function (v) { ta.value = v === undefined ? '' : pretty(v); } };
  }

  function listWidget(def, id, edit, unset) {
    var wrap = H.el('div', { class: 'listed' });
    var itemsBox = H.el('div', { class: 'items' });
    var addInput = H.el('input', { type: 'text', 'aria-label': 'Add to ' + def.label, autocomplete: 'off', spellcheck: 'false' });
    var addBtn = H.el('button', { type: 'button', text: 'Add' });
    var labels = {};
    var dl = null;
    if (def.suggestions && def.suggestions.length) {
      dl = H.el('datalist', { id: id + '-sugg' });
      def.suggestions.forEach(function (s) {
        labels[s.value] = s.label;
        dl.appendChild(H.el('option', { value: s.value, label: s.label }));
      });
      addInput.setAttribute('list', id + '-sugg');
      addInput.placeholder = 'Type or pick a value';
    }
    var current = [];
    function commitList() {
      if (current.length) edit(current.slice()); else unset();
      paint();
    }
    function paint() {
      H.clear(itemsBox);
      current.forEach(function (item, i) {
        var txt = H.el('input', { type: 'text', value: item, 'aria-label': def.label + ' item ' + (i + 1), spellcheck: 'false' });
        txt.addEventListener('change', function () {
          var v = txt.value.trim();
          if (v === '') current.splice(i, 1); else current[i] = v;
          commitList();
        });
        var up = H.el('button', { type: 'button', text: '↑', 'aria-label': 'Move up', title: 'Move up' });
        var down = H.el('button', { type: 'button', text: '↓', 'aria-label': 'Move down', title: 'Move down' });
        var del = H.el('button', { type: 'button', text: '×', 'aria-label': 'Remove', title: 'Remove' });
        up.disabled = i === 0;
        down.disabled = i === current.length - 1;
        up.addEventListener('click', function () { current.splice(i - 1, 0, current.splice(i, 1)[0]); commitList(); });
        down.addEventListener('click', function () { current.splice(i + 1, 0, current.splice(i, 1)[0]); commitList(); });
        del.addEventListener('click', function () { current.splice(i, 1); commitList(); });
        var rowKids = [txt, up, down, del];
        var line = H.el('div', { class: 'item' }, rowKids);
        if (has(labels, item)) line.appendChild(H.el('span', { class: 'muted', text: labels[item] }));
        itemsBox.appendChild(line);
      });
      var full = def.maxItems && current.length >= def.maxItems;
      addInput.disabled = !!full;
      addBtn.disabled = !!full;
    }
    function add() {
      var v = addInput.value.trim();
      if (!v || current.indexOf(v) >= 0) { addInput.value = ''; return; }
      if (def.maxItems && current.length >= def.maxItems) return;
      current.push(v);
      addInput.value = '';
      commitList();
    }
    addBtn.addEventListener('click', add);
    addInput.addEventListener('keydown', function (e) { if (e.key === 'Enter') { e.preventDefault(); add(); } });
    var addRow = H.el('div', { class: 'row' }, [addInput, addBtn]);
    wrap.appendChild(itemsBox);
    wrap.appendChild(addRow);
    if (dl) wrap.appendChild(dl);
    return {
      node: wrap,
      sync: function (v) { current = Array.isArray(v) ? v.slice() : []; paint(); }
    };
  }

  var WIDGETS = {
    bool: boolWidget, enum: enumWidget, number: numberWidget, string: stringWidget,
    color: colorWidget, json: jsonWidget, list: listWidget
  };

  // ---- one control row -------------------------------------------------

  function buildRow(def) {
    var id = 'opt-' + def.key;
    var badge = H.el('span', { class: 'badge' });
    var reset = H.el('button', { type: 'button', class: 'reset', text: 'Reset to unset' });
    var warnEl = H.el('p', { class: 'warn', hidden: 'hidden' });
    var node = H.el('div', { class: 'opt' });

    function warn(msg) { warnEl.textContent = msg; warnEl.hidden = !msg; }
    function edit(v) {
      setVal(def, v);
      state();
      changed();
    }
    function unset() {
      unsetVal(def);
      state();
      changed();
    }
    var widget = WIDGETS[def.type](def, id, edit, unset, warn);

    // The badge and the reset button follow the settings object.
    function state() {
      var v = getVal(def);
      var isSet = v !== undefined;
      node.className = 'opt' + (isSet ? ' is-set' : '');
      reset.hidden = !isSet;
      if (!isSet) {
        badge.className = 'badge unset';
        badge.textContent = 'App default (not set)';
      } else if (!fits(def, v)) {
        badge.className = 'badge odd';
        badge.textContent = 'Set as default (unrecognised value)';
      } else {
        badge.className = 'badge set';
        badge.textContent = 'Set as default' + (def.type === 'bool' ? ': ' + (v ? 'Yes' : 'No') : '');
      }
    }

    // Repaint the whole row from the settings object.
    function sync() {
      var v = getVal(def);
      var odd = v !== undefined && !fits(def, v);
      widget.sync(odd ? undefined : v);
      var msg = '';
      if (odd) msg = 'The stored value does not fit this control: ' + H.preview(v) + '. Edit it in Advanced (JSON) or reset it.';
      else if (v !== undefined && keysDiffer(def)) msg = 'The related keys hold different values (' + allKeys(def).join(', ') + '). Changing this control overwrites all of them.';
      warn(msg);
      state();
    }

    reset.addEventListener('click', function () { unset(); sync(); });

    var labelFor = ['bool', 'enum', 'number', 'string', 'color', 'json'].indexOf(def.type) >= 0;
    var label = labelFor ? H.el('label', { for: id, class: 'opt-label', text: def.label })
      : H.el('span', { class: 'opt-label', text: def.label });
    node.appendChild(H.el('div', { class: 'opt-head' }, [label, H.el('code', { text: def.key })]));
    node.appendChild(H.el('div', { class: 'opt-body' }, [widget.node]));
    node.appendChild(H.el('div', { class: 'opt-meta' }, [badge, reset]));
    if (def.help) node.appendChild(H.el('p', { class: 'muted help', text: def.help }));
    if (def.type === 'number') {
      var rangeText = 'Range ' + def.min + ' to ' + def.max + (def.default !== undefined ? '; Moonbase starts at ' + def.default : '');
      node.appendChild(H.el('p', { class: 'muted help', text: rangeText }));
    }
    node.appendChild(warnEl);

    var hay = (def.label + ' ' + def.key + ' ' + (def.section || '')).toLowerCase();
    return { def: def, node: node, sync: sync, state: state, hay: hay, section: null };
  }

  // ---- building the page -----------------------------------------------

  function buildForms() {
    var tabs = H.$('defaults-tabs');
    var host = H.$('defaults-panels');
    H.clear(tabs); H.clear(host);
    rows = []; tabButtons = {}; panels = {};
    var groups = schema.groups.filter(function (g) {
      return schema.controls.some(function (c) { return c.group === g; });
    });
    groups.forEach(function (g) {
      var btn = H.el('button', { type: 'button', role: 'tab', class: 'tab' });
      btn.addEventListener('click', function () { selectGroup(g); });
      tabs.appendChild(btn);
      tabButtons[g] = btn;
      var panel = H.el('div', { class: 'panel', role: 'tabpanel' });
      panel.appendChild(H.el('h3', { class: 'panel-title', text: g }));
      host.appendChild(panel);
      panels[g] = panel;
      var secNode = null, secName = null;
      schema.controls.forEach(function (def) {
        if (def.group !== g) return;
        if (secNode === null || def.section !== secName) {
          secName = def.section;
          secNode = H.el('div', { class: 'sect' });
          if (secName) secNode.appendChild(H.el('h4', { text: secName }));
          panel.appendChild(secNode);
        }
        var row = buildRow(def);
        row.section = secNode;
        row.group = g;
        rows.push(row);
        secNode.appendChild(row.node);
      });
    });
    activeGroup = groups[0] || '';
    built = true;
    renderExcluded();
  }

  function selectGroup(g) {
    activeGroup = g;
    applyFilter();
  }

  function applyFilter() {
    var q = query.trim().toLowerCase();
    var shownIn = {}, sectionShown = [];
    rows.forEach(function (r) {
      var show = !q || r.hay.indexOf(q) >= 0;
      r.node.hidden = !show;
      if (show) { shownIn[r.group] = (shownIn[r.group] || 0) + 1; if (sectionShown.indexOf(r.section) < 0) sectionShown.push(r.section); }
    });
    rows.forEach(function (r) { r.section.hidden = sectionShown.indexOf(r.section) < 0; });
    Object.keys(panels).forEach(function (g) {
      panels[g].hidden = q ? !shownIn[g] : g !== activeGroup;
      tabButtons[g].setAttribute('aria-selected', (!q && g === activeGroup) ? 'true' : 'false');
    });
    H.$('defaults-tabs').hidden = !!q;
    var total = Object.keys(shownIn).reduce(function (a, g) { return a + shownIn[g]; }, 0);
    var info = H.$('defaults-search-info');
    info.textContent = q ? (total + (total === 1 ? ' setting matches' : ' settings match')) : '';
    updateTabCounts();
  }

  function updateTabCounts() {
    Object.keys(tabButtons).forEach(function (g) {
      var n = rows.filter(function (r) { return r.group === g && getVal(r.def) !== undefined; }).length;
      tabButtons[g].textContent = g + (n ? ' (' + n + ' set)' : '');
    });
  }

  function renderExcluded() {
    var box = H.$('defaults-excluded');
    H.clear(box);
    var ex = schema.excluded || [];
    if (!ex.length) { box.hidden = true; return; }
    box.hidden = false;
    var d = H.el('details');
    d.appendChild(H.el('summary', { text: 'Options not offered here (' + ex.length + ')' }));
    d.appendChild(H.el('p', { class: 'muted', text: 'Moonbase has these, but Remux does not use them or manages them itself. You can still set them in Advanced (JSON) unless Remux refuses them.' }));
    var ul = H.el('ul', { class: 'plain' });
    ex.forEach(function (e) {
      ul.appendChild(H.el('li', null, [H.el('strong', { text: e.label }), document.createTextNode(' '), H.el('code', { text: e.key }), H.el('span', { class: 'muted', text: ' – ' + e.reason })]));
    });
    d.appendChild(ul);
    box.appendChild(d);
  }

  // Keys in the stored settings that no control covers.
  function knownKeys() {
    var s = {};
    if (schema) schema.controls.forEach(function (c) { allKeys(c).forEach(function (k) { s[k] = true; }); });
    return s;
  }

  function renderOther() {
    var known = knownKeys();
    var reasons = {};
    if (schema) (schema.excluded || []).forEach(function (e) { reasons[e.key] = e.reason; });
    var keys = Object.keys(settings).filter(function (k) { return !known[k]; }).sort();
    var ul = H.$('other-keys');
    H.clear(ul);
    H.$('other-empty').hidden = keys.length > 0;
    keys.forEach(function (k) {
      var blocked = blockedKeys.indexOf(k) >= 0;
      var del = H.el('button', { type: 'button', class: 'danger', text: 'Remove' });
      del.addEventListener('click', function () { delete settings[k]; changed(); refreshAll(); });
      var note = blocked ? 'Managed by Remux user settings. The server will refuse this key.'
        : (has(reasons, k) ? 'Not offered here: ' + reasons[k] + '.' : '');
      var kids = [H.el('code', { text: k }), H.el('span', { text: ' ' + H.preview(settings[k]) })];
      var li = H.el('li', { class: 'other' + (blocked ? ' bad' : '') }, [H.el('div', { class: 'other-main' }, kids)]);
      if (note) li.appendChild(H.el('div', { class: 'muted', text: note }));
      li.appendChild(del);
      ul.appendChild(li);
    });
  }

  // ---- sync between the form, the raw JSON and the server --------------

  function isDirty() { return !same(settings, stored); }

  function updateMeta() {
    var dirty = isDirty();
    H.$('defaults-dirty').textContent = !jsonValid ? 'The JSON has errors; fix it before saving.'
      : (dirty ? 'Unsaved changes' : 'No unsaved changes');
    H.$('defaults-dirty').className = dirty || !jsonValid ? 'warn-text' : 'muted';
    H.$('defaults-save').disabled = !dirty || !jsonValid;
    H.$('defaults-revert').disabled = !dirty;
    updateApplyState();
  }

  // Called after any edit that came from a control.
  function changed() {
    jsonValid = true;
    var ta = H.$('defaults-text');
    ta.value = pretty(settings);
    renderOther();
    updateTabCounts();
    updateMeta();
  }

  function refreshAll() {
    rows.forEach(function (r) { r.sync(); });
    updateTabCounts();
    renderOther();
    updateMeta();
  }

  function onRawInput() {
    var text = H.$('defaults-text').value || '{}';
    var obj;
    try { obj = JSON.parse(text); }
    catch (e) { jsonValid = false; H.setErrors('defaults-errors', ['Invalid JSON: ' + e.message + ' (the forms show the last valid version)']); updateMeta(); return; }
    if (!isObj(obj)) { jsonValid = false; H.setErrors('defaults-errors', ['Settings must be a JSON object.']); updateMeta(); return; }
    jsonValid = true;
    H.setErrors('defaults-errors', [], '');
    settings = obj;
    refreshAll();
  }

  // Returns a list of problems; empty when the settings look saveable.
  function localProblems() {
    var errs = [];
    var bad = Object.keys(settings).filter(function (k) { return blockedKeys.indexOf(k) >= 0; });
    if (bad.length) errs.push('These keys are managed by Remux user settings and cannot be defaults: ' + bad.join(', '));
    var hs = has(settings, 'homeSections'), ho = has(settings, 'homeRowOrder');
    if (hs !== ho) errs.push('homeSections and homeRowOrder are defaults that must be set together as a pair.');
    return errs;
  }

  function applyServer(d) {
    blockedKeys = d.blockedKeys || [];
    stored = isObj(d.settings) ? d.settings : {};
    settings = clone(stored);
    jsonValid = true;
    H.$('defaults-text').value = pretty(settings);
    H.$('defaults-meta').textContent = d.updatedAt ? 'Last saved ' + H.fmtTime(d.updatedAt) : 'Nothing saved yet';
    var b = H.$('blocked');
    b.hidden = !blockedKeys.length;
    b.textContent = 'Managed by Remux user settings, so they cannot be defaults: ' + blockedKeys.join(', ') + '.';
    if (built) refreshAll(); else { renderOther(); updateMeta(); }
  }

  function save() {
    if (!jsonValid) return;
    var errs = localProblems();
    if (errs.length) { H.setErrors('defaults-errors', errs); return; }
    H.api('PUT', '/moonfin/admin/defaults', JSON.stringify({ settings: settings })).then(function (d) {
      applyServer(d);
      H.setErrors('defaults-errors', [], 'Saved.');
      H.loadStatus();
    }).catch(function (err) {
      if (err && err.status === 400) H.setErrors('defaults-errors', [err.message]);
      else H.handleError(err);
    });
  }

  // ---- apply to users --------------------------------------------------

  var PROFILE_NAMES = {
    global: 'Global (all devices)', desktop: 'Desktop', mobile: 'Mobile', tv: 'TV'
  };

  function updateApplyState() {
    var dirty = isDirty() || !jsonValid;
    H.$('apply-unsaved').hidden = !dirty;
    var scopeUser = H.$('apply-scope-user').checked;
    H.$('apply-user').disabled = !scopeUser;
    H.$('apply-run').disabled = dirty || (scopeUser && !H.$('apply-user').value);
  }

  function renderUsers() {
    var sel = H.$('apply-user');
    var keep = sel.value;
    H.clear(sel);
    sel.appendChild(H.el('option', { value: '', text: users.length ? 'Choose a user' : 'No users' }));
    users.forEach(function (u) {
      sel.appendChild(H.el('option', { value: String(u.userId), text: String(u.name || u.userId) }));
    });
    sel.value = keep;
    updateApplyState();
  }

  function loadUsers() {
    return H.api('GET', '/moonfin/admin/users').then(function (d) {
      users = (d && d.items) || [];
      renderUsers();
    }).catch(H.handleError);
  }

  function checkedValue(name) {
    var n = document.querySelector('input[name="' + name + '"]:checked');
    return n ? n.value : '';
  }

  function applySummary(scope, mode, profile, userName, count) {
    var who = scope === 'all' ? 'every user' : userName;
    var prof = PROFILE_NAMES[profile] || profile;
    var tail = ' The defaults currently saved on the server have ' + count + ' setting' + (count === 1 ? '' : 's') + '.';
    if (mode === 'merge') {
      return 'Merge the saved defaults into the ' + prof + ' profile of ' + who + '? ' +
        'This only fills in settings the user has not set. Their own choices are kept. ' +
        'Users who have no saved ' + prof + ' profile already see the defaults, so they are skipped.' + tail;
    }
    return 'Overwrite the ' + prof + ' profile of ' + who + ' with the saved defaults? ' +
      'For every setting in the defaults, the user’s own value is replaced, even where they chose it on purpose, ' +
      'and the profile is created for users who have none. Settings that are not in the defaults are left alone. ' +
      'This cannot be undone.' + tail;
  }

  function runApply() {
    var scope = checkedValue('apply-scope') || 'all';
    var mode = checkedValue('apply-mode') || 'merge';
    var profile = H.$('apply-profile').value || 'global';
    var userId = H.$('apply-user').value;
    var userName = '';
    if (scope === 'user') {
      users.forEach(function (u) { if (String(u.userId) === userId) userName = String(u.name || u.userId); });
      if (!userId) return;
    }
    var count = Object.keys(stored).filter(function (k) { return stored[k] !== null && blockedKeys.indexOf(k) < 0; }).length;
    H.confirmDialog(applySummary(scope, mode, profile, userName, count)).then(function (ok) {
      if (!ok) return;
      var body = { scope: scope, mode: mode, profile: profile };
      if (scope === 'user') body.userId = userId;
      var btn = H.$('apply-run');
      btn.disabled = true;
      H.api('POST', '/moonfin/admin/defaults/apply', JSON.stringify(body)).then(function (r) {
        showApplyResult(r);
        H.loadUsers();
      }).catch(function (err) {
        if (err && (err.status === 400 || err.status === 404)) showApplyResult(null, err.message);
        else H.handleError(err);
      }).then(updateApplyState);
    });
  }

  function showApplyResult(r, error) {
    var box = H.$('apply-result');
    H.clear(box);
    box.hidden = false;
    if (error) {
      box.appendChild(H.el('div', { class: 'bad', text: error }));
      return;
    }
    var t = function (k, v) { return H.el('div', { class: 'tile' }, [H.el('div', { class: 'k', text: k }), H.el('div', { class: 'v', text: String(v) })]); };
    box.appendChild(H.el('div', { class: 'grid' }, [
      t('Users updated', r.usersAffected), t('Settings written', r.keysWritten), t('Users skipped', r.usersSkipped)
    ]));
    if (r.usersSkipped) {
      box.appendChild(H.el('p', { class: 'muted', text: 'Skipped users would have exceeded the size limit for a settings profile.' }));
    }
  }

  // ---- public ----------------------------------------------------------

  function loadSchema() {
    if (schema || built) return Promise.resolve();
    return fetch('/moonfin/admin/default-options.json', { cache: 'no-store' }).then(function (res) {
      if (!res.ok) throw new Error('HTTP ' + res.status);
      return res.json();
    }).then(function (s) {
      if (!s || !Array.isArray(s.controls) || !Array.isArray(s.groups)) throw new Error('unexpected format');
      schema = s;
    }).catch(function (e) {
      schema = null;
      H.setErrors('defaults-errors', ['Could not load the settings list (' + e.message + '). Use the Advanced (JSON) editor instead.']);
    });
  }

  function load() {
    var serverData = null;
    return Promise.all([
      loadSchema(),
      H.api('GET', '/moonfin/admin/defaults').then(function (d) { serverData = d; })
    ]).then(function () {
      if (schema && !built) buildForms();
      applyServer(serverData || {});
      H.$('defaults-forms').hidden = !schema;
      if (built) applyFilter();
      return loadUsers();
    }).catch(H.handleError);
  }

  function init(helpers) {
    H = helpers;
    H.$('defaults-search').addEventListener('input', function () {
      query = H.$('defaults-search').value;
      if (built) applyFilter();
    });
    H.$('defaults-text').addEventListener('input', onRawInput);
    H.$('defaults-validate').addEventListener('click', function () {
      if (!jsonValid) return;
      var errs = localProblems();
      if (errs.length) H.setErrors('defaults-errors', errs); else H.setErrors('defaults-errors', [], 'Looks valid.');
    });
    H.$('defaults-save').addEventListener('click', save);
    H.$('defaults-revert').addEventListener('click', function () {
      settings = clone(stored);
      jsonValid = true;
      H.$('defaults-text').value = pretty(settings);
      H.setErrors('defaults-errors', [], '');
      refreshAll();
    });
    ['apply-scope-all', 'apply-scope-user'].forEach(function (id) {
      H.$(id).addEventListener('change', updateApplyState);
    });
    H.$('apply-user').addEventListener('change', updateApplyState);
    H.$('apply-run').addEventListener('click', runApply);
  }

  window.MoonfinAdminDefaults = { init: init, load: load, loadUsers: loadUsers };
})();
