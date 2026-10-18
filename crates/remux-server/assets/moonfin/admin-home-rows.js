// Moonfin admin page: the pure logic behind the home-row builder.
//
// The default home layout is two settings that are always written together:
//   homeSections  full objects: { kind, type, enabled, order, ... }
//   homeRowOrder  ids of the enabled built-in rows, in display order
// Everything here is plain data in, plain data out (no DOM), so it can be
// checked in Node: `require('./admin-home-rows.js')`.
//
// Model (from Moonfin's HomeSectionConfig and Moonbase's admin page):
//   builtin rows      type is a lowercase id such as "resume", enabled is a bool
//   pluginDynamic     collection / playlist / genre / custom rows; kept as they
//                     are and carried through, shown with their display text
// A row is { raw, kind, type, enabled, label, supported }. raw is the original
// object, so fields this page does not know about survive an edit.
(function (root) {
  'use strict';

  // Built-in rows Remux can serve, in Moonbase's order. Rows that need Seerr,
  // IMDb or TMDB lists, or the Radarr/Sonarr calendars, are left out: Remux
  // does not implement those (see default-options.json "excluded").
  var SUPPORTED = [
    { type: 'smalllibrarytiles', label: 'My Media' },
    { type: 'resume', label: 'Continue Watching' },
    { type: 'nextup', label: 'Next Up' },
    { type: 'latestmedia', label: 'Recently Added Media' },
    { type: 'recentlyreleased', label: 'Recently Released' },
    { type: 'livetv', label: 'Live TV' },
    { type: 'onnow', label: 'On Now' },
    { type: 'librarybuttons', label: 'Library Buttons' },
    { type: 'resumeaudio', label: 'Resume Audio' },
    { type: 'resumebook', label: 'Resume Books' },
    { type: 'activerecordings', label: 'Active Recordings' },
    { type: 'collections', label: 'Collections' },
    { type: 'favoritemovies', label: 'Favorite Movies' },
    { type: 'favoriteseries', label: 'Favorite Series' },
    { type: 'favoriteepisodes', label: 'Favorite Episodes' },
    { type: 'favoritepeople', label: 'Favorite People' },
    { type: 'favoriteartists', label: 'Favorite Artists' },
    { type: 'favoritemusicvideos', label: 'Favorite Music Videos' },
    { type: 'favoritealbums', label: 'Favorite Albums' },
    { type: 'favoritesongs', label: 'Favorite Songs' },
    { type: 'genres', label: 'Genres' },
    { type: 'playlists', label: 'Playlists' },
    { type: 'sinceyouwatched1', label: 'Since You Watched 1' },
    { type: 'sinceyouwatched2', label: 'Since You Watched 2' },
    { type: 'sinceyouwatched3', label: 'Since You Watched 3' },
    { type: 'sinceyouwatched4', label: 'Since You Watched 4' },
    { type: 'sinceyouwatched5', label: 'Since You Watched 5' },
    { type: 'rewatch', label: 'Rewatch' }
  ];

  // The layout the app starts with: these four rows, in this order.
  var DEFAULT_TYPES = ['smalllibrarytiles', 'resume', 'nextup', 'latestmedia'];

  var labels = {};
  SUPPORTED.forEach(function (s) { labels[s.type] = s.label; });

  function isObj(v) { return v !== null && typeof v === 'object' && !Array.isArray(v); }
  function has(o, k) { return Object.prototype.hasOwnProperty.call(o, k); }
  function copy(v) { return JSON.parse(JSON.stringify(v)); }

  function isDynamic(raw) {
    return typeof raw.kind === 'string' && raw.kind.toLowerCase() === 'plugindynamic';
  }

  function builtinRow(type, enabled) {
    return makeRow({ kind: 'builtin', type: type, enabled: enabled, order: 0 });
  }

  function makeRow(raw) {
    var dyn = isDynamic(raw);
    var type = dyn ? 'none' : String(raw.type);
    var supported = dyn || has(labels, type);
    var label;
    if (dyn) label = raw.pluginDisplayText || raw.pluginSection || 'Dynamic row';
    else label = supported ? labels[type] : type;
    return {
      raw: raw,
      kind: dyn ? 'pluginDynamic' : 'builtin',
      type: type,
      enabled: raw.enabled !== false,
      label: String(label),
      supported: supported
    };
  }

  // Identity of a row: one built-in row per type; dynamic rows by their source.
  function keyOf(row) {
    if (row.kind === 'pluginDynamic') {
      var r = row.raw;
      return ['dyn', r.pluginSource || '', r.serverId || '', r.pluginSection || '', r.pluginAdditionalData || ''].join(':');
    }
    return 'builtin:' + row.type;
  }

  // Rows from the stored pair. homeSections wins; a lone homeRowOrder is the
  // legacy form (enabled built-in rows only). Returns [] when neither is usable.
  function parse(sections, order) {
    var raws = [];
    if (Array.isArray(sections) && sections.length) {
      raws = sections.map(function (s, i) { return { s: s, i: i }; }).filter(function (e) {
        if (!isObj(e.s)) return false;
        return isDynamic(e.s) || (typeof e.s.type === 'string' && e.s.type !== '' && e.s.type.toLowerCase() !== 'none');
      }).sort(function (a, b) {
        var ao = typeof a.s.order === 'number' ? a.s.order : 0;
        var bo = typeof b.s.order === 'number' ? b.s.order : 0;
        return ao - bo || a.i - b.i;
      }).map(function (e) { return copy(e.s); });
    } else if (Array.isArray(order)) {
      raws = order.filter(function (t) { return typeof t === 'string' && t !== '' && t !== 'none'; })
        .map(function (t) { return { kind: 'builtin', type: t, enabled: true, order: 0 }; });
    }
    var seen = {};
    var rows = [];
    raws.forEach(function (raw) {
      var row = makeRow(raw);
      var k = keyOf(row);
      if (seen[k]) return;
      seen[k] = true;
      rows.push(row);
    });
    return rows;
  }

  // The pair to store for these rows, or null when there are no rows (which
  // means "unset both keys"). homeRowOrder lists the enabled built-in rows.
  function serialize(rows) {
    if (!rows.length) return null;
    var sections = rows.map(function (row, i) {
      var out = Object.assign({}, row.raw);
      out.kind = row.kind;
      out.type = row.type;
      out.enabled = row.enabled;
      out.order = i;
      return out;
    });
    var order = rows.filter(function (r) { return r.kind === 'builtin' && r.enabled; })
      .map(function (r) { return r.type; });
    return { homeSections: sections, homeRowOrder: order };
  }

  function defaultRows() {
    return DEFAULT_TYPES.map(function (t) { return builtinRow(t, true); });
  }

  // Supported built-in rows that are not in the layout yet.
  function available(rows) {
    var present = {};
    rows.forEach(function (r) { if (r.kind === 'builtin') present[r.type] = true; });
    return SUPPORTED.filter(function (s) { return !present[s.type]; });
  }

  function add(rows, type) {
    if (!has(labels, type)) return rows.slice();
    if (rows.some(function (r) { return r.kind === 'builtin' && r.type === type; })) return rows.slice();
    return rows.concat([builtinRow(type, true)]);
  }

  function remove(rows, index) {
    if (index < 0 || index >= rows.length) return rows.slice();
    return rows.slice(0, index).concat(rows.slice(index + 1));
  }

  function toggle(rows, index, enabled) {
    if (index < 0 || index >= rows.length) return rows.slice();
    return rows.map(function (r, i) {
      if (i !== index) return r;
      var next = Object.assign({}, r);
      next.enabled = typeof enabled === 'boolean' ? enabled : !r.enabled;
      return next;
    });
  }

  // Moves the row at `from` so it ends up at index `to` (both in the current
  // list). Out of range targets are clamped.
  function move(rows, from, to) {
    var n = rows.length;
    if (from < 0 || from >= n) return rows.slice();
    to = Math.max(0, Math.min(n - 1, to));
    var out = rows.slice();
    var item = out.splice(from, 1)[0];
    out.splice(to, 0, item);
    return out;
  }

  // Where a dragged row lands when dropped before the row at `slot` in the
  // current list (slot may be rows.length to mean the end).
  function dropIndex(from, slot) {
    return slot > from ? slot - 1 : slot;
  }

  var api = {
    SUPPORTED: SUPPORTED, DEFAULT_TYPES: DEFAULT_TYPES,
    parse: parse, serialize: serialize, defaultRows: defaultRows, available: available,
    add: add, remove: remove, toggle: toggle, move: move, dropIndex: dropIndex, keyOf: keyOf
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  root.MoonfinHomeRows = api;
})(typeof window !== 'undefined' ? window : this);
