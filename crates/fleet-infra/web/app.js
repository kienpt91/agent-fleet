'use strict';
/*
 * fleet dashboard. One file, no framework, no build step.
 *
 * Every piece of text that comes from the daemon (titles, summaries, branches,
 * worker output, command lines) is shown through textContent or text nodes
 * only, after control and bidi characters were made visible by clean().
 * Nothing from the server is ever used as markup, as a URL or as a style.
 * Positions of the DAG are plain numbers written to SVG attributes.
 */
(function () {
  const POLL_MS = 2000;
  const EVENTS_MS = 5000;
  const CONFIRM_MS = 5000;
  const FLUSH_MS = 60;
  const MAX_OUTPUT = 262144; // characters of live output kept in the page
  const MAX_EVENTS = 500;
  const MAX_SEND_BYTES = 4096;
  const TAIL_MAX = 65536;
  const SVG_NS = 'http:' + '//www.w3.org/2000/svg';
  const NOT_LOGGED_IN = 'Not logged in. Open the login URL printed by `fleet daemon --web` in its terminal.';
  const MAX_PENDING = 1048576; // undecoded stream bytes (one frame) we are willing to hold
  const RECONNECT_MS = 1000;
  const RETRY_MS = 3000;
  const MAX_STREAM_RETRIES = 5;
  const ID_RE = /^[0-9a-f]{64}$/;
  // Ids that are safe to put into a hash route. Anything else is shown as plain text.
  const ID_OK = /^[A-Za-z0-9._-]{1,40}$/;
  const REF_OK = /^[A-Za-z0-9._-]{1,40}\/[A-Za-z0-9._-]{1,40}$/;
  const GOAL_STATUS = new Set(['open', 'in_progress', 'blocked', 'verified', 'abandoned']);
  const CONTRACT = new Set(['none', 'pending', 'passed', 'failed']);
  const VERIFICATION = new Set(['none', 'pending', 'unverified', 'failed', 'verified']);
  const VERDICT = new Set(['passed', 'failed', 'error']);
  const VERIF_RANK = new Map([['failed', 4], ['unverified', 3], ['pending', 2], ['verified', 1]]);
  const MAX_GOALS_SHOWN = 500;
  const MAX_LIST_SHOWN = 100;
  const MAX_EVIDENCE_SHOWN = 200;
  const MAX_EVIDENCE_OUTPUT = 20000;
  const MAX_GOAL_DETAILS = 5;

  const KNOWN = new Set([
    'pending', 'ready', 'running', 'review', 'completed', 'failed', 'blocked',
    'created', 'active', 'finished',
    'approved', 'denied', 'expired', 'consumed',
    'ok', 'info', 'fail'
  ]);
  const TASK_ORDER = ['pending', 'ready', 'running', 'review', 'completed', 'failed', 'blocked'];

  // Control characters and bidi controls are made visible instead of being rendered.
  const CTRL_ONE = /[\u0000-\u001f\u007f-\u009f\u00ad\u061c\u200b\u200e\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]/g;
  const CTRL_MULTI = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u00ad\u061c\u200b\u200e\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]/g;

  let csrf = null;
  let session = null;
  let lastGoals = null;
  let goalsNote = null;
  let loggedOut = false;
  let lastStatus = null;
  let lastParam = null;
  let fetchedAt = 0;
  let inflight = false;
  let pollTimer = null;
  let view = null;
  let confirmKey = null;
  let confirmTimer = null;
  let actionButtons = [];
  let countdowns = [];
  const drafts = new Map();

  // ---------------------------------------------------------------- helpers

  function mark(c) {
    return '[U+' + c.charCodeAt(0).toString(16).toUpperCase().padStart(4, '0') + ']';
  }

  function clean(v, o) {
    const multi = !!(o && o.multi);
    const max = (o && o.max) || 4000;
    let s = v === null || v === undefined ? '' : String(v);
    let cut = false;
    if (s.length > max) {
      s = s.slice(0, max);
      cut = true;
    }
    s = s.replace(/\r\n?/g, '\n').replace(multi ? CTRL_MULTI : CTRL_ONE, mark);
    return cut ? s + ' ...[truncated]' : s;
  }

  function short(v, n) {
    const s = clean(v);
    return s.length > n ? s.slice(0, n - 1) + '\u2026' : s;
  }

  function num(v) {
    return typeof v === 'number' && Number.isFinite(v) ? String(v) : '?';
  }

  function el(tag, cls, text, multi) {
    const e = document.createElement(tag);
    if (cls) {
      e.className = cls;
    }
    if (text !== undefined && text !== null) {
      e.textContent = clean(text, { multi: !!multi, max: multi ? 20000 : 4000 });
    }
    return e;
  }

  function svg(tag, attrs, text) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const k of Object.keys(attrs || {})) {
      e.setAttribute(k, String(attrs[k]));
    }
    if (text !== undefined) {
      e.textContent = clean(text);
    }
    return e;
  }

  function stClass(s) {
    return 'st-' + (typeof s === 'string' && KNOWN.has(s) ? s : 'unknown');
  }

  function badge(s) {
    return el('span', 'badge ' + stClass(s), s);
  }

  function enc(s) {
    return encodeURIComponent(String(s));
  }

  function go(hash) {
    location.hash = hash;
  }

  function fmtTime(ms) {
    if (typeof ms !== 'number' || !Number.isFinite(ms) || Math.abs(ms) > 8.6e15) {
      return '?';
    }
    return new Date(ms).toISOString().replace('T', ' ').slice(0, 19) + 'Z';
  }

  function fmtUptime(ms) {
    if (typeof ms !== 'number' || !Number.isFinite(ms)) {
      return '?';
    }
    const s = Math.floor(ms / 1000);
    const d = Math.floor(s / 86400);
    const h = Math.floor((s % 86400) / 3600);
    const m = Math.floor((s % 3600) / 60);
    if (d > 0) {
      return d + 'd ' + h + 'h';
    }
    if (h > 0) {
      return h + 'h ' + m + 'm';
    }
    return m + 'm ' + (s % 60) + 's';
  }

  function fmtRemaining(ms) {
    if (ms <= 0) {
      return 'expired';
    }
    const s = Math.floor(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    if (h > 0) {
      return h + 'h ' + String(m).padStart(2, '0') + 'm left';
    }
    return m + 'm ' + String(r).padStart(2, '0') + 's left';
  }

  function visible() {
    return document.visibilityState === 'visible';
  }

  function viewEl() {
    return document.getElementById('view');
  }

  function setLine(text) {
    const e = document.getElementById('statusline');
    if (e) {
      e.textContent = clean(text);
    }
  }

  function setText(id, text) {
    const e = document.getElementById(id);
    if (e) {
      e.textContent = clean(text);
    }
  }

  function showMsg(box, kind, text) {
    box.className = 'msg ' + kind;
    box.textContent = clean(text, { multi: true, max: 2000 });
  }

  // ------------------------------------------------------------------- api

  async function api(method, path, body) {
    const opts = { method: method, credentials: 'omit', cache: 'no-store', headers: authHeaders() };
    if (method === 'POST') {
      opts.headers['Content-Type'] = 'application/json';
      if (csrf !== null) {
        opts.headers['X-CSRF-Token'] = csrf;
      }
      opts.body = JSON.stringify(body || {});
    }
    let resp;
    try {
      resp = await fetch(path, opts);
    } catch (e) {
      return { ok: false, status: 0, data: null, networkError: true, retryAfter: null };
    }
    let data = null;
    try {
      data = await resp.json();
    } catch (e) {
      data = null;
    }
    let retryAfter = null;
    if (resp.headers && typeof resp.headers.get === 'function') {
      retryAfter = resp.headers.get('Retry-After');
    }
    return { ok: !!resp.ok, status: resp.status, data: data, networkError: false, retryAfter: retryAfter };
  }

  // The session id and the CSRF token are sent as request headers only, never as ambient browser
  // credentials. They are kept in this tab's session storage so a reload keeps the login.
  function authHeaders() {
    const h = {};
    if (session !== null) {
      h['X-Fleet-Session'] = session;
    }
    return h;
  }

  function validId(v) {
    return typeof v === 'string' && ID_RE.test(v);
  }

  function saveSession() {
    try {
      sessionStorage.setItem('fleet.session', JSON.stringify({ session: session, csrf: csrf }));
    } catch (e) {
      setLine('Could not keep the login for a reload.');
    }
  }

  function loadSession() {
    try {
      const raw = sessionStorage.getItem('fleet.session');
      const d = raw === null ? null : JSON.parse(raw);
      if (d && validId(d.session) && validId(d.csrf)) {
        session = d.session;
        csrf = d.csrf;
        return true;
      }
    } catch (e) {
      return false;
    }
    return false;
  }

  function clearSession() {
    session = null;
    csrf = null;
    try {
      sessionStorage.removeItem('fleet.session');
    } catch (e) {
      setLine('Could not clear the stored login.');
    }
  }

  function errText(res) {
    if (res.status === 429) {
      return 'Too many failed attempts. The server asks you to retry after ' +
        clean(res.retryAfter === null || res.retryAfter === undefined ? 'a few' : res.retryAfter, { max: 32 }) +
        ' seconds.';
    }
    const e = res.data && res.data.error;
    if (e && typeof e.message === 'string') {
      return e.message;
    }
    if (res.networkError) {
      return 'Network error: the daemon did not answer.';
    }
    return 'Request failed (HTTP ' + res.status + ').';
  }

  async function post(path, body) {
    const res = await api('POST', path, body);
    if (res.status === 401) {
      showLoggedOut(NOT_LOGGED_IN);
    }
    return res;
  }

  // ------------------------------------------------------ confirm buttons

  function paintButton(rec) {
    const armed = confirmKey === rec.key;
    rec.b.textContent = armed ? 'Confirm?' : rec.label;
    rec.b.classList.toggle('armed', armed);
  }

  function clearConfirm() {
    clearTimeout(confirmTimer);
    confirmTimer = null;
    confirmKey = null;
    actionButtons.forEach(paintButton);
  }

  function armConfirm(key) {
    clearTimeout(confirmTimer);
    confirmKey = key;
    actionButtons.forEach(paintButton);
    confirmTimer = setTimeout(clearConfirm, CONFIRM_MS);
  }

  // A button that needs a second click within CONFIRM_MS before it acts.
  function wireAction(b, key, label, group, fn) {
    const rec = { b: b, key: key, label: label, group: group };
    actionButtons.push(rec);
    b.addEventListener('click', function () {
      if (b.disabled) {
        return;
      }
      if (confirmKey === key) {
        clearConfirm();
        fn();
      } else {
        armConfirm(key);
      }
    });
    paintButton(rec);
    return b;
  }

  function actionButton(key, label, group, cls, fn) {
    const b = el('button', 'btn ' + cls, label);
    b.setAttribute('type', 'button');
    return wireAction(b, key, label, group, fn);
  }

  function dropGroup(group) {
    actionButtons = actionButtons.filter(function (r) {
      return r.group !== group;
    });
  }

  // ------------------------------------------------------- session states

  function showLoggedOut(message) {
    loggedOut = true;
    clearSession();
    if (pollTimer !== null) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    leaveView();
    const v = viewEl();
    v.replaceChildren(el('p', 'notice', message));
    setLine('Not logged in.');
  }

  // ------------------------------------------------------------- polling

  function statusParam() {
    const r = parseRoute();
    if (r.name === 'run' || r.name === 'task') {
      return '?run=' + enc(r.run);
    }
    return '';
  }

  function schedulePoll() {
    if (pollTimer !== null) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    if (loggedOut || !visible()) {
      return;
    }
    pollTimer = setTimeout(poll, POLL_MS);
  }

  async function poll() {
    if (inflight || loggedOut) {
      return;
    }
    if (pollTimer !== null) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    inflight = true;
    const param = statusParam();
    let again = false;
    try {
      const res = await api('GET', '/api/status' + param);
      if (res.status === 401) {
        showLoggedOut(NOT_LOGGED_IN);
        return;
      }
      if (res.ok && res.data && Array.isArray(res.data.runs)) {
        lastStatus = res.data;
        lastParam = param;
        fetchedAt = Date.now();
        const gres = await api('GET', '/api/goals');
        if (gres.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (gres.ok && gres.data && Array.isArray(gres.data.goals)) {
          lastGoals = gres.data;
          goalsNote = null;
        } else {
          lastGoals = null;
          goalsNote = errText(gres);
        }
        updateHeader(lastStatus);
        if (param === statusParam() && view && view.update) {
          try {
            view.update(lastStatus);
          } catch (e) {
            setLine('Could not draw the page: ' + (e && e.message));
          }
        }
        setLine('Updated ' + fmtTime(typeof lastStatus.now_ms === 'number' ? lastStatus.now_ms : Date.now()));
      } else {
        setLine('Status unavailable: ' + errText(res) + ' Retrying.');
      }
      again = statusParam() !== param;
    } finally {
      inflight = false;
    }
    if (again) {
      poll();
    } else {
      schedulePoll();
    }
  }

  function updateHeader(d) {
    const dm = d.daemon || {};
    setText('hdr-version', 'fleet ' + clean(dm.version, { max: 40 }));
    setText('hdr-sessions', num(dm.sessions) + '/' + num(dm.max_sessions) + ' sessions');
    setText('hdr-rss', typeof dm.rss_kb === 'number' ? 'rss ' + (dm.rss_kb / 1024).toFixed(1) + ' MiB' : 'rss n/a');
    const pending = (Array.isArray(d.approvals) ? d.approvals : []).filter(function (a) {
      return a && a.status === 'pending';
    }).length;
    const b = document.getElementById('hdr-approvals');
    if (b) {
      b.textContent = pending + (pending === 1 ? ' pending approval' : ' pending approvals');
      b.className = 'badge ' + (pending > 0 ? 'st-review' : 'st-unknown');
    }
    const gl = document.getElementById('hdr-goals');
    if (gl) {
      const left = goalsLeft();
      gl.classList.toggle('hidden', left === 0);
      gl.textContent = left + (left === 1 ? ' goal left' : ' goals left');
    }
  }

  function estimateNow() {
    if (lastStatus && typeof lastStatus.now_ms === 'number') {
      return lastStatus.now_ms + (Date.now() - fetchedAt);
    }
    return Date.now();
  }

  function tickCountdowns() {
    if (!visible()) {
      return;
    }
    const now = estimateNow();
    countdowns.forEach(function (c) {
      c.node.textContent = fmtRemaining(c.expires - now);
    });
  }

  // --------------------------------------------------------------- routes

  function safeDecode(s) {
    try {
      return decodeURIComponent(s);
    } catch (e) {
      return s;
    }
  }

  function parseRoute() {
    const parts = String(location.hash || '').replace(/^#\/?/, '').split('/').map(safeDecode);
    if (parts[0] === 'run' && parts[1]) {
      return { name: 'run', run: parts[1] };
    }
    if (parts[0] === 'task' && parts[1] && parts[2]) {
      return { name: 'task', run: parts[1], task: parts.slice(2).join('/') };
    }
    if (parts[0] === 'goal' && parts[1]) {
      return { name: 'goal', id: parts[1] };
    }
    if (parts[0] === 'group' && parts[1]) {
      return { name: 'group', id: parts[1] };
    }
    if (parts[0] === 'decisions' || parts[0] === 'events' || parts[0] === 'health' || parts[0] === 'goals' || parts[0] === 'groups') {
      return { name: parts[0] };
    }
    return { name: 'runs' };
  }

  function leaveView() {
    if (view && view.stop) {
      view.stop();
    }
    view = null;
    actionButtons = actionButtons.filter(function (r) {
      return r.group === 'static';
    });
    countdowns = [];
    clearConfirm();
  }

  function setNav(name) {
    const section = name === 'run' || name === 'task' ? 'runs' : name === 'goal' ? 'goals' : name === 'group' ? 'groups' : name;
    ['runs', 'goals', 'groups', 'decisions', 'events', 'health'].forEach(function (id) {
      const a = document.getElementById('nav-' + id);
      if (a) {
        a.classList.toggle('active', id === section);
      }
    });
  }

  function route() {
    if (loggedOut) {
      return;
    }
    leaveView();
    const r = parseRoute();
    setNav(r.name);
    let v;
    if (r.name === 'run') {
      v = runView(r.run);
    } else if (r.name === 'task') {
      v = taskView(r.run, r.task);
    } else if (r.name === 'decisions') {
      v = decisionsView();
    } else if (r.name === 'events') {
      v = eventsView();
    } else if (r.name === 'health') {
      v = healthView();
    } else if (r.name === 'goals') {
      v = goalsView();
    } else if (r.name === 'goal') {
      v = goalView(r.id);
    } else if (r.name === 'groups') {
      v = groupsView();
    } else if (r.name === 'group') {
      v = groupView(r.id);
    } else {
      v = runsView();
    }
    view = v;
    viewEl().replaceChildren(v.node);
    if (v.start) {
      v.start();
    }
    if (lastStatus && lastParam === statusParam() && v.update) {
      v.update(lastStatus);
    }
    poll();
  }

  function onVisibility() {
    if (visible()) {
      poll();
      if (view && view.resume) {
        view.resume();
      }
    } else {
      if (pollTimer !== null) {
        clearTimeout(pollTimer);
        pollTimer = null;
      }
      if (view && view.pause) {
        view.pause();
      }
    }
  }

  // ------------------------------------------------------------ runs view

  function countsRow(counts) {
    const row = el('span', 'counts');
    TASK_ORDER.forEach(function (s) {
      const n = counts && typeof counts[s] === 'number' ? counts[s] : 0;
      if (n > 0) {
        row.appendChild(el('span', 'badge ' + stClass(s), s + ' ' + n));
      }
    });
    if (!row.childNodes.length) {
      row.appendChild(el('span', 'muted', 'no tasks'));
    }
    return row;
  }

  function runsView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Runs'));
    const holder = el('div', 'runs');
    root.appendChild(holder);
    let sig = null;
    return {
      node: root,
      update: function (d) {
        const runs = d.runs;
        const s = JSON.stringify(runs.map(function (r) {
          return [r.id, r.objective, r.status, r.counts];
        }));
        if (s === sig) {
          return;
        }
        sig = s;
        if (!runs.length) {
          holder.replaceChildren(el('p', 'muted', 'No runs yet.'));
          return;
        }
        const table = el('table', 'grid');
        const head = el('tr');
        ['Run', 'Objective', 'Phase', 'Tasks'].forEach(function (h) {
          head.appendChild(el('th', '', h));
        });
        table.appendChild(head);
        runs.forEach(function (r) {
          const tr = el('tr');
          const idCell = el('td');
          const open = el('button', 'btn link', r.id);
          open.setAttribute('type', 'button');
          open.addEventListener('click', function () {
            go('#/run/' + enc(r.id));
          });
          idCell.appendChild(open);
          tr.appendChild(idCell);
          tr.appendChild(el('td', 'wrap', r.objective));
          const ph = el('td');
          ph.appendChild(badge(r.status));
          tr.appendChild(ph);
          const cc = el('td');
          cc.appendChild(countsRow(r.counts));
          tr.appendChild(cc);
          table.appendChild(tr);
        });
        holder.replaceChildren(table);
      }
    };
  }

  // --------------------------------------------------------------- DAG

  const BOX_W = 220;
  const BOX_H = 92;
  const GAP_X = 72;
  const GAP_Y = 18;
  const PAD = 12;

  function layout(tasks) {
    const byId = new Map();
    tasks.forEach(function (t) {
      byId.set(t.id, t);
    });
    const depth = new Map();
    const visiting = new Set();
    function d(id) {
      if (depth.has(id)) {
        return depth.get(id);
      }
      if (visiting.has(id)) {
        return 0;
      }
      visiting.add(id);
      let best = 0;
      (byId.get(id).deps || []).forEach(function (dep) {
        if (byId.has(dep)) {
          best = Math.max(best, d(dep) + 1);
        }
      });
      visiting.delete(id);
      depth.set(id, best);
      return best;
    }
    const rows = new Map();
    const pos = new Map();
    let cols = 0;
    let maxRows = 0;
    tasks.forEach(function (t) {
      if (pos.has(t.id)) {
        return;
      }
      const col = d(t.id);
      const row = rows.get(col) || 0;
      rows.set(col, row + 1);
      pos.set(t.id, { x: PAD + col * (BOX_W + GAP_X), y: PAD + row * (BOX_H + GAP_Y) });
      cols = Math.max(cols, col + 1);
      maxRows = Math.max(maxRows, row + 1);
    });
    return {
      pos: pos,
      width: PAD * 2 + cols * BOX_W + Math.max(0, cols - 1) * GAP_X,
      height: PAD * 2 + maxRows * BOX_H + Math.max(0, maxRows - 1) * GAP_Y
    };
  }

  function drawDag(runId, tasks, vmap) {
    const lay = layout(tasks);
    const root = svg('svg', {
      'class': 'dag',
      width: lay.width,
      height: lay.height,
      viewBox: '0 0 ' + lay.width + ' ' + lay.height,
      role: 'group',
      'aria-label': 'Task dependency graph, dependencies on the left'
    });
    tasks.forEach(function (t) {
      const to = lay.pos.get(t.id);
      (t.deps || []).forEach(function (dep) {
        const from = lay.pos.get(dep);
        if (from && to) {
          root.appendChild(svg('line', {
            'class': 'edge',
            x1: from.x + BOX_W,
            y1: from.y + BOX_H / 2,
            x2: to.x,
            y2: to.y + BOX_H / 2
          }));
        }
      });
    });
    const seen = new Set();
    tasks.forEach(function (t) {
      if (seen.has(t.id)) {
        return;
      }
      seen.add(t.id);
      const p = lay.pos.get(t.id);
      const cls = stClass(t.status);
      const node = svg('svg', {
        'class': 'node ' + cls,
        x: p.x,
        y: p.y,
        width: BOX_W,
        height: BOX_H,
        tabindex: '0',
        role: 'link',
        'aria-label': 'Task ' + short(t.id, 40) + ', ' + short(t.status, 20)
      });
      node.appendChild(svg('title', {}, t.id + ': ' + t.title));
      node.appendChild(svg('rect', { 'class': 'node-bg', x: 1, y: 1, width: BOX_W - 2, height: BOX_H - 2, rx: 6 }));
      node.appendChild(svg('text', { 'class': 'node-id', x: 10, y: 20 }, short(t.id, 24)));
      node.appendChild(svg('text', { 'class': 'node-title', x: 10, y: 40 }, short(t.title, 30)));
      node.appendChild(svg('rect', { 'class': 'node-badge ' + cls, x: 10, y: 50, width: 86, height: 18, rx: 9 }));
      node.appendChild(svg('text', { 'class': 'node-badge-text', x: 53, y: 63 }, short(t.status, 12)));
      const vf = vmap.get(runId + '/' + t.id);
      if (vf) {
        node.appendChild(svg('rect', { 'class': 'node-badge vf-' + vf, x: 104, y: 50, width: 86, height: 18, rx: 9 }));
        node.appendChild(svg('text', { 'class': 'node-badge-text', x: 147, y: 63 }, short(vf, 12)));
      }
      if (t.live) {
        node.appendChild(svg('circle', { 'class': 'node-live', cx: BOX_W - 16, cy: 59, r: 5 }));
      }
      node.appendChild(svg('text', { 'class': 'node-meta', x: 10, y: 84 },
        short(t.worker === null || t.worker === undefined ? 'no worker' : t.worker, 18) + ' \u00b7 attempt ' + num(t.attempt)));
      const open = function () {
        go('#/task/' + enc(runId) + '/' + enc(t.id));
      };
      node.addEventListener('click', open);
      node.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' || ev.key === ' ') {
          open();
        }
      });
      root.appendChild(node);
    });
    return root;
  }

  function runView(runId) {
    const root = el('section', 'page');
    const back = el('button', 'btn link', 'All runs');
    back.setAttribute('type', 'button');
    back.addEventListener('click', function () {
      go('#/runs');
    });
    root.appendChild(back);
    const head = el('div', 'runhead');
    const dagBox = el('div', 'dagbox');
    root.appendChild(head);
    root.appendChild(dagBox);
    let sig = null;
    return {
      node: root,
      update: function (d) {
        const run = d.runs.find(function (r) {
          return r && r.id === runId;
        });
        if (!run) {
          head.replaceChildren(el('p', 'notice', 'Run not found: ' + runId));
          dagBox.replaceChildren();
          sig = null;
          return;
        }
        const tasks = Array.isArray(run.tasks) ? run.tasks : [];
        const vmap = verificationMap();
        const s = JSON.stringify([run, tasks.map(function (t) {
          return [t.id, t.title, t.status, t.deps, t.worker, t.attempt, t.live];
        }), Array.from(vmap)]);
        if (s === sig) {
          return;
        }
        sig = s;
        const top = el('div', 'titlebar');
        top.appendChild(el('h2', '', 'Run ' + run.id));
        top.appendChild(badge(run.status));
        top.appendChild(countsRow(run.counts));
        const dl = el('dl', 'fields');
        field(dl, 'Objective', run.objective, true);
        field(dl, 'Repository', run.repo);
        field(dl, 'Base', clean(run.base_ref) + ' @ ' + clean(run.base_sha));
        field(dl, 'Integration branch', run.integration_branch);
        field(dl, 'Max parallel', num(run.max_parallel));
        head.replaceChildren(top, dl);
        if (!tasks.length) {
          dagBox.replaceChildren(el('p', 'muted', 'This run has no tasks yet.'));
        } else {
          const scroller = el('div', 'dagscroll');
          scroller.appendChild(drawDag(run.id, tasks, vmap));
          dagBox.replaceChildren(scroller, el('p', 'muted small', 'Arrows run from a dependency (left) to the task that waits for it. Click a task for details.'));
        }
      }
    };
  }

  function field(dl, label, value, multi) {
    dl.appendChild(el('dt', '', label));
    const dd = el('dd');
    if (value === null || value === undefined || value === '') {
      dd.appendChild(el('span', 'muted', 'none'));
    } else if (value && value.nodeType) {
      dd.appendChild(value);
    } else {
      dd.appendChild(el(multi ? 'div' : 'span', multi ? 'multi' : 'mono-text', value, !!multi));
    }
    dl.appendChild(dd);
  }

  // ------------------------------------------------------------ task view

  function taskView(runId, taskId) {
    const ref = runId + '/' + taskId;
    const root = el('section', 'page');
    const back = el('button', 'btn link', 'Back to run ' + runId);
    back.setAttribute('type', 'button');
    back.addEventListener('click', function () {
      go('#/run/' + enc(runId));
    });
    root.appendChild(back);
    const titlebar = el('div', 'titlebar');
    root.appendChild(titlebar);
    const fields = el('div', 'taskfields');
    root.appendChild(fields);

    const msg = el('div', 'msg');
    const actions = el('div', 'actions');
    const mergeBtn = actionButton('merge:' + ref, 'Merge', 'task', 'primary', doMerge);
    mergeBtn.disabled = true;
    actions.appendChild(mergeBtn);
    root.appendChild(actions);
    root.appendChild(msg);

    const sendBox = el('div', 'send hidden');
    sendBox.appendChild(el('h3', '', 'Send input to the worker'));
    const sendRow = el('div', 'row');
    const sendInput = el('input', 'textin');
    sendInput.setAttribute('type', 'text');
    sendInput.setAttribute('maxlength', String(MAX_SEND_BYTES));
    sendInput.setAttribute('autocomplete', 'off');
    sendInput.setAttribute('aria-label', 'Text to send');
    const nlLabel = el('label', 'check');
    const nl = el('input');
    nl.setAttribute('type', 'checkbox');
    nl.checked = true;
    nlLabel.appendChild(nl);
    nlLabel.appendChild(document.createTextNode(' press Enter after'));
    const sendBtn = actionButton('send:' + ref, 'Send', 'task', '', doSend);
    sendRow.appendChild(sendInput);
    sendRow.appendChild(nlLabel);
    sendRow.appendChild(sendBtn);
    sendBox.appendChild(sendRow);
    sendBox.appendChild(el('p', 'muted small', 'At most ' + MAX_SEND_BYTES + ' bytes per message.'));
    root.appendChild(sendBox);

    root.appendChild(el('h3', '', 'Live output'));
    const streamNote = el('p', 'muted small', '');
    const lostNote = el('p', 'notice small hidden', 'Some output was dropped by the server; what you see has gaps.');
    const pre = el('pre', 'output');
    pre.setAttribute('tabindex', '0');
    root.appendChild(streamNote);
    root.appendChild(lostNote);
    root.appendChild(pre);
    root.appendChild(el('h3', '', 'Goals and evidence'));
    const govBox = el('div', 'govbox');
    govBox.appendChild(el('p', 'muted', 'Loading goals.'));
    root.appendChild(govBox);

    let stopped = false;
    let gen = 0;
    let out = '';
    let nextOff = 0;
    let abortCtl = null;
    let tailDone = false;
    let halted = false;
    let flushTimer = null;
    let fieldsSig = null;
    let govSig = null;
    let govGen = 0;
    const govDocs = new Map();

    function renderGov(gov) {
      if (lastGoals === null) {
        govBox.replaceChildren(el('p', 'muted', 'Goal information is not available' + (goalsNote ? ': ' + clean(goalsNote, { max: 300 }) : '.')));
        return;
      }
      if (!gov.length) {
        govBox.replaceChildren(el('p', 'muted', 'No goal names this task.'));
        return;
      }
      const cards = [];
      gov.forEach(function (g, i) {
        const card = el('article', 'card');
        const head = el('div', 'cardhead');
        head.appendChild(goalLink(g.id, g.title));
        head.appendChild(el('span', 'muted mono-text', g.id));
        head.appendChild(tagged('gs', GOAL_STATUS, g.status));
        head.appendChild(tagged('ct', CONTRACT, g.contract, 'contract ' + clean(g.contract, { max: 20 })));
        const mine = (Array.isArray(g.tasks) ? g.tasks : []).find(function (t) {
          return t && t.task === ref;
        });
        if (mine && mine.verification) {
          head.appendChild(tagged('vf', VERIFICATION, mine.verification, 'verification ' + clean(mine.verification, { max: 20 })));
        }
        card.appendChild(head);
        const entry = govDocs.get(g.id);
        if (i >= MAX_GOAL_DETAILS) {
          card.appendChild(el('p', 'muted small', 'Evidence is not loaded for this goal (at most ' + MAX_GOAL_DETAILS + ' goals are expanded).'));
        } else if (!entry) {
          card.appendChild(el('p', 'muted small', 'Loading evidence.'));
        } else if (entry.error) {
          card.appendChild(el('p', 'notice small', 'Could not load evidence: ' + entry.error));
        } else {
          const checks = Array.isArray(entry.doc.checks) ? entry.doc.checks : [];
          const history = Array.isArray(entry.doc.history) ? entry.doc.history : [];
          if (!checks.length) {
            card.appendChild(el('p', 'muted small', 'This goal has no checks.'));
          } else {
            const table = makeTable(['Check', 'Kind', 'Mandatory', 'Newest evidence for this task', 'Summary', 'Commit', 'Counts'], 'auto');
            checks.forEach(function (c) {
              const e = history.find(function (x) {
                return x && x.check === c.check && (x.task === ref || x.task === taskId);
              });
              const tr = el('tr', e && e.counts !== true ? 'dim' : '');
              tr.appendChild(cell(c.check));
              tr.appendChild(cell(c.kind));
              tr.appendChild(cell(c.mandatory ? 'yes' : 'no'));
              tr.appendChild(cell(e ? tagged('vd', VERDICT, e.verdict) : 'no evidence for this task'));
              tr.appendChild(cell(e ? e.summary : '', 'wrap multi', true));
              tr.appendChild(cell(e ? el('code', 'mono-text', shortHash(e.commit)) : ''));
              tr.appendChild(cell(e ? (e.counts === true ? 'yes' : 'no') : ''));
              table.appendChild(tr);
            });
            card.appendChild(scrollBox(table));
          }
        }
        cards.push(card);
      });
      cards.push(el('p', 'muted small', 'Evidence that does not count (dirty worktree or not the reviewed commit) is shown dimmed.'));
      govBox.replaceChildren.apply(govBox, cards);
    }

    async function loadGov(gov, myGen) {
      const wanted = gov.slice(0, MAX_GOAL_DETAILS);
      for (let i = 0; i < wanted.length; i++) {
        const res = await api('GET', '/api/goal?id=' + enc(wanted[i].id));
        if (stopped || myGen !== govGen) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (res.ok && res.data && typeof res.data === 'object') {
          govDocs.set(wanted[i].id, { doc: res.data, error: null });
        } else {
          govDocs.set(wanted[i].id, { doc: null, error: errText(res) });
        }
        renderGov(gov);
      }
    }

    function syncGov() {
      const gov = governing(ref).filter(function (g) {
        return typeof g.id === 'string' && ID_OK.test(g.id);
      });
      const sig = JSON.stringify([lastGoals === null, goalsNote, gov]);
      if (sig === govSig) {
        return;
      }
      govSig = sig;
      govGen += 1;
      renderGov(gov);
      if (lastGoals !== null) {
        loadGov(gov, govGen);
      }
    }

    function note(text) {
      streamNote.textContent = clean(text);
    }

    function flush() {
      flushTimer = null;
      const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 8;
      pre.textContent = out;
      if (atBottom) {
        pre.scrollTop = pre.scrollHeight;
      }
    }

    function append(text) {
      let s = String(text === null || text === undefined ? '' : text);
      if (s.length > MAX_OUTPUT) {
        s = s.slice(s.length - MAX_OUTPUT);
      }
      out += clean(s, { multi: true, max: MAX_OUTPUT + 1 });
      if (out.length > MAX_OUTPUT) {
        out = out.slice(out.length - MAX_OUTPUT);
        const c = out.charCodeAt(0);
        if (c >= 0xdc00 && c <= 0xdfff) {
          out = out.slice(1);
        }
      }
      if (flushTimer === null) {
        flushTimer = setTimeout(flush, FLUSH_MS);
      }
    }

    function closeStream() {
      if (abortCtl !== null) {
        abortCtl.abort();
      }
    }

    function openStream() {
      if (abortCtl !== null || stopped) {
        return;
      }
      abortCtl = new AbortController();
      runStream(abortCtl);
    }

    function sleep(ms, signal) {
      return new Promise(function (resolve) {
        const t = setTimeout(done, ms);
        function done() {
          clearTimeout(t);
          signal.removeEventListener('abort', done);
          resolve();
        }
        signal.addEventListener('abort', done);
      });
    }

    let endReason = '';

    // Returns 'end' or 'limit' when the frame ends this connection, otherwise null.
    function handleEvent(type, data) {
      let d = null;
      try {
        d = JSON.parse(data);
      } catch (e) {
        return null;
      }
      if (!d || typeof d !== 'object') {
        return null;
      }
      if (type === 'output') {
        if (typeof d.next === 'number') {
          nextOff = d.next;
        }
        if (d.lost) {
          lostNote.classList.remove('hidden');
        }
        append(d.text);
        return null;
      }
      if (type === 'end') {
        endReason = typeof d.reason === 'string' ? d.reason : '';
        return 'end';
      }
      if (type === 'limit') {
        if (typeof d.next === 'number') {
          nextOff = d.next;
        }
        return 'limit';
      }
      return null;
    }

    // One connection. Resolves to 'end', 'limit', 'closed' (body ended without an end event),
    // 'overflow', 'auth' or 'error'.
    async function readStream(ac, reconnecting) {
      const headers = authHeaders();
      headers.Accept = 'text/event-stream';
      if (reconnecting) {
        headers['Last-Event-ID'] = String(nextOff);
      }
      let resp;
      try {
        resp = await fetch('/api/stream?task=' + enc(ref) + '&since=' + enc(nextOff), {
          method: 'GET',
          credentials: 'omit',
          cache: 'no-store',
          headers: headers,
          signal: ac.signal
        });
      } catch (e) {
        return 'error';
      }
      if (resp.status === 401) {
        return 'auth';
      }
      if (!resp.ok || !resp.body || typeof resp.body.getReader !== 'function') {
        return 'error';
      }
      note('Streaming live output.');
      let frame = { type: '', id: null, data: [], size: 0 };
      function feed(line) {
        if (line === '') {
          const f = frame;
          frame = { type: '', id: null, data: [], size: 0 };
          if (f.id !== null) {
            nextOff = f.id;
          }
          return f.data.length ? handleEvent(f.type, f.data.join('\n')) : null;
        }
        if (line.charCodeAt(0) === 58) {
          return null;
        }
        const i = line.indexOf(':');
        const field = i < 0 ? line : line.slice(0, i);
        let value = i < 0 ? '' : line.slice(i + 1);
        if (value.charCodeAt(0) === 32) {
          value = value.slice(1);
        }
        if (field === 'event') {
          frame.type = value;
        } else if (field === 'data') {
          frame.size += value.length + 1;
          if (frame.size > MAX_PENDING) {
            return 'overflow';
          }
          frame.data.push(value);
        } else if (field === 'id' && /^[0-9]{1,15}$/.test(value)) {
          frame.id = Number(value);
        }
        return null;
      }
      const reader = resp.body.getReader();
      const dec = new TextDecoder();
      const re = /\r\n|\n|\r/g;
      let buf = '';
      let result = 'closed';
      try {
        outer: for (;;) {
          const r = await reader.read();
          if (r.done) {
            break;
          }
          buf += dec.decode(r.value, { stream: true });
          let pos = 0;
          for (;;) {
            re.lastIndex = pos;
            const m = re.exec(buf);
            if (m === null || (m[0] === '\r' && m.index + 1 === buf.length)) {
              break;
            }
            const line = buf.slice(pos, m.index);
            pos = m.index + m[0].length;
            const sig = feed(line);
            if (sig !== null) {
              result = sig;
              break outer;
            }
          }
          buf = buf.slice(pos);
          if (buf.length > MAX_PENDING) {
            result = 'overflow';
            break;
          }
        }
      } catch (e) {
        result = 'error';
      }
      try {
        const p = reader.cancel();
        if (p && typeof p.catch === 'function') {
          p.catch(function () {});
        }
      } catch (e) {
        setLine('Could not close the stream reader.');
      }
      return result;
    }

    // Keeps one connection at a time for this view until it ends, fails for good or is aborted.
    async function runStream(ac) {
      let failures = 0;
      let reconnecting = false;
      try {
        for (;;) {
          const r = await readStream(ac, reconnecting);
          if (ac.signal.aborted) {
            return;
          }
          if (r === 'end') {
            halted = true;
            note('Session ended' + (endReason ? ': ' + clean(endReason, { max: 200 }) : '.'));
            return;
          }
          if (r === 'auth') {
            showLoggedOut(NOT_LOGGED_IN);
            return;
          }
          if (r === 'overflow') {
            halted = true;
            note('Live output stopped: the server sent more than 1 MiB without a complete frame.');
            return;
          }
          if (r === 'error') {
            failures += 1;
            if (failures > MAX_STREAM_RETRIES) {
              halted = true;
              note('Live output stopped after ' + MAX_STREAM_RETRIES + ' failed retries.');
              return;
            }
            note('Stream interrupted, retrying in 3 s (' + failures + ' of ' + MAX_STREAM_RETRIES + ').');
            await sleep(RETRY_MS, ac.signal);
          } else {
            failures = 0;
            note('Reconnecting.');
            await sleep(RECONNECT_MS, ac.signal);
          }
          if (ac.signal.aborted) {
            return;
          }
          reconnecting = true;
        }
      } finally {
        if (abortCtl === ac) {
          abortCtl = null;
        }
      }
    }

    async function initOutput() {
      const myGen = ++gen;
      const res = await api('GET', '/api/tail?task=' + enc(ref) + '&since=0&max=' + TAIL_MAX);
      if (stopped || myGen !== gen) {
        return;
      }
      if (res.status === 401) {
        showLoggedOut(NOT_LOGGED_IN);
        return;
      }
      tailDone = true;
      if (!res.ok || !res.data) {
        note('Could not read the output: ' + errText(res));
        return;
      }
      const d = res.data;
      if (typeof d.next === 'number') {
        nextOff = d.next;
      }
      if (d.lost) {
        lostNote.classList.remove('hidden');
      }
      append(d.text);
      if (d.live) {
        openStream();
      } else {
        note(out.length ? 'The session is not live. Showing the stored output.' : 'The session is not live and has no output.');
      }
    }

    async function doMerge() {
      const res = await post('/api/merge', { task: ref });
      if (res.ok && res.data) {
        showMsg(msg, 'ok', 'Merged ' + clean(res.data.branch, { max: 200 }) + ' into ' +
          clean(res.data.integration_branch, { max: 200 }) + ' at ' + clean(res.data.merge_sha, { max: 80 }) + '.');
        poll();
        return;
      }
      const e = res.data && res.data.error;
      if (res.status === 409 && e && e.code === 'approval_required') {
        showMsg(msg, 'warn', 'Merge needs approval ' + clean(e.subject, { max: 80 }) +
          ': approve it in the Decisions page, then press Merge again.');
        return;
      }
      if (res.status !== 401) {
        showMsg(msg, 'err', errText(res));
      }
    }

    async function doSend() {
      const text = sendInput.value;
      const bytes = new TextEncoder().encode(text).length;
      if (bytes > MAX_SEND_BYTES) {
        showMsg(msg, 'err', 'The text is ' + bytes + ' bytes; the limit is ' + MAX_SEND_BYTES + ' bytes.');
        return;
      }
      if (!text && !nl.checked) {
        showMsg(msg, 'err', 'Type something to send.');
        return;
      }
      const res = await post('/api/send', { task: ref, text: text, newline: !!nl.checked });
      if (res.ok && res.data) {
        sendInput.value = '';
        showMsg(msg, 'ok', 'Sent ' + num(res.data.bytes) + ' bytes.');
      } else if (res.status !== 401) {
        showMsg(msg, 'err', errText(res));
      }
    }

    function renderFields(run, t) {
      titlebar.replaceChildren(el('h2', '', 'Task ' + ref), badge(t.status));
      const dl = el('dl', 'fields');
      field(dl, 'Title', t.title);
      field(dl, 'Status', badge(t.status));
      field(dl, 'Worker', t.worker);
      field(dl, 'Attempt', 'attempt ' + num(t.attempt) + ', retries used ' + num(t.retries_used));
      field(dl, 'Depends on', Array.isArray(t.deps) && t.deps.length ? t.deps.join(', ') : null);
      field(dl, 'Branch', t.branch);
      field(dl, 'Worktree', t.worktree);
      field(dl, 'Head', t.head);
      field(dl, 'Merged', t.merged);
      field(dl, 'Summary', t.summary, true);
      field(dl, 'Failure', t.failure, true);
      field(dl, 'Blocked by', t.block ? clean(t.block.kind) + ': ' + clean(t.block.subject) + ' ' + clean(t.block.detail, { multi: true, max: 2000 }) : null, true);
      field(dl, 'Live session', t.live ? 'yes' : 'no');
      const gates = Array.isArray(t.gates) ? t.gates : [];
      const gbox = el('div', 'gatesbox');
      gbox.appendChild(el('h3', '', 'Quality gates'));
      if (!gates.length) {
        gbox.appendChild(el('p', 'muted', 'No quality gate results yet.'));
      } else {
        const table = el('table', 'grid');
        const head = el('tr');
        ['Gate', 'Result', 'Summary'].forEach(function (h) {
          head.appendChild(el('th', '', h));
        });
        table.appendChild(head);
        gates.forEach(function (g) {
          const tr = el('tr');
          tr.appendChild(el('td', 'wrap', g.name));
          const rc = el('td');
          rc.appendChild(el('span', 'badge ' + (g.passed ? 'st-ok' : 'st-fail'), g.passed ? 'pass' : 'fail'));
          tr.appendChild(rc);
          tr.appendChild(el('td', 'wrap', g.summary));
          table.appendChild(tr);
        });
        gbox.appendChild(table);
      }
      fields.replaceChildren(dl, gbox);
    }

    return {
      node: root,
      start: function () {
        initOutput();
      },
      update: function (d) {
        const run = d.runs.find(function (r) {
          return r && r.id === runId;
        });
        const t = run && Array.isArray(run.tasks) ? run.tasks.find(function (x) {
          return x && x.id === taskId;
        }) : null;
        if (!t) {
          titlebar.replaceChildren(el('h2', '', 'Task ' + ref));
          fields.replaceChildren(el('p', 'notice', 'Task not found: ' + ref));
          mergeBtn.disabled = true;
          sendBox.classList.add('hidden');
          fieldsSig = null;
          return;
        }
        const s = JSON.stringify(t);
        if (s !== fieldsSig) {
          fieldsSig = s;
          renderFields(run, t);
        }
        syncGov();
        mergeBtn.disabled = !(t.status === 'completed' && t.merged === null);
        sendBox.classList.toggle('hidden', !t.live);
        if (t.live) {
          if (tailDone && abortCtl === null && !halted) {
            openStream();
          }
        } else {
          halted = false;
        }
      },
      stop: function () {
        stopped = true;
        gen += 1;
        closeStream();
        if (flushTimer !== null) {
          clearTimeout(flushTimer);
          flushTimer = null;
        }
        out = '';
        pre.textContent = '';
      }
    };
  }

  // ------------------------------------------------------- decisions view

  function decisionsView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Decisions'));
    const msg = el('div', 'msg');
    root.appendChild(msg);
    const gatesBox = el('div', 'gates');
    const approvalsBox = el('div', 'approvals');
    root.appendChild(gatesBox);
    root.appendChild(approvalsBox);
    let sig = null;

    function whoFor(x) {
      return clean(x.run) + '/' + clean(x.task);
    }

    async function resolveGate(g, area) {
      const answer = area.value.trim();
      if (!answer) {
        showMsg(msg, 'err', 'Type or pick an answer for gate ' + clean(g.id, { max: 80 }) + ' first.');
        return;
      }
      const res = await post('/api/gate/resolve', { id: g.id, answer: answer });
      if (res.ok && res.data) {
        drafts.delete(g.id);
        showMsg(msg, 'ok', 'Gate ' + clean(res.data.id, { max: 80 }) + ' resolved for task ' + clean(res.data.task, { max: 200 }) + '.');
        poll();
      } else if (res.status !== 401) {
        showMsg(msg, 'err', errText(res));
      }
    }

    async function decide(a, approve) {
      const digest = a.digest;
      if (typeof digest !== 'string' || !/^[0-9a-f]{64}$/.test(digest)) {
        showMsg(msg, 'err', 'Approval ' + clean(a.id, { max: 80 }) + ' has no valid digest; refusing to send a decision.');
        return;
      }
      const res = await post('/api/approval/decide', { id: a.id, approve: approve, digest: digest });
      if (res.ok && res.data) {
        showMsg(msg, 'ok', 'Approval ' + clean(res.data.id, { max: 80 }) + ': ' + clean(res.data.decision, { max: 40 }) + '.');
        poll();
      } else if (res.status !== 401) {
        showMsg(msg, 'err', errText(res));
      }
    }

    function gateCard(g) {
      const card = el('article', 'card');
      const head = el('div', 'cardhead');
      head.appendChild(el('strong', '', 'Gate ' + clean(g.id)));
      head.appendChild(el('span', 'muted', whoFor(g)));
      head.appendChild(el('span', 'badge st-review', g.kind));
      card.appendChild(head);
      card.appendChild(el('div', 'multi', g.text, true));
      const area = el('textarea', 'textin answer');
      area.setAttribute('rows', '2');
      area.setAttribute('aria-label', 'Answer for gate ' + short(g.id, 40));
      area.value = drafts.get(g.id) || '';
      area.addEventListener('input', function () {
        drafts.set(g.id, area.value);
      });
      const choices = Array.isArray(g.choices) ? g.choices : [];
      if (choices.length) {
        const row = el('div', 'row');
        choices.forEach(function (c) {
          const b = el('button', 'btn', c);
          b.setAttribute('type', 'button');
          b.addEventListener('click', function () {
            area.value = String(c);
            drafts.set(g.id, area.value);
            area.focus();
          });
          row.appendChild(b);
        });
        card.appendChild(row);
      }
      card.appendChild(area);
      const row2 = el('div', 'row');
      row2.appendChild(actionButton('resolve:' + g.id, 'Resolve', 'decisions', 'primary', function () {
        resolveGate(g, area);
      }));
      card.appendChild(row2);
      return card;
    }

    function approvalCard(a) {
      const card = el('article', 'card');
      const head = el('div', 'cardhead');
      head.appendChild(el('strong', '', 'Approval ' + clean(a.id)));
      head.appendChild(el('span', 'muted', whoFor(a)));
      head.appendChild(badge(a.status));
      card.appendChild(head);
      const dl = el('dl', 'fields');
      field(dl, 'Purpose', a.purpose);
      field(dl, 'Tool', a.tool);
      card.appendChild(dl);
      card.appendChild(el('div', 'label', 'Exact action'));
      card.appendChild(el('pre', 'mono block', a.display, true));
      card.appendChild(el('div', 'label', 'Command'));
      card.appendChild(el('pre', 'mono block', a.command, true));
      const args = Array.isArray(a.args) ? a.args : [];
      card.appendChild(el('div', 'label', 'Arguments (' + args.length + ')'));
      if (!args.length) {
        card.appendChild(el('p', 'muted', 'none'));
      } else {
        const ol = el('ol', 'args');
        args.forEach(function (x) {
          ol.appendChild(el('li', 'mono', x));
        });
        card.appendChild(ol);
      }
      card.appendChild(el('div', 'label', 'Digest (the decision is bound to this)'));
      card.appendChild(el('code', 'mono block digest', a.digest));
      const when = el('div', 'row');
      when.appendChild(el('span', 'muted', 'requested ' + fmtTime(a.requested_ms) + ', expires ' + fmtTime(a.expires_ms)));
      if (a.status === 'pending') {
        const cd = el('strong', 'countdown', fmtRemaining(a.expires_ms - estimateNow()));
        countdowns.push({ node: cd, expires: a.expires_ms });
        when.appendChild(cd);
      }
      card.appendChild(when);
      if (a.status === 'pending') {
        const row = el('div', 'row');
        row.appendChild(actionButton('approve:' + a.id, 'Approve', 'decisions', 'primary', function () {
          decide(a, true);
        }));
        row.appendChild(actionButton('deny:' + a.id, 'Deny', 'decisions', 'danger', function () {
          decide(a, false);
        }));
        card.appendChild(row);
      }
      return card;
    }

    return {
      node: root,
      update: function (d) {
        const gates = Array.isArray(d.gates) ? d.gates : [];
        const approvals = Array.isArray(d.approvals) ? d.approvals.slice() : [];
        const s = JSON.stringify([gates, approvals]);
        if (s === sig) {
          return;
        }
        sig = s;
        dropGroup('decisions');
        countdowns = [];
        clearConfirm();

        gatesBox.replaceChildren();
        gatesBox.appendChild(el('h3', '', 'Decision gates'));
        const open = gates.filter(function (g) {
          return g && g.open;
        });
        const closed = gates.filter(function (g) {
          return g && !g.open;
        }).slice(-20);
        if (!open.length) {
          gatesBox.appendChild(el('p', 'muted', 'No open gates.'));
        }
        open.forEach(function (g) {
          gatesBox.appendChild(gateCard(g));
        });
        if (closed.length) {
          gatesBox.appendChild(el('h4', '', 'Answered gates (latest ' + closed.length + ')'));
          closed.forEach(function (g) {
            const li = el('div', 'closed');
            li.appendChild(el('strong', '', 'Gate ' + clean(g.id) + ' '));
            li.appendChild(el('span', 'muted', whoFor(g) + ': '));
            li.appendChild(el('span', '', g.answer === null ? 'no answer' : g.answer));
            gatesBox.appendChild(li);
          });
        }

        approvalsBox.replaceChildren();
        approvalsBox.appendChild(el('h3', '', 'Approvals'));
        approvals.sort(function (x, y) {
          const px = x.status === 'pending' ? 0 : 1;
          const py = y.status === 'pending' ? 0 : 1;
          return px - py || (y.requested_ms || 0) - (x.requested_ms || 0);
        });
        const shown = approvals.slice(0, 50);
        if (!shown.length) {
          approvalsBox.appendChild(el('p', 'muted', 'No approvals.'));
        }
        shown.forEach(function (a) {
          approvalsBox.appendChild(approvalCard(a));
        });
        if (approvals.length > shown.length) {
          approvalsBox.appendChild(el('p', 'muted', (approvals.length - shown.length) + ' older approvals are not shown.'));
        }
        tickCountdowns();
      }
    };
  }

  // ---------------------------------------------------------- events view

  function eventsView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Events'));
    const banner = el('div', 'chain unknown', 'Checking the audit log chain.');
    const list = el('ol', 'events');
    root.appendChild(banner);
    root.appendChild(list);
    let timer = null;
    let busy = false;
    let active = true;

    function paintChain(res) {
      if (res.ok && res.data && res.data.ok === true) {
        banner.className = 'chain good';
        banner.textContent = clean('Audit log chain verified: ' + num(res.data.records) + ' records, head ' +
          clean(res.data.head_hash, { max: 80 }) + '.');
      } else if (res.ok && res.data) {
        banner.className = 'chain bad';
        banner.textContent = clean('AUDIT LOG CHAIN BROKEN. The log may have been tampered with: ' +
          clean(res.data.error === null || res.data.error === undefined ? 'no detail given' : res.data.error, { max: 1000 }));
      } else {
        banner.className = 'chain unknown';
        banner.textContent = clean('The audit log chain could not be checked: ' + errText(res));
      }
    }

    async function load() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const both = await Promise.all([api('GET', '/api/log'), api('GET', '/api/events?limit=' + MAX_EVENTS)]);
        if (!active) {
          return;
        }
        if (both[0].status === 401 || both[1].status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        paintChain(both[0]);
        const ev = both[1];
        if (ev.ok && ev.data && Array.isArray(ev.data.events)) {
          const items = ev.data.events.slice(-MAX_EVENTS).reverse();
          const frag = [];
          items.forEach(function (e) {
            const li = el('li', 'event');
            li.appendChild(el('span', 'seq', '#' + num(e.seq)));
            li.appendChild(el('span', 'ts', fmtTime(e.ts_ms)));
            li.appendChild(el('span', 'line', e.line));
            frag.push(li);
          });
          if (!frag.length) {
            frag.push(el('li', 'muted', 'No events yet.'));
          }
          list.replaceChildren.apply(list, frag);
        } else {
          setLine('Events unavailable: ' + errText(ev));
        }
      } finally {
        busy = false;
      }
    }

    async function cycle() {
      await load();
      schedule();
    }

    function schedule() {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      if (active && !loggedOut && visible()) {
        timer = setTimeout(cycle, EVENTS_MS);
      }
    }

    return {
      node: root,
      start: function () {
        cycle();
      },
      resume: function () {
        cycle();
      },
      pause: function () {
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
      },
      stop: function () {
        active = false;
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
      }
    };
  }

  // ---------------------------------------------------------- health view

  function healthView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Health'));
    const summary = el('div', 'chain unknown', 'Running the checks.');
    const holder = el('div', 'checks');
    const refresh = el('button', 'btn', 'Refresh');
    refresh.setAttribute('type', 'button');
    root.appendChild(summary);
    root.appendChild(refresh);
    root.appendChild(holder);
    let active = true;
    let busy = false;

    async function load() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const res = await api('GET', '/api/health');
        if (!active) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (!res.ok || !res.data || !Array.isArray(res.data.checks)) {
          summary.className = 'chain unknown';
          summary.textContent = clean('Health checks unavailable: ' + errText(res));
          holder.replaceChildren();
          return;
        }
        summary.className = 'chain ' + (res.data.ok ? 'good' : 'bad');
        summary.textContent = res.data.ok ? 'All checks passed.' : 'At least one check failed.';
        const table = el('table', 'grid');
        const head = el('tr');
        ['Check', 'Status', 'Detail'].forEach(function (h) {
          head.appendChild(el('th', '', h));
        });
        table.appendChild(head);
        res.data.checks.forEach(function (c) {
          const tr = el('tr');
          tr.appendChild(el('td', 'wrap', c.name));
          const sc = el('td');
          sc.appendChild(badge(c.status));
          tr.appendChild(sc);
          tr.appendChild(el('td', 'wrap', c.detail, true));
          table.appendChild(tr);
        });
        holder.replaceChildren(table);
      } finally {
        busy = false;
      }
    }

    refresh.addEventListener('click', load);
    return {
      node: root,
      start: load,
      resume: load,
      stop: function () {
        active = false;
      }
    };
  }

  // ------------------------------------------------------------ goals views

  function tagged(prefix, set, value, label) {
    const known = typeof value === 'string' && set.has(value);
    return el('span', 'badge ' + prefix + '-' + (known ? value : 'unknown'), label === undefined ? value : label);
  }

  function routeLink(route, label) {
    const a = el('a', 'rlink', label);
    a.setAttribute('href', route);
    return a;
  }

  // A link only for ids that are plain, otherwise the text itself.
  function goalLink(id, label) {
    const text = label === undefined || label === null || label === '' ? id : label;
    if (typeof id === 'string' && ID_OK.test(id)) {
      return routeLink('#/goal/' + id, text);
    }
    return el('span', 'mono-text', text);
  }

  function taskLink(ref, label) {
    const text = label === undefined || label === null || label === '' ? ref : label;
    if (typeof ref === 'string' && REF_OK.test(ref)) {
      return routeLink('#/task/' + ref, text);
    }
    return el('span', 'mono-text', text);
  }

  function shortHash(v) {
    return clean(v, { max: 80 }).slice(0, 12);
  }

  function goalList() {
    return lastGoals && Array.isArray(lastGoals.goals) ? lastGoals.goals.filter(Boolean) : [];
  }

  function goalsLeft() {
    return goalList().filter(function (g) {
      return g.status !== 'verified' && g.status !== 'abandoned';
    }).length;
  }

  function governing(ref) {
    return goalList().filter(function (g) {
      return Array.isArray(g.tasks) && g.tasks.some(function (t) {
        return t && t.task === ref;
      });
    });
  }

  // The worst verification of every task that a goal names (failed, unverified, pending, verified).
  function verificationMap() {
    const m = new Map();
    goalList().forEach(function (g) {
      (Array.isArray(g.tasks) ? g.tasks : []).forEach(function (t) {
        if (!t || typeof t.task !== 'string') {
          return;
        }
        const rank = VERIF_RANK.get(t.verification) || 0;
        if (rank > (VERIF_RANK.get(m.get(t.task)) || 0)) {
          m.set(t.task, t.verification);
        }
      });
    });
    return m;
  }

  function makeTable(headers, cls) {
    const t = el('table', 'grid ' + (cls || ''));
    const head = el('tr');
    headers.forEach(function (h) {
      head.appendChild(el('th', '', h));
    });
    t.appendChild(head);
    return t;
  }

  function cell(content, cls, multi) {
    const td = el('td', cls || 'wrap');
    if (content && content.nodeType) {
      td.appendChild(content);
    } else if (content !== undefined && content !== null && content !== '') {
      td.textContent = clean(content, { multi: !!multi, max: multi ? 20000 : 4000 });
    }
    return td;
  }

  function scrollBox(node) {
    const box = el('div', 'tablescroll');
    box.appendChild(node);
    return box;
  }

  function taskChips(tasks) {
    const line = el('div', 'taskchips');
    const list = Array.isArray(tasks) ? tasks : [];
    list.slice(0, 20).forEach(function (t) {
      if (!t) {
        return;
      }
      const chip = el('span', 'taskchip');
      chip.appendChild(taskLink(t.task));
      chip.appendChild(badge(t.status));
      if (t.verification && t.verification !== 'none') {
        chip.appendChild(tagged('vf', VERIFICATION, t.verification));
      }
      line.appendChild(chip);
    });
    if (list.length > 20) {
      line.appendChild(el('span', 'muted small', (list.length - 20) + ' more tasks not shown'));
    }
    return line;
  }

  function goalsView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Goals'));
    const summary = el('p', 'goalsum', '');
    const remBox = el('div', 'remaining');
    const treeBox = el('div', 'goaltree');
    root.appendChild(summary);
    root.appendChild(el('h3', '', 'Remaining work'));
    root.appendChild(remBox);
    root.appendChild(el('h3', '', 'Goal tree'));
    root.appendChild(treeBox);
    let treeSig = null;
    let remSig = null;
    let busy = false;
    let active = true;

    function goalRow(g) {
      const depth = typeof g.depth === 'number' && g.depth > 0 ? Math.min(Math.floor(g.depth), 12) : 0;
      const row = el('div', 'goalrow indent-' + depth);
      const head = el('div', 'goalhead');
      head.appendChild(goalLink(g.id, g.title));
      head.appendChild(el('span', 'muted mono-text', g.id));
      head.appendChild(tagged('gs', GOAL_STATUS, g.status));
      head.appendChild(tagged('ct', CONTRACT, g.contract, 'contract ' + clean(g.contract, { max: 20 })));
      head.appendChild(el('span', 'muted small', num(g.evidence) + ' evidence'));
      if (typeof g.children === 'number' && g.children > 0) {
        head.appendChild(el('span', 'muted small', g.children + (g.children === 1 ? ' child' : ' children')));
      }
      row.appendChild(head);
      if (g.reason) {
        row.appendChild(el('div', 'muted small reason', g.reason));
      }
      if (Array.isArray(g.tasks) && g.tasks.length) {
        row.appendChild(taskChips(g.tasks));
      }
      return row;
    }

    function paintTree() {
      const sig = JSON.stringify([lastGoals, goalsNote]);
      if (sig === treeSig) {
        return;
      }
      treeSig = sig;
      if (lastGoals === null) {
        summary.textContent = clean(goalsNote ? 'Goals are not available: ' + goalsNote : 'Loading goals.', { max: 400 });
        treeBox.replaceChildren();
        return;
      }
      summary.textContent = num(lastGoals.total) + ' goals, ' + num(lastGoals.verified) + ' verified, ' +
        num(lastGoals.abandoned) + ' abandoned';
      const goals = goalList();
      if (!goals.length) {
        treeBox.replaceChildren(el('p', 'muted', 'There are no goals.'));
        return;
      }
      const rows = goals.slice(0, MAX_GOALS_SHOWN).map(goalRow);
      if (goals.length > MAX_GOALS_SHOWN) {
        rows.push(el('p', 'muted', (goals.length - MAX_GOALS_SHOWN) + ' more goals are not shown.'));
      }
      treeBox.replaceChildren.apply(treeBox, rows);
    }

    function section(title, list, headers, build) {
      const box = el('div', 'remsection');
      box.appendChild(el('h4', '', title + ' (' + list.length + ')'));
      if (!list.length) {
        box.appendChild(el('p', 'muted', 'none'));
        return box;
      }
      const table = makeTable(headers, 'auto');
      list.slice(0, MAX_LIST_SHOWN).forEach(function (x) {
        const tr = el('tr');
        build(x).forEach(function (c) {
          tr.appendChild(c);
        });
        table.appendChild(tr);
      });
      box.appendChild(scrollBox(table));
      if (list.length > MAX_LIST_SHOWN) {
        box.appendChild(el('p', 'muted small', (list.length - MAX_LIST_SHOWN) + ' more are not shown.'));
      }
      return box;
    }

    function paintRemaining(d) {
      const leaves = Array.isArray(d.open_leaves) ? d.open_leaves.filter(Boolean) : [];
      const blockers = Array.isArray(d.blockers) ? d.blockers.filter(Boolean) : [];
      const ready = Array.isArray(d.next_ready) ? d.next_ready.filter(Boolean) : [];
      remBox.replaceChildren(
        section('Open leaf goals', leaves, ['Goal', 'Status', 'Contract', 'Evidence'], function (g) {
          return [cell(goalLink(g.id, g.title)), cell(tagged('gs', GOAL_STATUS, g.status)),
            cell(tagged('ct', CONTRACT, g.contract, 'contract ' + clean(g.contract, { max: 20 }))), cell(num(g.evidence))];
        }),
        section('Blockers', blockers, ['Kind', 'Subject', 'Detail', 'Status'], function (b) {
          const subject = el('span');
          if (b.kind === 'task') {
            subject.appendChild(taskLink(b.task));
            subject.appendChild(document.createTextNode(' in goal '));
            subject.appendChild(goalLink(b.goal));
          } else {
            subject.appendChild(goalLink(b.goal, b.title));
          }
          return [cell(b.kind), cell(subject), cell(b.detail, 'wrap multi', true),
            cell(b.status ? tagged('gs', GOAL_STATUS, b.status, b.status) : '')];
        }),
        section('Next ready tasks', ready, ['Task', 'Goal', 'Title'], function (r) {
          return [cell(taskLink(r.task)), cell(goalLink(r.goal)), cell(r.title)];
        })
      );
    }

    async function loadRemaining() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const res = await api('GET', '/api/remaining');
        if (!active) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (!res.ok || !res.data) {
          remSig = null;
          remBox.replaceChildren(el('p', 'muted', 'Remaining work is not available: ' + errText(res)));
          return;
        }
        const sig = JSON.stringify(res.data);
        if (sig !== remSig) {
          remSig = sig;
          paintRemaining(res.data);
        }
      } finally {
        busy = false;
      }
    }

    return {
      node: root,
      start: function () {
        paintTree();
        loadRemaining();
      },
      update: function () {
        paintTree();
        loadRemaining();
      },
      stop: function () {
        active = false;
      }
    };
  }

  function goalView(id) {
    const root = el('section', 'page');
    root.appendChild(routeLink('#/goals', 'All goals'));
    const holder = el('div', 'goalpage');
    root.appendChild(holder);
    const openRows = new Set();
    let sig = null;
    let busy = false;
    let active = ID_OK.test(id);
    if (!active) {
      holder.appendChild(el('p', 'notice', 'Not a valid goal id: ' + id));
    } else {
      holder.appendChild(el('p', 'muted', 'Loading the goal.'));
    }

    function evidenceDetail(e) {
      const box = el('div', 'evidetail');
      box.appendChild(el('div', 'label', 'Commit'));
      box.appendChild(el('code', 'mono block', e.commit));
      box.appendChild(el('div', 'label', 'Tree'));
      box.appendChild(el('code', 'mono block', e.tree));
      box.appendChild(el('div', 'label', 'Output'));
      const text = typeof e.output === 'string' ? e.output : '';
      if (!text) {
        box.appendChild(el('p', 'muted', 'no output'));
      } else {
        if (text.length > MAX_EVIDENCE_OUTPUT) {
          box.appendChild(el('p', 'notice small', 'Output cut: showing the first ' + MAX_EVIDENCE_OUTPUT + ' of ' + text.length + ' characters.'));
        }
        box.appendChild(el('pre', 'mono block evout', text.slice(0, MAX_EVIDENCE_OUTPUT), true));
      }
      return box;
    }

    function historyTable(history) {
      const table = makeTable(['Id', 'Check', 'Verdict', 'Exit', 'Commit', 'Tree', 'Dirty', 'Counts', 'Task', 'Time', 'Summary'], 'auto evidence');
      history.slice(0, MAX_EVIDENCE_SHOWN).forEach(function (e, i) {
        const key = typeof e.id === 'string' ? e.id : String(i);
        const tr = el('tr', 'evrow' + (e.counts === true ? '' : ' dim'));
        tr.setAttribute('tabindex', '0');
        tr.setAttribute('aria-expanded', 'false');
        tr.appendChild(cell(e.id));
        tr.appendChild(cell(e.check));
        tr.appendChild(cell(tagged('vd', VERDICT, e.verdict)));
        tr.appendChild(cell(typeof e.exit_code === 'number' ? String(e.exit_code) : 'none'));
        tr.appendChild(cell(el('code', 'mono-text', shortHash(e.commit))));
        tr.appendChild(cell(el('code', 'mono-text', shortHash(e.tree))));
        tr.appendChild(cell(e.dirty ? 'yes' : 'no'));
        tr.appendChild(cell(e.counts === true ? 'yes' : 'no'));
        tr.appendChild(cell(e.task));
        tr.appendChild(cell(fmtTime(e.at_ms)));
        tr.appendChild(cell(e.summary, 'wrap multi', true));
        const more = el('tr', 'evdetail hidden');
        const td = el('td', 'wrap');
        td.setAttribute('colspan', '11');
        more.appendChild(td);
        let built = false;
        const toggle = function () {
          const opening = more.classList.contains('hidden');
          if (opening && !built) {
            td.appendChild(evidenceDetail(e));
            built = true;
          }
          more.classList.toggle('hidden', !opening);
          tr.setAttribute('aria-expanded', opening ? 'true' : 'false');
          if (opening) {
            openRows.add(key);
          } else {
            openRows.delete(key);
          }
        };
        tr.addEventListener('click', toggle);
        tr.addEventListener('keydown', function (ev) {
          if (ev.key === 'Enter' || ev.key === ' ') {
            toggle();
          }
        });
        table.appendChild(tr);
        table.appendChild(more);
        if (openRows.has(key)) {
          toggle();
        }
      });
      return table;
    }

    function paint(g) {
      const top = el('div', 'titlebar');
      top.appendChild(el('h2', '', g.title));
      top.appendChild(tagged('gs', GOAL_STATUS, g.status));
      top.appendChild(tagged('ct', CONTRACT, g.contract, 'contract ' + clean(g.contract, { max: 20 })));
      const dl = el('dl', 'fields');
      field(dl, 'Goal id', g.id);
      field(dl, 'Intent', g.intent, true);
      const st = el('span');
      st.appendChild(tagged('gs', GOAL_STATUS, g.status));
      st.appendChild(el('span', 'multi', g.reason ? ' ' + clean(g.reason, { multi: true, max: 2000 }) : ''));
      field(dl, 'Status and reason', st);
      const ct = el('span');
      ct.appendChild(tagged('ct', CONTRACT, g.contract, 'contract ' + clean(g.contract, { max: 20 })));
      if (typeof g.contract_id === 'string' && g.contract_id) {
        ct.appendChild(el('code', 'mono block', g.contract_id));
        ct.appendChild(el('span', 'muted small', 'short form ' + shortHash(g.contract_id)));
      }
      field(dl, 'Contract', ct);
      field(dl, 'Parent', typeof g.parent === 'string' && g.parent ? goalLink(g.parent) : null);
      const kids = Array.isArray(g.child_ids) ? g.child_ids : [];
      if (kids.length) {
        const span = el('span', 'links');
        kids.slice(0, MAX_LIST_SHOWN).forEach(function (c) {
          span.appendChild(goalLink(c));
          span.appendChild(document.createTextNode(' '));
        });
        field(dl, 'Children', span);
      } else {
        field(dl, 'Children', null);
      }
      field(dl, 'Depth', num(g.depth));
      field(dl, 'Evidence recorded', num(g.evidence));

      const parts = [top, dl];
      parts.push(el('h3', '', 'Tasks'));
      const tasks = Array.isArray(g.tasks) ? g.tasks.filter(Boolean) : [];
      if (!tasks.length) {
        parts.push(el('p', 'muted', 'No task is named by this goal.'));
      } else {
        const table = makeTable(['Task', 'Status', 'Verification'], 'auto');
        tasks.slice(0, MAX_LIST_SHOWN).forEach(function (t) {
          const tr = el('tr');
          tr.appendChild(cell(taskLink(t.task)));
          tr.appendChild(cell(badge(t.status)));
          tr.appendChild(cell(tagged('vf', VERIFICATION, t.verification)));
          table.appendChild(tr);
        });
        parts.push(scrollBox(table));
      }

      parts.push(el('h3', '', 'Checks'));
      const checks = Array.isArray(g.checks) ? g.checks.filter(Boolean) : [];
      if (!checks.length) {
        parts.push(el('p', 'muted', 'This goal has no checks.'));
      } else {
        const table = makeTable(['Check', 'Kind', 'Mandatory', 'Detail', 'Newest verdict', 'Summary', 'Commit', 'Tree', 'Counts'], 'auto');
        checks.slice(0, MAX_LIST_SHOWN).forEach(function (c) {
          const l = c.latest && typeof c.latest === 'object' ? c.latest : null;
          const tr = el('tr', l && l.counts !== true ? 'dim' : '');
          tr.appendChild(cell(c.check));
          tr.appendChild(cell(c.kind));
          tr.appendChild(cell(c.mandatory ? 'yes' : 'no'));
          tr.appendChild(cell(c.detail, 'wrap multi', true));
          tr.appendChild(cell(l ? tagged('vd', VERDICT, l.verdict) : 'no evidence'));
          tr.appendChild(cell(l ? l.summary : '', 'wrap multi', true));
          tr.appendChild(cell(l ? el('code', 'mono-text', shortHash(l.commit)) : ''));
          tr.appendChild(cell(l ? el('code', 'mono-text', shortHash(l.tree)) : ''));
          tr.appendChild(cell(l ? (l.counts === true ? 'yes' : 'no') : ''));
          table.appendChild(tr);
        });
        parts.push(scrollBox(table));
      }

      parts.push(el('h3', '', 'Evidence history'));
      parts.push(el('p', 'muted small', 'Evidence that does not count (dirty worktree or not the reviewed commit) is shown dimmed. Click a row for the full output, commit and tree.'));
      const history = Array.isArray(g.history) ? g.history.filter(Boolean) : [];
      if (!history.length) {
        parts.push(el('p', 'muted', 'No evidence has been recorded.'));
      } else {
        parts.push(scrollBox(historyTable(history)));
        if (history.length > MAX_EVIDENCE_SHOWN) {
          parts.push(el('p', 'muted small', (history.length - MAX_EVIDENCE_SHOWN) + ' older entries are not shown.'));
        }
      }
      holder.replaceChildren.apply(holder, parts);
    }

    async function load() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const res = await api('GET', '/api/goal?id=' + enc(id));
        if (!active) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (!res.ok || !res.data) {
          sig = null;
          holder.replaceChildren(el('p', 'notice', 'Could not load the goal: ' + errText(res)));
          return;
        }
        const s = JSON.stringify(res.data);
        if (s !== sig) {
          sig = s;
          paint(res.data);
        }
      } finally {
        busy = false;
      }
    }

    return {
      node: root,
      start: load,
      update: load,
      stop: function () {
        active = false;
      }
    };
  }

  // ------------------------------------------------------------ fan-out groups
  // Read only: a fan-out is created and its winner picked with the command line (a pick is a merge
  // and goes through the approval). These pages show the groups and the comparison the daemon
  // computed from facts it knows; every text in them (titles, summaries, file names) is untrusted
  // and goes through el() and cell() like everything else.

  const FX = new Set(['running', 'finished', 'decided', 'closed', 'completed', 'failed', 'winner', 'superseded', 'verified', 'unverified']);
  const MAX_FX_FILES = 100;

  function fxBadge(value) {
    const known = typeof value === 'string' && FX.has(value);
    return el('span', 'badge fx-' + (known ? value : 'unknown'), value === null || value === undefined ? 'n/a' : value);
  }

  function fxDuration(ms) {
    if (typeof ms !== 'number' || !Number.isFinite(ms)) {
      return 'n/a';
    }
    if (ms < 1000) {
      return ms + ' ms';
    }
    const s = ms / 1000;
    return s < 120 ? s.toFixed(1) + ' s' : (s / 60).toFixed(1) + ' min';
  }

  function groupLink(id, label) {
    const text = label === undefined || label === null || label === '' ? id : label;
    if (typeof id === 'string' && ID_OK.test(id)) {
      return routeLink('#/group/' + id, text);
    }
    return el('span', 'mono-text', text);
  }

  function groupsView() {
    const root = el('section', 'page');
    root.appendChild(el('h2', '', 'Fan-out groups'));
    root.appendChild(el('p', 'muted small', 'One prompt given to several agents. Groups are created and picked with the command line (fleet fanout, fleet compare, fleet pick).'));
    const box = el('div', 'fxlist');
    root.appendChild(box);
    let sig = null;
    let busy = false;
    let active = true;
    box.appendChild(el('p', 'muted', 'Loading the groups.'));

    function paint(d) {
      const groups = Array.isArray(d.groups) ? d.groups.filter(Boolean) : [];
      if (!groups.length) {
        box.replaceChildren(el('p', 'muted', 'There are no fan-out groups.'));
        return;
      }
      const table = makeTable(['Group', 'Title', 'Status', 'Run', 'Members', 'Completed', 'Failed', 'Winner', 'Evidence of the winner'], 'auto');
      groups.slice(0, MAX_LIST_SHOWN).forEach(function (g) {
        const c = g.counts || {};
        const tr = el('tr');
        [cell(groupLink(g.id)), cell(g.title), cell(fxBadge(g.status)), cell(g.run), cell(num(c.members)),
          cell(num(c.completed)), cell(num(c.failed)), cell(g.winner || ''), cell(g.winner_verification ? fxBadge(g.winner_verification) : '')].forEach(function (x) {
          tr.appendChild(x);
        });
        table.appendChild(tr);
      });
      box.replaceChildren(scrollBox(table));
      if (groups.length > MAX_LIST_SHOWN) {
        box.appendChild(el('p', 'muted small', (groups.length - MAX_LIST_SHOWN) + ' more are not shown.'));
      }
    }

    async function load() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const res = await api('GET', '/api/groups');
        if (!active) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (!res.ok || !res.data) {
          sig = null;
          box.replaceChildren(el('p', 'muted', 'Groups are not available: ' + errText(res)));
          return;
        }
        const s = JSON.stringify(res.data);
        if (s !== sig) {
          sig = s;
          paint(res.data);
        }
      } finally {
        busy = false;
      }
    }

    return {
      node: root,
      start: load,
      update: load,
      stop: function () {
        active = false;
      }
    };
  }

  function groupView(id) {
    const root = el('section', 'page');
    root.appendChild(routeLink('#/groups', 'All groups'));
    const holder = el('div', 'fxpage');
    root.appendChild(holder);
    let sig = null;
    let busy = false;
    let active = ID_OK.test(id);
    if (!active) {
      holder.appendChild(el('p', 'notice', 'Not a valid group id: ' + id));
    } else {
      holder.appendChild(el('p', 'muted', 'Loading the comparison.'));
    }

    function line(label, value) {
      const p = el('p', 'fxline');
      p.appendChild(el('span', 'label', label + ' '));
      if (value && value.nodeType) {
        p.appendChild(value);
      } else {
        p.appendChild(document.createTextNode(clean(value, { max: 600 })));
      }
      return p;
    }

    function memberDetail(run, m) {
      const d = el('details', 'fxdetail');
      d.appendChild(el('summary', '', m.task + ' in detail'));
      const v = m.verification || {};
      d.appendChild(el('div', 'label', 'Verification: ' + clean(v.status, { max: 20 })));
      d.appendChild(el('p', 'small', v.reason));
      const checks = Array.isArray(v.checks) ? v.checks.filter(Boolean) : [];
      if (checks.length) {
        const t = makeTable(['Goal', 'Check', 'Kind', 'Mandatory', 'Evidence', 'Summary'], 'auto');
        checks.slice(0, MAX_LIST_SHOWN).forEach(function (c) {
          const tr = el('tr');
          [cell(c.goal), cell(c.check), cell(c.kind), cell(c.mandatory ? 'yes' : 'no'), cell(fxBadge(c.status)), cell(c.summary)].forEach(function (x) {
            tr.appendChild(x);
          });
          t.appendChild(tr);
        });
        d.appendChild(scrollBox(t));
      }
      const sc = (m.scope && Array.isArray(m.scope.checks)) ? m.scope.checks.filter(Boolean) : [];
      sc.forEach(function (r) {
        d.appendChild(el('div', 'label', 'Scope check ' + clean(r.check, { max: 60 }) + (r.mandatory ? '' : ' (optional)')));
        const outside = Array.isArray(r.outside) ? r.outside : [];
        const allowed = Array.isArray(r.allowed_touched) ? r.allowed_touched : [];
        d.appendChild(el('p', 'small', 'allowed paths touched: ' + (allowed.length ? allowed.join(', ') : 'none')));
        d.appendChild(el('p', 'small ' + (outside.length ? 'fxbad' : ''), 'paths outside the scope: ' + num(r.outside_count) + (outside.length ? ' - ' + outside.join(', ') : '')));
      });
      const diff = m.diff || {};
      if (diff.available) {
        d.appendChild(el('div', 'label', 'Files changed (' + num(diff.files) + (diff.truncated ? ', list cut' : '') + ')'));
        const changes = Array.isArray(diff.changes) ? diff.changes.filter(Boolean) : [];
        const t = makeTable(['Path', 'Change', '+', '-'], 'auto');
        changes.slice(0, MAX_FX_FILES).forEach(function (c) {
          const tr = el('tr');
          const where = c.old_path ? c.old_path + ' -> ' + c.path : c.path;
          [cell(where), cell(c.binary ? c.kind + ' (binary)' : c.kind), cell(num(c.insertions)), cell(num(c.deletions))].forEach(function (x) {
            tr.appendChild(x);
          });
          t.appendChild(tr);
        });
        d.appendChild(scrollBox(t));
      } else {
        d.appendChild(el('p', 'muted small', 'The diff is not available: ' + clean(diff.error, { max: 400 })));
      }
      d.appendChild(el('div', 'label', 'Commit and tree the evidence is bound to'));
      d.appendChild(el('code', 'mono block', m.head || 'none'));
      d.appendChild(el('code', 'mono block', m.tree || 'none'));
      if (m.branch_moved) {
        d.appendChild(el('p', 'notice small', 'The branch moved after review (now at ' + shortHash(m.branch_head) + '): this member cannot be picked.'));
      }
      d.appendChild(el('div', 'label', 'What the worker reported (not used for anything above)'));
      d.appendChild(el('p', 'small', m.summary === null || m.summary === undefined ? 'nothing' : m.summary));
      if (m.failure) {
        d.appendChild(el('p', 'small fxbad', 'Why it failed: ' + clean(m.failure, { max: 600 })));
      }
      return d;
    }

    function paint(d) {
      const g = d.group || {};
      const run = g.run;
      const parts = [];
      parts.push(el('h2', '', g.title));
      parts.push(line('Group', g.id));
      parts.push(line('Status', fxBadge(g.status)));
      parts.push(line('Run', g.run));
      parts.push(line('Winner', g.winner ? g.winner + ' (merged as ' + shortHash(g.merge_sha) + ', evidence ' + clean(g.winner_verification, { max: 20 }) + ')' : 'not picked'));
      const rk = d.ranking || {};
      parts.push(el('h3', '', 'Ranking rule (advisory)'));
      parts.push(el('p', 'mono-text', rk.formula));
      const crit = Array.isArray(rk.described) ? rk.described.filter(Boolean) : [];
      if (crit.length) {
        const ol = el('ol', 'fxrule');
        crit.forEach(function (c) {
          ol.appendChild(el('li', 'small', c.criterion + ': ' + clean(c.rule, { max: 300 })));
        });
        parts.push(ol);
      }
      parts.push(el('h3', '', 'Members'));
      const members = Array.isArray(d.members) ? d.members.filter(Boolean) : [];
      const table = makeTable(['Rank', 'Task', 'Worker', 'State', 'Evidence', 'In scope', 'Files', '+/-', 'Gates', 'Attempts', 'Time', 'Why here'], 'auto');
      members.forEach(function (m) {
        const df = m.diff || {};
        const tr = el('tr');
        const ref = run && m.task ? run + '/' + m.task : '';
        [cell(num(m.rank)), cell(ref ? taskLink(ref, m.task) : m.task), cell(m.worker || ''), cell(fxBadge(m.state)),
          cell(fxBadge(m.verification && m.verification.status)),
          cell(m.scope && m.scope.unknown ? 'unknown' : (m.scope && m.scope.in_scope ? 'yes' : 'no')),
          cell(df.available ? num(df.files) : 'n/a'),
          cell(df.available ? '+' + num(df.insertions) + ' -' + num(df.deletions) : 'n/a'),
          cell(m.gates_passed ? 'passed' : 'failed'), cell(num(m.attempts)), cell(fxDuration(m.duration_ms)),
          cell(m.decided_by)].forEach(function (x) {
          tr.appendChild(x);
        });
        table.appendChild(tr);
      });
      parts.push(scrollBox(table));
      members.forEach(function (m) {
        parts.push(memberDetail(run, m));
      });
      const ov = d.overlap || {};
      parts.push(el('h3', '', 'Files changed by several members'));
      const shared = Array.isArray(ov.shared) ? ov.shared.filter(Boolean) : [];
      if (!shared.length) {
        parts.push(el('p', 'muted', 'none' + (ov.incomplete ? ' (some diffs could not be read in full)' : '')));
      } else {
        const t = makeTable(['Path', 'Members'], 'auto');
        shared.slice(0, MAX_LIST_SHOWN).forEach(function (s) {
          const tr = el('tr');
          [cell(s.path), cell(Array.isArray(s.members) ? s.members.join(', ') : '')].forEach(function (x) {
            tr.appendChild(x);
          });
          t.appendChild(tr);
        });
        parts.push(scrollBox(t));
        parts.push(el('p', 'muted small', num(ov.shared_count) + ' shared files' + (ov.incomplete ? '; the picture may be incomplete' : '')));
      }
      const tasks = Array.isArray(ov.tasks) ? ov.tasks : [];
      const matrix = Array.isArray(ov.matrix) ? ov.matrix : [];
      if (tasks.length > 1) {
        const t = makeTable([''].concat(tasks.map(String)), 'auto');
        matrix.slice(0, tasks.length).forEach(function (row, i) {
          const tr = el('tr');
          tr.appendChild(cell(tasks[i]));
          (Array.isArray(row) ? row : []).slice(0, tasks.length).forEach(function (n) {
            tr.appendChild(cell(num(n)));
          });
          t.appendChild(tr);
        });
        parts.push(scrollBox(t));
      }
      parts.push(el('p', 'notice small', 'Picking a winner is a merge: run fleet pick ' + clean(g.id, { max: 40 }) + ' <task> on the command line. It asks for the normal merge approval.'));
      holder.replaceChildren.apply(holder, parts);
    }

    async function load() {
      if (busy || !active) {
        return;
      }
      busy = true;
      try {
        const res = await api('GET', '/api/group/compare?id=' + enc(id));
        if (!active) {
          return;
        }
        if (res.status === 401) {
          showLoggedOut(NOT_LOGGED_IN);
          return;
        }
        if (!res.ok || !res.data) {
          sig = null;
          holder.replaceChildren(el('p', 'notice', 'Could not load the group: ' + errText(res)));
          return;
        }
        const s = JSON.stringify(res.data);
        if (s !== sig) {
          sig = s;
          paint(res.data);
        }
      } finally {
        busy = false;
      }
    }

    return {
      node: root,
      start: load,
      update: load,
      stop: function () {
        active = false;
      }
    };
  }

  // ---------------------------------------------------------------- start

  async function start() {
    const lo = document.getElementById('logout');
    if (lo) {
      wireAction(lo, 'logout', 'Log out', 'static', async function () {
        const res = await post('/api/logout', {});
        if (res.ok) {
          showLoggedOut('Logged out. Open the login URL printed by `fleet daemon --web` to log in again.');
        } else if (res.status !== 401) {
          setLine('Log out failed: ' + errText(res));
        }
      });
    }
    const frag = String(location.hash || '');
    const m = /(?:^|[#&?])token=([0-9a-f]{64})(?![0-9a-f])/.exec(frag);
    const nx = /(?:^|[#&?])next=([^&]*)/.exec(frag);
    // A route to open after the login. Plain path characters only, never a protocol-relative form.
    const next = nx && /^\/[A-Za-z0-9._~\/-]{0,200}$/.test(nx[1]) && nx[1].slice(0, 2) !== '//' ? nx[1] : '/runs';
    let res = null;
    let authed = false;
    if (m) {
      clearSession();
      res = await api('POST', '/api/login', { token: m[1] });
      const ok = res.ok && res.data && validId(res.data.session) && validId(res.data.csrf);
      // The token never stays in the address bar; a successful login lands on the requested route.
      history.replaceState(null, '', ok ? location.pathname + '#' + next : location.pathname);
      if (ok) {
        session = res.data.session;
        csrf = res.data.csrf;
        saveSession();
        authed = true;
      }
    } else if (loadSession()) {
      res = await api('GET', '/api/session');
      if (res.ok && res.data && validId(res.data.csrf)) {
        csrf = res.data.csrf;
        saveSession();
        authed = true;
      } else if (res.status === 401) {
        clearSession();
      }
    }
    if (!authed) {
      loggedOut = true;
      let text;
      if (res === null || (res.status === 401 && !m)) {
        text = NOT_LOGGED_IN;
      } else if (res.status === 429) {
        text = errText(res);
      } else if (m) {
        text = 'Login failed: ' + errText(res) + ' Open the login URL printed by `fleet daemon --web` in its terminal.';
      } else {
        text = 'Could not start a session: ' + errText(res);
      }
      viewEl().replaceChildren(el('p', 'notice', text));
      setLine('Not logged in.');
      return;
    }
    window.addEventListener('hashchange', route);
    document.addEventListener('visibilitychange', onVisibility);
    setInterval(tickCountdowns, 1000);
    route();
  }

  start();
})();
