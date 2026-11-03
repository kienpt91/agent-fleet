'use strict';
/*
 * fleet workbench: the dock (tabs and splits) and the page that hosts the panes.
 *
 * Text from the daemon is shown through textContent only (window.fleetHost.el and friends clean it
 * first). The layout is kept in the session storage of the tab under one key, as JSON that is checked field by field
 * when it is read back: anything that does not match is ignored and a preset is used instead.
 * Panes are registered by kind (see registerPane); a pane that cannot be created shows an error box.
 */
(function () {
  const LAYOUT_KEY = 'fleet.layout';
  const MAX_PANES = 12;
  const MAX_GROUPS = 12;
  const MAX_DEPTH = 6; // nested splits
  const MAX_LAYOUT_BYTES = 16384;
  const MAX_TITLE = 40;
  const MAX_PATH = 1024;
  const MAX_QUERY = 2000;
  const MAX_LIST = 200;
  const RATIO_MIN = 0.1;
  const RATIO_MAX = 0.9;
  const RATIO_STEP = 0.05;
  const ID_OK = /^[A-Za-z0-9._-]{1,40}$/;
  const REF_OK = /^[A-Za-z0-9._-]{1,40}\/[A-Za-z0-9._-]{1,40}$/;
  const SHA_OK = /^[0-9a-f]{40}$/;
  const KIND_OK = /^[a-z][a-z0-9_-]{0,23}$/;
  const CTRL = /[\u0000-\u001f\u007f-\u009f\u00ad\u061c\u200b\u200e\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]/;
  const VERIFICATION = new Set(['none', 'pending', 'unverified', 'failed', 'verified']);
  const OPEN_KINDS = new Set(['explorer', 'file', 'diff', 'markdown', 'image', 'task', 'runs', 'goals', 'decisions']);
  const PRESETS = ['default', 'review', 'single'];
  const QUERY_KEYS = new Set(['open', 'path', 'commit', 'mode', 'layout', 'line']);
  const TOOLBAR = [['explorer', 'Explorer'], ['diff', 'Diff'], ['task', 'Task'], ['run', 'Run'], ['goals', 'Goals'], ['decisions', 'Decisions']];
  const VERIF_LABEL = {
    none: 'UNVERIFIED',
    unverified: 'UNVERIFIED',
    failed: 'verification failed',
    pending: 'verification pending',
    verified: 'verified'
  };

  const specs = new Map();

  // ------------------------------------------------------------ validation

  function isObj(v) {
    return Object.prototype.toString.call(v) === '[object Object]';
  }

  function onlyKeys(o, allowed) {
    return Object.keys(o).every(function (k) {
      return allowed.indexOf(k) >= 0;
    });
  }

  function textOk(v, max) {
    return typeof v === 'string' && v.length <= max && !CTRL.test(v);
  }

  function revOk(v) {
    return v === 'head' || v === 'base' || (typeof v === 'string' && SHA_OK.test(v));
  }

  function lineOk(v) {
    return typeof v === 'number' && Number.isInteger(v) && v >= 1 && v <= 10000000;
  }

  function byteLength(s) {
    return typeof TextEncoder === 'function' ? new TextEncoder().encode(s).length : s.length * 3;
  }

  function safeDecode(s) {
    try {
      return decodeURIComponent(s);
    } catch (e) {
      return null;
    }
  }

  function message(e) {
    return e && typeof e.message === 'string' && e.message !== '' ? e.message : 'unknown error';
  }

  // The parameters every pane kind may carry. Strict (a stored layout): anything else makes the
  // whole layout invalid. Not strict (a pane opening another): unknown keys and bad values are dropped.
  // The task is always the one of this workbench.
  function cleanParams(raw, strict) {
    if (!isObj(raw)) {
      return null;
    }
    const out = {};
    const keys = Object.keys(raw);
    for (let i = 0; i < keys.length; i++) {
      const k = keys[i];
      let v = raw[k];
      let good;
      if (k === 'task') {
        good = typeof v === 'string' && REF_OK.test(v);
      } else if (k === 'commit' || k === 'base' || k === 'head') {
        good = revOk(v);
      } else if (k === 'path' || k === 'file') {
        good = textOk(v, MAX_PATH);
      } else if (k === 'mode') {
        good = v === 'unified' || v === 'split';
      } else if (k === 'line') {
        if (typeof v === 'string' && /^[0-9]{1,7}$/.test(v)) {
          v = Number(v);
        }
        good = lineOk(v);
      } else if (k === 'context') {
        good = typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 25;
      } else {
        good = false;
      }
      if (good) {
        out[k] = v;
      } else if (strict) {
        return null;
      }
    }
    return out;
  }

  function sanitizeParams(kind, raw, strict, ref) {
    let out = cleanParams(raw === undefined ? {} : raw, strict);
    if (out === null) {
      return null;
    }
    const spec = specs.get(kind);
    if (spec && spec.validate) {
      let r = null;
      try {
        r = spec.validate(Object.assign({}, out));
      } catch (e) {
        r = null;
      }
      out = cleanParams(r, false);
      if (out === null) {
        return null;
      }
    }
    out.task = ref;
    return Object.freeze(out);
  }

  // Reads the stored layout. Returns {tree, active} (plain descriptions, parameters cleaned) or null.
  function parseLayout(text, ref) {
    if (typeof text !== 'string' || text === '' || byteLength(text) > MAX_LAYOUT_BYTES) {
      return null;
    }
    let o;
    try {
      o = JSON.parse(text);
    } catch (e) {
      return null;
    }
    if (!isObj(o) || !onlyKeys(o, ['v', 'tree', 'active']) || o.v !== 1 || !Number.isInteger(o.active) || o.active < 0) {
      return null;
    }
    const acc = { panes: 0, groups: 0, singles: new Set() };
    const tree = walkNode(o.tree, 0, acc, ref);
    if (tree === null || acc.panes < 1 || o.active >= acc.groups) {
      return null;
    }
    return { tree: tree, active: o.active };
  }

  function walkNode(n, depth, acc, ref) {
    if (!isObj(n)) {
      return null;
    }
    if (n.t === 'group') {
      if (!onlyKeys(n, ['t', 'tabs', 'sel']) || !Array.isArray(n.tabs) || n.tabs.length > MAX_PANES) {
        return null;
      }
      if (!Number.isInteger(n.sel) || n.sel < 0 || (n.tabs.length > 0 ? n.sel >= n.tabs.length : n.sel !== 0)) {
        return null;
      }
      acc.groups += 1;
      if (acc.groups > MAX_GROUPS) {
        return null;
      }
      const tabs = [];
      for (let i = 0; i < n.tabs.length; i++) {
        const t = n.tabs[i];
        if (!isObj(t) || !onlyKeys(t, ['kind', 'params']) || typeof t.kind !== 'string' || !specs.has(t.kind)) {
          return null;
        }
        const params = sanitizeParams(t.kind, t.params, true, ref);
        acc.panes += 1;
        if (params === null || acc.panes > MAX_PANES) {
          return null;
        }
        if (specs.get(t.kind).single) {
          if (acc.singles.has(t.kind)) {
            return null;
          }
          acc.singles.add(t.kind);
        }
        tabs.push({ kind: t.kind, params: params });
      }
      return { t: 'group', tabs: tabs, sel: n.sel };
    }
    if (n.t === 'split') {
      if (!onlyKeys(n, ['t', 'dir', 'ratio', 'a', 'b']) || (n.dir !== 'h' && n.dir !== 'v')) {
        return null;
      }
      if (typeof n.ratio !== 'number' || !Number.isFinite(n.ratio) || n.ratio < RATIO_MIN || n.ratio > RATIO_MAX) {
        return null;
      }
      if (depth + 1 > MAX_DEPTH) {
        return null;
      }
      const a = walkNode(n.a, depth + 1, acc, ref);
      const b = a === null ? null : walkNode(n.b, depth + 1, acc, ref);
      return a === null || b === null ? null : { t: 'split', dir: n.dir, ratio: n.ratio, a: a, b: b };
    }
    return null;
  }

  // ---------------------------------------------------------------- routes

  function parseQuery(text) {
    const out = {};
    if (text.length > MAX_QUERY) {
      return out;
    }
    text.split('&').forEach(function (pair) {
      const i = pair.indexOf('=');
      if (i < 1) {
        return;
      }
      const k = pair.slice(0, i);
      if (!QUERY_KEYS.has(k) || Object.prototype.hasOwnProperty.call(out, k)) {
        return;
      }
      const v = safeDecode(pair.slice(i + 1));
      if (v === null) {
        return;
      }
      let good = false;
      let val = v;
      if (k === 'open') {
        good = OPEN_KINDS.has(v);
      } else if (k === 'path') {
        good = textOk(v, MAX_PATH);
      } else if (k === 'commit') {
        good = revOk(v);
      } else if (k === 'mode') {
        good = v === 'unified' || v === 'split';
      } else if (k === 'layout') {
        good = PRESETS.indexOf(v) >= 0;
      } else if (k === 'line') {
        good = /^[0-9]{1,7}$/.test(v) && lineOk(Number(v));
        val = Number(v);
      }
      if (good) {
        out[k] = val;
      }
    });
    return out;
  }

  // '#/work' (pick a task) or '#/work/<run>/<task>[?query]'. Null for anything else.
  function parse(hash) {
    const body = String(hash === undefined || hash === null ? '' : hash).replace(/^#\/?/, '');
    const qi = body.indexOf('?');
    const segs = (qi < 0 ? body : body.slice(0, qi)).split('/');
    if (segs[0] !== 'work') {
      return null;
    }
    const query = qi < 0 ? {} : parseQuery(body.slice(qi + 1));
    if (segs.length === 1 || (segs.length === 2 && segs[1] === '')) {
      return { run: null, task: null, ref: null, query: {}, hasQuery: qi >= 0 };
    }
    if (segs.length !== 3) {
      return null;
    }
    const run = safeDecode(segs[1]);
    const task = safeDecode(segs[2]);
    if (run === null || task === null || !ID_OK.test(run) || !ID_OK.test(task)) {
      return null;
    }
    return { run: run, task: task, ref: run + '/' + task, query: query, hasQuery: qi >= 0 };
  }

  // ----------------------------------------------------------------- panes

  function registerPane(kind, spec) {
    if (typeof kind !== 'string' || !KIND_OK.test(kind)) {
      throw new TypeError('registerPane: invalid kind');
    }
    if (specs.has(kind)) {
      throw new Error('registerPane: kind already registered: ' + kind);
    }
    if (spec === null || typeof spec !== 'object' || typeof spec.create !== 'function' || typeof spec.title !== 'function') {
      throw new TypeError('registerPane: a spec needs title() and create()');
    }
    specs.set(kind, Object.freeze({
      title: spec.title,
      create: spec.create,
      validate: typeof spec.validate === 'function' ? spec.validate : null,
      single: spec.single === true
    }));
  }

  function runOf(p) {
    return p && typeof p.task === 'string' && REF_OK.test(p.task) ? p.task.split('/')[0] : '';
  }

  function taskOf(p) {
    return p && typeof p.task === 'string' && REF_OK.test(p.task) ? p.task.split('/')[1] : '';
  }

  registerPane('runs', {
    title: function () {
      return 'Runs';
    },
    create: function (ctx) {
      return ctx.host.views.runs();
    }
  });
  registerPane('run', {
    title: function (p) {
      return 'Run ' + runOf(p);
    },
    create: function (ctx, p) {
      return ctx.host.views.run(runOf(p));
    }
  });
  registerPane('task', {
    title: function (p) {
      return 'Task ' + taskOf(p);
    },
    create: function (ctx, p) {
      return ctx.host.views.task(runOf(p), taskOf(p));
    }
  });
  registerPane('goals', {
    title: function () {
      return 'Goals';
    },
    create: function (ctx) {
      return ctx.host.views.goals();
    }
  });
  // the decisions page keeps page-wide state (countdowns, confirm buttons): one instance only
  registerPane('decisions', {
    single: true,
    title: function () {
      return 'Decisions';
    },
    create: function (ctx) {
      return ctx.host.views.decisions();
    }
  });

  // --------------------------------------------------------------- presets

  function group(tabs) {
    return { t: 'group', tabs: tabs, sel: 0 };
  }

  function split(dir, ratio, a, b) {
    return { t: 'split', dir: dir, ratio: ratio, a: a, b: b };
  }

  function pane(kind, params) {
    return { kind: kind, params: params };
  }

  function preset(name) {
    const explorer = function () {
      return pane('explorer', { commit: 'head', path: '' });
    };
    const file = function () {
      return pane('file', { commit: 'head' });
    };
    const diff = function () {
      return pane('diff', { base: 'base', head: 'head' });
    };
    if (name === 'review') {
      return split('h', 0.3, group([explorer()]), split('v', 0.5, group([file()]), group([diff()])));
    }
    if (name === 'single') {
      return group([explorer(), file(), diff()]);
    }
    return split('h', 0.3, group([explorer()]), group([file(), diff()]));
  }

  // ------------------------------------------------------------------ list

  function listView(H) {
    const root = H.el('section', 'page workbench');
    root.appendChild(H.el('h2', '', 'Workbench'));
    root.appendChild(H.el('p', 'muted', 'Pick a task to review its files and changes. Only tasks with a recorded commit are listed. The files shown are the committed state of the repository, not the worktree.'));
    const box = H.el('div', 'worklist');
    root.appendChild(box);
    let sig = null;
    return {
      node: root,
      update: function (d) {
        const rows = [];
        (Array.isArray(d.runs) ? d.runs : []).forEach(function (r) {
          (r && Array.isArray(r.tasks) ? r.tasks : []).forEach(function (t) {
            if (t && typeof t.head === 'string' && t.head !== '') {
              rows.push({ ref: String(r.id) + '/' + String(t.id), title: t.title, status: t.status, head: t.head });
            }
          });
        });
        const s = JSON.stringify(rows);
        if (s === sig) {
          return;
        }
        sig = s;
        if (!rows.length) {
          box.replaceChildren(H.el('p', 'muted', 'No task has a recorded commit yet.'));
          return;
        }
        const list = H.el('ul', 'worktasks');
        rows.slice(0, MAX_LIST).forEach(function (row) {
          const li = H.el('li', 'worktask');
          if (REF_OK.test(row.ref)) {
            const a = H.el('a', 'rlink', row.ref);
            a.setAttribute('href', '#/work/' + row.ref);
            li.appendChild(a);
          } else {
            li.appendChild(H.el('span', 'mono-text', row.ref));
          }
          li.appendChild(H.badge(row.status));
          li.appendChild(H.el('span', 'muted', H.short(row.title, 80)));
          li.appendChild(H.el('code', 'mono-text', H.shortHash(row.head)));
          list.appendChild(li);
        });
        const out = [list];
        if (rows.length > MAX_LIST) {
          out.push(H.el('p', 'muted small', (rows.length - MAX_LIST) + ' more tasks are not shown.'));
        }
        box.replaceChildren.apply(box, out);
      }
    };
  }

  // ------------------------------------------------------------------ dock

  function dockView(H, route) {
    const ref = route.ref;
    const panes = [];
    let root = null;
    let activeGroup = null;
    let seq = 0;
    let started = false;
    let stopped = false;
    let lastStatus = null;
    let vSeq = 0;
    let headSig = null;

    const page = H.el('section', 'page workbench');
    const titlebar = H.el('div', 'titlebar');
    const state = H.el('div', 'workstate');
    const bar = H.el('div', 'dock-bar');
    const say = H.el('p', 'dock-say muted small', '');
    say.setAttribute('role', 'status');
    const host = H.el('div', 'dock');
    host.setAttribute('role', 'group');
    host.setAttribute('aria-label', 'Workbench panes');

    titlebar.appendChild(H.el('h2', '', 'Workbench ' + ref));
    const back = H.el('a', 'rlink', 'Task page');
    back.setAttribute('href', '#/task/' + ref);
    titlebar.appendChild(back);
    page.appendChild(titlebar);
    page.appendChild(state);
    page.appendChild(H.el('p', 'muted small', 'The files shown are the committed state of the repository (the task\u2019s recorded commit), not the worktree.'));
    page.appendChild(bar);
    page.appendChild(host);
    page.appendChild(H.el('p', 'muted small', 'Tabs: Left and Right move, Home and End jump, Delete closes, M moves the tab to the next group. Splitters: arrow keys resize by 5%, Home and End go to the limits.'));

    function note(text) {
      say.textContent = H.clean(text);
    }

    function prevent(ev) {
      if (ev && typeof ev.preventDefault === 'function') {
        ev.preventDefault();
      }
    }

    function button(label, cls, fn, aria, title) {
      const b = H.el('button', 'btn small ' + cls, label);
      b.setAttribute('type', 'button');
      if (aria) {
        b.setAttribute('aria-label', aria);
      }
      if (title) {
        b.setAttribute('title', title);
      }
      b.addEventListener('click', fn);
      return b;
    }

    // ------------------------------------------------------ tree model

    function makeGroup() {
      return { t: 'group', tabs: [], sel: null, parent: null, el: null, tools: null };
    }

    function groupsOf() {
      const out = [];
      (function walk(n) {
        if (!n) {
          return;
        }
        if (n.t === 'group') {
          out.push(n);
        } else {
          walk(n.a);
          walk(n.b);
        }
      })(root);
      return out;
    }

    function ancestors(n) {
      let d = 0;
      for (let p = n.parent; p; p = p.parent) {
        d += 1;
      }
      return d;
    }

    function firstGroup(n) {
      let c = n;
      while (c && c.t === 'split') {
        c = c.a;
      }
      return c;
    }

    function replaceNode(old, neu) {
      const p = old.parent;
      if (p === null) {
        root = neu;
        neu.parent = null;
      } else {
        if (p.a === old) {
          p.a = neu;
        } else {
          p.b = neu;
        }
        neu.parent = p;
      }
    }

    // an empty group leaves the tree and its sibling takes the place of the split
    function collapse(g) {
      const p = g.parent;
      if (p === null) {
        return;
      }
      const sib = p.a === g ? p.b : p.a;
      replaceNode(p, sib);
      if (activeGroup === g) {
        activeGroup = firstGroup(sib);
      }
    }

    function build(desc) {
      if (desc.t === 'group') {
        const g = makeGroup();
        desc.tabs.forEach(function (t) {
          const rec = makeRec(t.kind, Object.freeze(Object.assign({}, t.params, { task: ref })));
          rec.group = g;
          g.tabs.push(rec);
          panes.push(rec);
        });
        g.sel = g.tabs[desc.sel] || null;
        return g;
      }
      const a = build(desc.a);
      const b = build(desc.b);
      const s = { t: 'split', dir: desc.dir, ratio: desc.ratio, a: a, b: b, parent: null, el: null, splitter: null };
      a.parent = s;
      b.parent = s;
      return s;
    }

    function describe(n) {
      if (n.t === 'group') {
        return {
          t: 'group',
          tabs: n.tabs.map(function (r) {
            const p = Object.assign({}, r.params);
            delete p.task;
            return { kind: r.kind, params: p };
          }),
          sel: Math.max(0, n.tabs.indexOf(n.sel))
        };
      }
      return { t: 'split', dir: n.dir, ratio: n.ratio, a: describe(n.a), b: describe(n.b) };
    }

    function save() {
      if (stopped || root === null) {
        return;
      }
      try {
        const text = JSON.stringify({ v: 1, tree: describe(root), active: Math.max(0, groupsOf().indexOf(activeGroup)) });
        if (byteLength(text) <= MAX_LAYOUT_BYTES) {
          sessionStorage.setItem(LAYOUT_KEY, text);
        }
      } catch (e) {
        note('The layout could not be kept for a reload.');
      }
    }

    function load() {
      let text = null;
      try {
        text = sessionStorage.getItem(LAYOUT_KEY);
      } catch (e) {
        text = null;
      }
      return text === null ? null : parseLayout(text, ref);
    }

    // -------------------------------------------------------- instances

    function errorInst(text) {
      const box = H.el('div', 'msg err dock-error', text, true);
      box.setAttribute('role', 'alert');
      return { node: box };
    }

    function computeTitle(rec) {
      let t = rec.kind;
      const spec = specs.get(rec.kind);
      if (spec) {
        try {
          const r = spec.title(rec.params);
          if (typeof r === 'string' && r !== '') {
            t = r;
          }
        } catch (e) {
          t = rec.kind;
        }
      }
      rec.title = H.short(t, MAX_TITLE);
    }

    function makeCtx(rec) {
      return Object.freeze({
        host: H,
        params: rec.params,
        open: function (kind, params, opts) {
          return openPane(kind, params, opts, rec);
        },
        setTitle: function (text) {
          rec.title = H.short(text === undefined || text === null || text === '' ? rec.kind : text, MAX_TITLE);
          paintTitle(rec);
          save();
        },
        close: function () {
          closePane(rec, false);
        },
        isActive: function () {
          return !rec.dead && rec.group !== null && rec.group === activeGroup && rec.group.sel === rec;
        }
      });
    }

    function makeRec(kind, params) {
      seq += 1;
      const rec = {
        id: seq,
        kind: kind,
        params: params,
        title: kind,
        inst: null,
        group: null,
        tab: null,
        closeBtn: null,
        panel: H.el('div', 'dock-panel'),
        started: false,
        dead: false,
        failed: false
      };
      rec.panel.setAttribute('id', 'dock-panel-' + rec.id);
      rec.panel.setAttribute('role', 'tabpanel');
      const spec = specs.get(kind);
      let inst = null;
      if (!spec) {
        inst = errorInst('Unknown pane kind: ' + kind);
        rec.failed = true;
      } else {
        try {
          inst = spec.create(makeCtx(rec), params);
          if (!inst || typeof inst !== 'object' || !inst.node || inst.node.nodeType !== 1) {
            throw new Error('the pane returned no node');
          }
        } catch (e) {
          inst = errorInst('The ' + kind + ' pane could not be created: ' + message(e));
          rec.failed = true;
        }
      }
      rec.inst = inst;
      computeTitle(rec);
      rec.panel.appendChild(inst.node);
      return rec;
    }

    function failPane(rec, what, e) {
      rec.failed = true;
      rec.panel.replaceChildren(errorInst('The ' + rec.kind + ' pane failed (' + what + '): ' + message(e)).node);
    }

    function callInst(rec, name, arg) {
      const f = rec.inst ? rec.inst[name] : null;
      if (rec.failed || typeof f !== 'function') {
        return;
      }
      try {
        f.call(rec.inst, arg);
      } catch (e) {
        if (name === 'stop') {
          rec.failed = true;
        } else {
          failPane(rec, name, e);
        }
      }
    }

    function startRec(rec) {
      if (started && !rec.started && !rec.dead) {
        rec.started = true;
        callInst(rec, 'start');
      }
    }

    function paintTitle(rec) {
      if (rec.tab) {
        rec.tab.textContent = rec.title;
      }
      if (rec.closeBtn) {
        rec.closeBtn.setAttribute('aria-label', 'Close ' + rec.title);
      }
    }

    // ---------------------------------------------------------- render

    function pct(r) {
      return String(Math.round(r * 1000) / 10) + '%';
    }

    function setRatio(s, r, persist) {
      const v = Math.round(Math.min(RATIO_MAX, Math.max(RATIO_MIN, r)) * 1000) / 1000;
      s.ratio = v;
      if (s.a.el) {
        s.a.el.style.flexBasis = pct(v);
      }
      if (s.splitter) {
        s.splitter.setAttribute('aria-valuenow', String(Math.round(v * 100)));
      }
      if (persist) {
        save();
      }
    }

    function splitterOf(s) {
      const sp = H.el('div', 'dock-splitter ' + (s.dir === 'h' ? 'dock-vbar' : 'dock-hbar'));
      sp.setAttribute('role', 'separator');
      sp.setAttribute('tabindex', '0');
      sp.setAttribute('aria-orientation', s.dir === 'h' ? 'vertical' : 'horizontal');
      sp.setAttribute('aria-label', 'Resize panes');
      sp.setAttribute('aria-valuemin', String(Math.round(RATIO_MIN * 100)));
      sp.setAttribute('aria-valuemax', String(Math.round(RATIO_MAX * 100)));
      sp.setAttribute('aria-valuenow', String(Math.round(s.ratio * 100)));
      s.splitter = sp;
      sp.addEventListener('keydown', function (ev) {
        if (ev.ctrlKey || ev.altKey || ev.metaKey) {
          return;
        }
        let r = null;
        if (ev.key === 'ArrowLeft' || ev.key === 'ArrowUp') {
          r = s.ratio - RATIO_STEP;
        } else if (ev.key === 'ArrowRight' || ev.key === 'ArrowDown') {
          r = s.ratio + RATIO_STEP;
        } else if (ev.key === 'Home') {
          r = RATIO_MIN;
        } else if (ev.key === 'End') {
          r = RATIO_MAX;
        }
        if (r !== null) {
          prevent(ev);
          setRatio(s, r, true);
        }
      });
      let dragging = false;
      sp.addEventListener('pointerdown', function (ev) {
        if (ev.button !== undefined && ev.button !== 0) {
          return;
        }
        dragging = true;
        sp.classList.add('dragging');
        if (typeof sp.setPointerCapture === 'function' && ev.pointerId !== undefined) {
          try {
            sp.setPointerCapture(ev.pointerId);
          } catch (e) {
            dragging = true;
          }
        }
        prevent(ev);
      });
      sp.addEventListener('pointermove', function (ev) {
        if (!dragging || !s.el || typeof s.el.getBoundingClientRect !== 'function') {
          return;
        }
        const rect = s.el.getBoundingClientRect();
        const size = s.dir === 'h' ? rect.width : rect.height;
        const pos = s.dir === 'h' ? ev.clientX - rect.left : ev.clientY - rect.top;
        if (typeof size === 'number' && size > 0 && Number.isFinite(pos)) {
          setRatio(s, pos / size, false);
        }
      });
      const end = function () {
        if (dragging) {
          dragging = false;
          sp.classList.remove('dragging');
          save();
        }
      };
      sp.addEventListener('pointerup', end);
      sp.addEventListener('pointercancel', end);
      return sp;
    }

    function applySelection(g) {
      g.tabs.forEach(function (r) {
        const on = g.sel === r;
        if (r.tab) {
          r.tab.setAttribute('aria-selected', on ? 'true' : 'false');
          r.tab.setAttribute('tabindex', on ? '0' : '-1');
          r.tab.classList.toggle('selected', on);
        }
        r.panel.classList.toggle('hidden', !on);
      });
    }

    function setActive(g) {
      activeGroup = g;
      groupsOf().forEach(function (x) {
        if (x.el) {
          x.el.classList.toggle('active', x === g);
        }
      });
    }

    function activate(g) {
      if (activeGroup !== g) {
        setActive(g);
        save();
      }
    }

    function focusTab(rec) {
      if (rec && rec.tab && typeof rec.tab.focus === 'function') {
        rec.tab.focus();
      }
    }

    function onTabKey(rec, ev) {
      if (ev.ctrlKey || ev.altKey || ev.metaKey) {
        return;
      }
      const g = rec.group;
      const i = g.tabs.indexOf(rec);
      const n = g.tabs.length;
      let to = null;
      if (ev.key === 'ArrowRight') {
        to = g.tabs[(i + 1) % n];
      } else if (ev.key === 'ArrowLeft') {
        to = g.tabs[(i + n - 1) % n];
      } else if (ev.key === 'Home') {
        to = g.tabs[0];
      } else if (ev.key === 'End') {
        to = g.tabs[n - 1];
      } else if (ev.key === 'Delete') {
        prevent(ev);
        closePane(rec, true);
        return;
      } else if (ev.key === 'm' || ev.key === 'M') {
        prevent(ev);
        movePane(rec);
        return;
      }
      if (to) {
        prevent(ev);
        selectPane(to, true);
      }
    }

    function renderGroup(g) {
      const ge = H.el('div', 'dock-group');
      const head = H.el('div', 'dock-head');
      const tabs = H.el('div', 'dock-tabs');
      tabs.setAttribute('role', 'tablist');
      tabs.setAttribute('aria-label', 'Open panes');
      const box = H.el('div', 'dock-panels');
      g.tabs.forEach(function (rec) {
        const wrap = H.el('div', 'dock-tabwrap');
        wrap.setAttribute('role', 'presentation');
        const t = H.el('button', 'dock-tab', rec.title);
        t.setAttribute('type', 'button');
        t.setAttribute('role', 'tab');
        t.setAttribute('id', 'dock-tab-' + rec.id);
        t.setAttribute('aria-controls', 'dock-panel-' + rec.id);
        const x = H.el('button', 'dock-tabclose', '\u00d7');
        x.setAttribute('type', 'button');
        x.setAttribute('tabindex', '-1');
        rec.tab = t;
        rec.closeBtn = x;
        rec.panel.setAttribute('aria-labelledby', 'dock-tab-' + rec.id);
        t.addEventListener('click', function () {
          selectPane(rec, false);
        });
        t.addEventListener('keydown', function (ev) {
          onTabKey(rec, ev);
        });
        t.addEventListener('auxclick', function (ev) {
          if (ev.button === 1) {
            prevent(ev);
            closePane(rec, false);
          }
        });
        x.addEventListener('click', function () {
          closePane(rec, true);
        });
        wrap.appendChild(t);
        wrap.appendChild(x);
        tabs.appendChild(wrap);
        paintTitle(rec);
        box.appendChild(rec.panel);
      });
      if (!g.tabs.length) {
        const empty = H.el('div', 'dock-empty');
        empty.appendChild(H.el('p', 'muted', 'This group is empty. Use the buttons above to open a pane here, or move a tab into it.'));
        if (g.parent) {
          empty.appendChild(button('Remove empty group', 'removegroup', function () {
            collapse(g);
            if (groupsOf().indexOf(activeGroup) < 0) {
              activeGroup = firstGroup(root);
            }
            render();
            save();
          }));
        }
        box.appendChild(empty);
      }
      const tools = H.el('div', 'dock-tools');
      const bRight = button('Split right', '', function () {
        splitAction(g, 'h');
      }, null, 'Split right: puts the current tab into a new group on the right when the group has several tabs');
      const bBelow = button('Split below', '', function () {
        splitAction(g, 'v');
      }, null, 'Split below: puts the current tab into a new group below when the group has several tabs');
      const bMove = button('Move tab', '', function () {
        if (g.sel) {
          movePane(g.sel);
        }
      }, 'Move the current tab to the next group', 'Move the current tab to the next group');
      bMove.disabled = groupsOf().length < 2 || !g.sel;
      tools.appendChild(bRight);
      tools.appendChild(bBelow);
      tools.appendChild(bMove);
      g.tools = bRight;
      head.appendChild(tabs);
      head.appendChild(tools);
      ge.appendChild(head);
      ge.appendChild(box);
      ge.addEventListener('focusin', function () {
        activate(g);
      });
      ge.addEventListener('pointerdown', function () {
        activate(g);
      });
      g.el = ge;
      applySelection(g);
      return ge;
    }

    function renderNode(n) {
      if (n.t === 'group') {
        return renderGroup(n);
      }
      const se = H.el('div', 'dock-split ' + (n.dir === 'h' ? 'dock-h' : 'dock-v'));
      n.el = se;
      const ae = renderNode(n.a);
      const be = renderNode(n.b);
      se.appendChild(ae);
      se.appendChild(splitterOf(n));
      se.appendChild(be);
      ae.classList.add('dock-first');
      be.classList.add('dock-second');
      setRatio(n, n.ratio, false);
      return se;
    }

    function render() {
      if (stopped || root === null) {
        return;
      }
      host.replaceChildren(renderNode(root));
      setActive(activeGroup);
    }

    // ------------------------------------------------------- operations

    function selectPane(rec, focus) {
      const g = rec.group;
      if (!g || rec.dead) {
        return;
      }
      g.sel = rec;
      applySelection(g);
      setActive(g);
      if (focus) {
        focusTab(rec);
      }
      save();
    }

    function detach(rec) {
      const g = rec.group;
      const i = g.tabs.indexOf(rec);
      g.tabs.splice(i, 1);
      rec.group = null;
      if (g.sel === rec) {
        g.sel = g.tabs[Math.min(i, g.tabs.length - 1)] || null;
      }
      return g;
    }

    function closePane(rec, focus) {
      if (rec.dead || stopped) {
        return;
      }
      rec.dead = true;
      callInst(rec, 'stop');
      const g = detach(rec);
      panes.splice(panes.indexOf(rec), 1);
      if (!g.tabs.length) {
        collapse(g);
      }
      if (activeGroup === g || groupsOf().indexOf(activeGroup) < 0) {
        activeGroup = groupsOf().indexOf(g) >= 0 ? g : firstGroup(root);
      }
      render();
      if (focus) {
        const ag = activeGroup;
        if (ag && ag.sel) {
          focusTab(ag.sel);
        } else if (ag && ag.tools && typeof ag.tools.focus === 'function') {
          ag.tools.focus();
        }
      }
      save();
    }

    function movePane(rec) {
      const gs = groupsOf();
      if (gs.length < 2 || rec.dead || !rec.group) {
        note('There is only one group. Split the view first.');
        return;
      }
      const from = rec.group;
      const to = gs[(gs.indexOf(from) + 1) % gs.length];
      detach(rec);
      to.tabs.push(rec);
      rec.group = to;
      to.sel = rec;
      if (!from.tabs.length) {
        collapse(from);
      }
      activeGroup = to;
      render();
      focusTab(rec);
      save();
    }

    function splitGroup(g, dir) {
      if (groupsOf().length >= MAX_GROUPS || ancestors(g) >= MAX_DEPTH) {
        return null;
      }
      const ng = makeGroup();
      const s = { t: 'split', dir: dir, ratio: 0.5, a: g, b: ng, parent: null, el: null, splitter: null };
      replaceNode(g, s);
      g.parent = s;
      ng.parent = s;
      return ng;
    }

    function splitAction(g, dir) {
      const ng = splitGroup(g, dir);
      if (ng === null) {
        note('The view cannot be split any further.');
        return;
      }
      if (g.tabs.length > 1 && g.sel) {
        const rec = g.sel;
        detach(rec);
        ng.tabs.push(rec);
        rec.group = ng;
        ng.sel = rec;
      }
      activeGroup = ng;
      render();
      if (ng.sel) {
        focusTab(ng.sel);
      } else if (ng.tools && typeof ng.tools.focus === 'function') {
        ng.tools.focus();
      }
      save();
    }

    function otherGroup(g) {
      const gs = groupsOf();
      return gs.length > 1 ? gs[(gs.indexOf(g) + 1) % gs.length] : g;
    }

    // Opens a pane, or focuses the existing one of the kind in opts.reuse. Returns true when a pane
    // was opened or reused. Without opts.split the pane goes to the next group (or the same one when
    // there is only one), so an explorer opens files beside itself.
    function openPane(kind, rawParams, opts, from) {
      const o = isObj(opts) ? opts : {};
      if (stopped || typeof kind !== 'string' || !KIND_OK.test(kind)) {
        return false;
      }
      const params = sanitizeParams(kind, rawParams, false, ref);
      if (params === null) {
        note('The pane could not be opened: its parameters are not valid.');
        return false;
      }
      const spec = specs.get(kind);
      const reuse = typeof o.reuse === 'string' ? o.reuse : spec && spec.single ? kind : null;
      if (reuse !== null) {
        const hit = panes.find(function (p) {
          return p.kind === reuse && !p.dead;
        });
        if (hit) {
          if (hit.inst && typeof hit.inst.setParams === 'function') {
            hit.params = params;
            callInst(hit, 'setParams', params);
            computeTitle(hit);
            paintTitle(hit);
            selectPane(hit, false);
            return true;
          }
          // a pane that cannot take new parameters is replaced
          closePane(hit, false);
        }
      }
      if (panes.length >= MAX_PANES) {
        note('At most ' + MAX_PANES + ' panes can be open. Close one first.');
        return false;
      }
      const base = from && from.group ? from.group : activeGroup;
      let target;
      if (o.split === 'right' || o.split === 'below') {
        target = splitGroup(base, o.split === 'right' ? 'h' : 'v') || base;
      } else {
        target = otherGroup(base);
      }
      const rec = makeRec(kind, params);
      rec.group = target;
      target.tabs.push(rec);
      target.sel = rec;
      panes.push(rec);
      activeGroup = target;
      render();
      startRec(rec);
      if (lastStatus) {
        callInst(rec, 'update', lastStatus);
      }
      save();
      return true;
    }

    function closeAll() {
      panes.splice(0).forEach(function (rec) {
        rec.dead = true;
        callInst(rec, 'stop');
      });
      root = null;
      activeGroup = null;
    }

    function initial(desc, active) {
      root = build(desc);
      const gs = groupsOf();
      activeGroup = gs[active] || gs[0];
    }

    function applyPreset(name) {
      closeAll();
      initial(preset(name), 0);
      render();
      panes.slice().forEach(function (rec) {
        startRec(rec);
        if (lastStatus) {
          callInst(rec, 'update', lastStatus);
        }
      });
      save();
    }

    // ---------------------------------------------------- deep links

    function deepParams(kind, q) {
      const p = { task: ref };
      if (kind === 'diff') {
        p.base = 'base';
        p.head = q.commit || 'head';
        if (q.path !== undefined) {
          p.file = q.path;
        }
        if (q.mode) {
          p.mode = q.mode;
        }
      } else if (kind === 'explorer' || kind === 'file' || kind === 'markdown' || kind === 'image') {
        if (q.commit) {
          p.commit = q.commit;
        }
        if (q.path !== undefined) {
          p.path = q.path;
        }
        if (kind === 'file' && q.line) {
          p.line = q.line;
        }
      }
      return p;
    }

    function applyDeepLink() {
      const q = route.query || {};
      if (q.open) {
        openPane(q.open, deepParams(q.open, q), { reuse: q.open }, null);
      }
      if (route.hasQuery && typeof history === 'object' && history !== null && typeof history.replaceState === 'function') {
        // the address keeps the task but not the one-shot request; replaceState does not run the router again
        history.replaceState(null, '', location.pathname + '#/work/' + H.enc(route.run) + '/' + H.enc(route.task));
      }
    }

    // ----------------------------------------------------------- header

    function paintHeader(info, v) {
      const sig = JSON.stringify([info ? [info.task.status, info.task.head] : null, v.state, v.dirty]);
      if (sig === headSig) {
        return;
      }
      headSig = sig;
      const row = [];
      if (!info) {
        row.push(H.el('span', 'muted', 'This task is not in the latest status.'));
      } else {
        row.push(H.badge(info.task.status));
        const cls = v.state === 'none' ? 'unverified' : v.state;
        row.push(H.tagged('vf', VERIFICATION, cls, VERIF_LABEL[cls] || 'UNVERIFIED'));
        if (v.dirty === true) {
          const d = H.el('span', 'badge dirty', 'dirty');
          d.setAttribute('title', 'The newest evidence for this commit was produced from a dirty worktree.');
          row.push(d);
        }
        if (typeof info.task.head === 'string' && info.task.head !== '') {
          row.push(H.el('span', 'muted small', 'commit'));
          row.push(H.el('code', 'mono-text', H.shortHash(info.task.head)));
        } else {
          row.push(H.el('span', 'notice small', 'This task has no recorded commit yet, so there is nothing to show.'));
        }
      }
      state.replaceChildren.apply(state, row);
    }

    function refreshHeader() {
      const info = H.taskInfo(ref);
      vSeq += 1;
      const mine = vSeq;
      let p;
      try {
        p = H.taskVerification(ref);
      } catch (e) {
        p = Promise.resolve({ state: 'none', dirty: null, commit: null });
      }
      p.then(function (v) {
        if (mine === vSeq && !stopped && v) {
          paintHeader(info, v);
        }
      }, function () {
        if (mine === vSeq && !stopped) {
          paintHeader(info, { state: 'none', dirty: null, commit: null });
        }
      });
    }

    // ---------------------------------------------------- toolbar

    TOOLBAR.forEach(function (it) {
      if (!specs.has(it[0])) {
        return;
      }
      bar.appendChild(button(it[1], '', function () {
        const kind = it[0];
        const p = { task: ref };
        if (kind === 'explorer') {
          p.commit = 'head';
          p.path = '';
        } else if (kind === 'diff') {
          p.base = 'base';
          p.head = 'head';
        }
        openPane(kind, p, { reuse: kind }, null);
      }, 'Open the ' + it[1] + ' pane'));
    });
    PRESETS.forEach(function (name) {
      bar.appendChild(button('Layout ' + name, 'preset', function () {
        applyPreset(name);
        note('Layout reset to ' + name + '.');
      }, 'Reset the layout to ' + name));
    });
    bar.appendChild(say);

    // ----------------------------------------------------- start up

    const q0 = route.query || {};
    const stored = q0.layout ? null : load();
    if (stored) {
      initial(stored.tree, stored.active);
    } else {
      initial(preset(q0.layout || 'default'), 0);
    }
    render();
    save();

    return {
      node: page,
      start: function () {
        started = true;
        panes.slice().forEach(startRec);
        applyDeepLink();
      },
      update: function (d) {
        lastStatus = d;
        refreshHeader();
        panes.slice().forEach(function (rec) {
          callInst(rec, 'update', d);
        });
      },
      pause: function () {
        panes.slice().forEach(function (rec) {
          callInst(rec, 'pause');
        });
      },
      resume: function () {
        panes.slice().forEach(function (rec) {
          callInst(rec, 'resume');
        });
      },
      stop: function () {
        if (stopped) {
          return;
        }
        panes.splice(0).forEach(function (rec) {
          rec.dead = true;
          callInst(rec, 'stop');
        });
        stopped = true;
      }
    };
  }

  function view(route) {
    const H = window.fleetHost;
    if (!H || !route || typeof route !== 'object') {
      return null;
    }
    if (typeof route.ref === 'string' && REF_OK.test(route.ref)) {
      return dockView(H, {
        run: route.run,
        task: route.task,
        ref: route.ref,
        query: isObj(route.query) ? route.query : {},
        hasQuery: route.hasQuery === true
      });
    }
    return listView(H);
  }

  window.fleetDock = Object.freeze({ registerPane: registerPane });
  window.fleetWorkbench = Object.freeze({ view: view, parse: parse });
})();
