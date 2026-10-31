// Smoke test of the dashboard script in crates/fleet-infra/web/app.js.
// No dependencies. It runs the real script in a vm context against a small fake DOM, a fake
// fetch and a fake EventSource. Run it from the repository root: node scripts/web-smoke.mjs
import fs from 'node:fs';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const read = (p) => fs.readFileSync(root + p, 'utf8');
const APP = read('crates/fleet-infra/web/app.js');
const HTML = read('crates/fleet-infra/web/index.html');
const CSS = read('crates/fleet-infra/web/app.css');

let checks = 0;
function check(cond, what) {
  checks += 1;
  if (!cond) {
    throw new Error('check failed: ' + what);
  }
}

const violations = [];

// ------------------------------------------------------------- hostile data

const IMG = '<img src=x onerror=alert(1)>';
const SCRIPT = '<script>alert(1)</script>';
const ESC = '\u001b[31mred\u001b[0m';
const BIDI = 'safe\u202egnp.exe';
const TOKEN = '0123456789abcdef'.repeat(4);
const DIGEST = 'ab'.repeat(32);
const DIGEST2 = 'cd'.repeat(32);

// ----------------------------------------------------------------- fake DOM

const FORBIDDEN_ATTR = /^(style|href|src|srcdoc|on.*)$/i;

class FakeText {
  constructor(data) {
    this.nodeType = 3;
    this.data = String(data);
    this.parentNode = null;
  }
  get textContent() {
    return this.data;
  }
  set textContent(v) {
    this.data = String(v);
  }
}

class FakeElement {
  constructor(env, tag, ns) {
    this.env = env;
    this.nodeType = 1;
    this.tagName = tag;
    this.namespaceURI = ns || null;
    this.childNodes = [];
    this.parentNode = null;
    this.attrs = new Map();
    this.listeners = new Map();
    this.disabled = false;
    this.value = '';
    this.checked = false;
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
    this.dataset = {};
    this.textWrites = 0;
  }
  get className() {
    return this.attrs.get('class') || '';
  }
  set className(v) {
    this.attrs.set('class', String(v));
  }
  get classList() {
    const self = this;
    const tokens = () => self.className.split(/\s+/).filter(Boolean);
    return {
      add(...c) {
        const t = tokens();
        c.forEach((x) => {
          if (!t.includes(x)) t.push(x);
        });
        self.className = t.join(' ');
      },
      remove(...c) {
        self.className = tokens().filter((x) => !c.includes(x)).join(' ');
      },
      contains(c) {
        return tokens().includes(c);
      },
      toggle(c, force) {
        const has = tokens().includes(c);
        const want = force === undefined ? !has : !!force;
        if (want) this.add(c);
        else this.remove(c);
        return want;
      }
    };
  }
  setAttribute(k, v) {
    if (FORBIDDEN_ATTR.test(k)) {
      violations.push('setAttribute ' + k);
    }
    this.attrs.set(k, String(v));
  }
  getAttribute(k) {
    return this.attrs.has(k) ? this.attrs.get(k) : null;
  }
  get textContent() {
    return this.childNodes.map((c) => c.textContent).join('');
  }
  set textContent(v) {
    this.textWrites += 1;
    this.childNodes.forEach((c) => {
      c.parentNode = null;
    });
    this.childNodes = [];
    if (String(v) !== '') {
      const t = new FakeText(v);
      t.parentNode = this;
      this.childNodes.push(t);
    }
  }
  appendChild(n) {
    if (n.parentNode) n.parentNode.removeChild(n);
    n.parentNode = this;
    this.childNodes.push(n);
    return n;
  }
  removeChild(n) {
    const i = this.childNodes.indexOf(n);
    if (i >= 0) this.childNodes.splice(i, 1);
    n.parentNode = null;
    return n;
  }
  replaceChildren(...nodes) {
    this.childNodes.forEach((c) => {
      c.parentNode = null;
    });
    this.childNodes = [];
    nodes.forEach((n) => this.appendChild(n));
  }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(fn);
  }
  dispatch(type, ev) {
    (this.listeners.get(type) || []).slice().forEach((fn) => fn(Object.assign({ target: this }, ev || {})));
  }
  focus() {}
  walk(fn) {
    fn(this);
    this.childNodes.forEach((c) => {
      if (c.nodeType === 1) c.walk(fn);
      else fn(c);
    });
  }
  all(pred) {
    const out = [];
    this.walk((n) => {
      if (n.nodeType === 1 && pred(n)) out.push(n);
    });
    return out;
  }
  hasClass(c) {
    return this.classList.contains(c);
  }
}

for (const prop of ['innerHTML', 'outerHTML']) {
  Object.defineProperty(FakeElement.prototype, prop, {
    get() {
      violations.push('read ' + prop);
      return '';
    },
    set() {
      violations.push('write ' + prop);
    }
  });
}
FakeElement.prototype.insertAdjacentHTML = function () {
  violations.push('insertAdjacentHTML');
};
Object.defineProperty(FakeElement.prototype, 'style', {
  get() {
    violations.push('style access');
    return {};
  }
});

function buildEnv(opts) {
  const env = {
    now: 0,
    timers: [],
    nextTimer: 1,
    calls: [],
    sources: [],
    created: [],
    maxStatusInflight: 0,
    statusInflight: 0,
    replaced: [],
    visibility: 'visible',
    docListeners: new Map(),
    winListeners: new Map(),
    server: Object.assign(
      {
        loginStatus: 200,
        sessionStatus: 200,
        statusStatus: 200,
        mergeStatus: 409,
        logOk: true,
        statusGate: null,
        eventCount: 600
      },
      opts.server || {}
    )
  };

  const registry = new Map();
  const html = HTML;
  const idRe = /<(\w+)([^>]*?)\sid="([^"]+)"/g;
  let m;
  while ((m = idRe.exec(html))) {
    const el = new FakeElement(env, m[1].toLowerCase());
    el.attrs.set('id', m[3]);
    const cls = /class="([^"]*)"/.exec(m[2]);
    if (cls) el.attrs.set('class', cls[1]);
    registry.set(m[3], el);
  }
  env.registry = registry;

  const document = {
    createElement(tag) {
      env.created.push(tag.toLowerCase());
      return new FakeElement(env, tag.toLowerCase());
    },
    createElementNS(ns, tag) {
      env.created.push(tag.toLowerCase());
      return new FakeElement(env, tag.toLowerCase(), ns);
    },
    createTextNode(t) {
      return new FakeText(t);
    },
    getElementById(id) {
      return registry.get(id) || null;
    },
    querySelector() {
      return null;
    },
    querySelectorAll() {
      return [];
    },
    get visibilityState() {
      return env.visibility;
    },
    addEventListener(type, fn) {
      if (!env.docListeners.has(type)) env.docListeners.set(type, []);
      env.docListeners.get(type).push(fn);
    },
    set cookie(v) {
      violations.push('document.cookie write');
    },
    get cookie() {
      return '';
    },
    write() {
      violations.push('document.write');
    }
  };
  env.document = document;

  const location = {
    pathname: '/',
    _hash: opts.hash || '',
    get hash() {
      return this._hash;
    },
    set hash(v) {
      const h = String(v).startsWith('#') ? String(v) : '#' + v;
      this._hash = h;
      (env.winListeners.get('hashchange') || []).slice().forEach((fn) => fn({}));
    }
  };
  env.location = location;
  const history = {
    replaceState(state, title, url) {
      env.replaced.push([state, title, url]);
      location._hash = String(url).includes('#') ? '#' + String(url).split('#')[1] : '';
    }
  };
  const window = {
    addEventListener(type, fn) {
      if (!env.winListeners.has(type)) env.winListeners.set(type, []);
      env.winListeners.get(type).push(fn);
    }
  };

  // timers: run only when the test advances the fake clock
  function addTimer(fn, ms, every) {
    const t = { id: env.nextTimer++, fn, due: env.now + (ms || 0), every: every ? ms : 0 };
    env.timers.push(t);
    return t.id;
  }
  const setTimeoutF = (fn, ms) => {
    if (typeof fn !== 'function') throw new Error('string timer');
    return addTimer(fn, ms, false);
  };
  const setIntervalF = (fn, ms) => {
    if (typeof fn !== 'function') throw new Error('string timer');
    return addTimer(fn, ms, true);
  };
  const clearTimer = (id) => {
    env.timers = env.timers.filter((t) => t.id !== id);
  };

  class FakeES {
    constructor(url) {
      this.url = url;
      this.readyState = 1;
      this.closed = false;
      this.ls = new Map();
      this.onerror = null;
      env.sources.push(this);
    }
    addEventListener(type, fn) {
      if (!this.ls.has(type)) this.ls.set(type, []);
      this.ls.get(type).push(fn);
    }
    close() {
      this.closed = true;
      this.readyState = 2;
    }
    emit(type, data) {
      (this.ls.get(type) || []).slice().forEach((fn) => fn({ data: JSON.stringify(data) }));
    }
  }

  const status = () => statusDoc(env);
  async function fetchF(url, init) {
    init = init || {};
    const call = {
      url: String(url),
      method: init.method || 'GET',
      headers: init.headers || {},
      body: init.body === undefined ? null : init.body,
      credentials: init.credentials,
      cache: init.cache
    };
    env.calls.push(call);
    const path = call.url.split('?')[0];
    const query = new URLSearchParams(call.url.split('?')[1] || '');
    const S = env.server;
    let code = 200;
    let body = null;
    let headers = {};
    const err = (c, msg, subject) => {
      code = c;
      body = { schema: 'fleet.error/1', error: { code: msg.code || 'error', message: msg.message || msg, subject: subject || null } };
    };
    if (path === '/api/session') {
      if (S.sessionStatus === 401) err(401, { code: 'unauthorized', message: 'no session' });
      else body = { schema: 'fleet.web.session/1', csrf: 'csrf-session', version: '0.1.0', bind: '127.0.0.1:7878' };
    } else if (path === '/api/login') {
      if (S.loginStatus === 429) {
        err(429, { code: 'rate_limited', message: 'slow down' });
        headers = { 'retry-after': '17' };
      } else if (S.loginStatus === 401) err(401, { code: 'unauthorized', message: 'bad token' });
      else body = { schema: 'fleet.web.session/1', csrf: 'csrf-login', version: '0.1.0', bind: '127.0.0.1:7878' };
    } else if (path === '/api/logout') {
      body = { schema: 'fleet.web.logout/1', ok: true };
    } else if (path === '/api/status') {
      env.statusInflight += 1;
      env.maxStatusInflight = Math.max(env.maxStatusInflight, env.statusInflight);
      try {
        if (S.statusGate) await S.statusGate;
      } finally {
        env.statusInflight -= 1;
      }
      if (S.statusStatus === 401) err(401, { code: 'unauthorized', message: 'expired' });
      else body = status();
    } else if (path === '/api/events') {
      const limit = Number(query.get('limit'));
      const n = Math.min(S.eventCount, 1000);
      const events = [];
      for (let i = 1; i <= n; i++) {
        events.push({ seq: i, ts_ms: 1700000000000 + i, line: i === n ? 'task ' + IMG + ' ' + ESC + ' ' + BIDI : 'event ' + i });
      }
      body = { schema: 'fleet.events/1', events: limit ? events : events };
    } else if (path === '/api/log') {
      body = S.logOk
        ? { schema: 'fleet.web.log/1', ok: true, records: 42, head_hash: DIGEST, error: null }
        : { schema: 'fleet.web.log/1', ok: false, records: null, head_hash: null, error: 'hash mismatch at record 7 ' + IMG };
    } else if (path === '/api/health') {
      body = {
        schema: 'fleet.doctor/1',
        ok: false,
        checks: [
          { name: 'git', status: 'ok', detail: 'git 2.45' },
          { name: 'worker ' + IMG, status: 'info', detail: SCRIPT },
          { name: 'disk', status: 'fail', detail: 'low ' + ESC }
        ]
      };
    } else if (path === '/api/tail') {
      const task = query.get('task');
      if (task === 'r1/web') {
        body = { schema: 'fleet.tail/1', task, live: true, text: 'boot ' + ESC + '\n', next: 12, lost: false };
      } else {
        body = { schema: 'fleet.tail/1', task, live: false, text: 'stored ' + IMG + '\n', next: 9, lost: false };
      }
    } else if (path === '/api/gate/resolve') {
      body = { schema: 'fleet.gate.resolve/1', id: JSON.parse(call.body).id, task: 'r1/web' };
    } else if (path === '/api/approval/decide') {
      const b = JSON.parse(call.body);
      body = { schema: 'fleet.approval.decide/1', id: b.id, decision: b.approve ? 'approved' : 'denied', digest: b.digest };
    } else if (path === '/api/send') {
      const b = JSON.parse(call.body);
      body = { schema: 'fleet.send/1', task: b.task, bytes: b.text.length + (b.newline ? 1 : 0) };
    } else if (path === '/api/merge') {
      if (S.mergeStatus === 409) err(409, { code: 'approval_required', message: 'needs approval' }, 'a1');
      else body = { schema: 'fleet.merge/1', task: 'r1/api', branch: 'fleet/r1/api', integration_branch: 'fleet/r1/integration', merge_sha: 'f'.repeat(40) };
    } else {
      err(404, { code: 'not_found', message: 'no such endpoint' });
    }
    return {
      ok: code >= 200 && code < 300,
      status: code,
      headers: { get: (k) => headers[String(k).toLowerCase()] || null },
      json: async () => {
        if (body === null) throw new Error('no body');
        return body;
      }
    };
  }

  env.advance = async function (ms) {
    const target = env.now + ms;
    for (;;) {
      await settle();
      const due = env.timers.filter((t) => t.due <= target).sort((a, b) => a.due - b.due || a.id - b.id)[0];
      if (!due) break;
      env.now = due.due;
      if (due.every) due.due += due.every;
      else env.timers = env.timers.filter((t) => t !== due);
      due.fn();
    }
    env.now = target;
    await settle();
  };

  env.setVisible = async function (v) {
    env.visibility = v;
    (env.docListeners.get('visibilitychange') || []).slice().forEach((fn) => fn({}));
    await settle();
  };

  const sandbox = {
    document,
    window,
    location,
    history,
    fetch: fetchF,
    EventSource: FakeES,
    setTimeout: setTimeoutF,
    clearTimeout: clearTimer,
    setInterval: setIntervalF,
    clearInterval: clearTimer,
    TextEncoder,
    console
  };
  env.context = vm.createContext(sandbox);
  env.view = registry.get('view');
  env.text = (id) => registry.get(id).textContent;
  env.posts = () => env.calls.filter((c) => c.method === 'POST' && c.url !== '/api/login');
  env.statusCalls = () => env.calls.filter((c) => c.url.startsWith('/api/status'));
  return env;
}

async function settle() {
  for (let i = 0; i < 6; i++) {
    await new Promise((r) => setImmediate(r));
  }
}

async function boot(opts) {
  const env = buildEnv(opts || {});
  vm.runInContext(APP, env.context, { filename: 'app.js' });
  await settle();
  return env;
}

// ------------------------------------------------------------- canned data

function statusDoc(env) {
  const now = Date.now();
  const mk = (id, title, status, deps, extra) =>
    Object.assign(
      {
        id,
        title,
        status,
        deps,
        worker: 'worker-' + id,
        attempt: 1,
        retries_used: 0,
        branch: 'fleet/r1/' + id,
        worktree: '/tmp/wt/' + id,
        head: null,
        merged: null,
        summary: null,
        failure: null,
        block: null,
        gates: [],
        live: false
      },
      extra || {}
    );
  return {
    schema: 'fleet.status/1',
    daemon: { pid: 1, version: '0.1.0', uptime_ms: 3661000, sessions: 1, max_sessions: 4, undo: 'available', rss_kb: 20480, output_bytes: 100 },
    now_ms: now,
    runs: [
      {
        id: 'r1',
        objective: 'ship ' + IMG,
        repo: '/tmp/repo',
        base_ref: 'main',
        base_sha: 'a'.repeat(40),
        status: 'active',
        max_parallel: 2,
        integration_branch: 'fleet/r1/integration',
        counts: { pending: 1, ready: 0, running: 1, review: 0, completed: 1, failed: 0, blocked: 0 },
        tasks: [
          mk('api', IMG, 'completed', [], {
            summary: SCRIPT + '\n' + ESC,
            branch: 'fleet/r1/' + BIDI,
            head: 'b'.repeat(40),
            gates: [{ name: 'tests ' + IMG, passed: true, summary: SCRIPT }, { name: 'lint', passed: false, summary: 'bad ' + ESC }]
          }),
          mk('web', ESC + ' ' + BIDI, 'running', ['api'], { live: true }),
          mk('ui', SCRIPT, 'pending', ['api', 'web'], { block: { kind: 'dependency', subject: 'web', detail: IMG } })
        ]
      }
    ],
    gates: [
      { id: 'g1', run: 'r1', task: 'web', kind: 'question', text: 'Which branch? ' + IMG + ' ' + BIDI, choices: ['main', SCRIPT], answer: null, open: true },
      { id: 'g0', run: 'r1', task: 'api', kind: 'question', text: 'old', choices: [], answer: 'yes ' + ESC, open: false }
    ],
    approvals: [
      {
        id: 'a1',
        run: 'r1',
        task: 'api',
        purpose: 'external-command',
        tool: 'command',
        command: '/usr/bin/tool',
        args: ['--flag', 'two words', IMG, BIDI],
        display: '/usr/bin/tool --flag "two words" ' + SCRIPT + ' ' + BIDI,
        digest: DIGEST,
        status: 'pending',
        requested_ms: now - 1000,
        expires_ms: now + 120000
      },
      {
        id: 'a0',
        run: 'r1',
        task: 'api',
        purpose: 'merge',
        tool: 'git',
        command: 'git',
        args: ['merge'],
        display: 'git merge',
        digest: DIGEST2,
        status: 'approved',
        requested_ms: now - 5000,
        expires_ms: now + 1000
      }
    ],
    recent: []
  };
}

// ----------------------------------------------------------------- helpers

const find = (env, pred) => env.view.all(pred);
const buttons = (env, label) => find(env, (n) => n.tagName === 'button' && n.textContent === label);
const hasTextNode = (env, s) => {
  let hit = false;
  env.view.walk((n) => {
    if (n.nodeType === 3 && n.data.includes(s)) hit = true;
  });
  return hit;
};
const allText = (env) => env.view.textContent;

function assertSafe(env, label) {
  const bad = new Set(['img', 'script', 'iframe', 'object', 'embed', 'style', 'link']);
  let problem = null;
  env.created.forEach((t) => {
    if (bad.has(t)) problem = 'created a <' + t + '> element';
  });
  env.view.walk((n) => {
    if (n.nodeType === 3) {
      if (/[\u202a-\u202e\u2066-\u2069]/.test(n.data)) problem = 'raw bidi control in a text node';
      if (n.data.includes('\u001b')) problem = 'raw ESC in a text node';
    }
  });
  check(problem === null, label + ': ' + problem);
}

// ------------------------------------------------- static file rules (source)

function staticChecks() {
  const banned = [
    'innerHTML', 'outerHTML', 'insertAdjacentHTML', 'document.write', 'eval(', 'new Function',
    "setAttribute('style'", 'setAttribute("style"', '.style.cssText', 'javascript:', 'srcdoc',
    'createContextualFragment', 'DOMParser', 'Function(', 'setTimeout("', "setTimeout('", 'setInterval("', "setInterval('",
    'localStorage', 'sessionStorage', 'document.cookie', '.href', '.src ='
  ];
  banned.forEach((b) => check(!APP.includes(b), 'app.js does not contain ' + b));
  check(!/https?:\/\//.test(APP) && !/https?:\/\//.test(HTML) && !/https?:\/\//.test(CSS), 'no absolute URL in the assets');
  check(HTML.startsWith('<!doctype html>'), 'doctype');
  check(HTML.includes('<meta charset=utf-8>'), 'charset');
  check(HTML.includes('<link rel="icon" href="data:,">'), 'icon');
  check(HTML.includes('<link rel="stylesheet" href="/app.css">'), 'stylesheet');
  check(HTML.includes('<script src="/app.js" defer></script>'), 'script tag');
  check(!/\sstyle\s*=/i.test(HTML), 'no style attribute in index.html');
  check(!/\son[a-z]+\s*=/i.test(HTML), 'no event handler attribute in index.html');
  check((HTML.match(/<script/g) || []).length === 1, 'exactly one script element');
  check(!/<style/i.test(HTML), 'no style element');
  check(!/@import/.test(CSS) && !/url\(/i.test(CSS), 'css has no @import or url()');
  check(CSS.includes('prefers-color-scheme: dark'), 'css supports dark mode');
  ['view', 'statusline', 'hdr-version', 'hdr-sessions', 'hdr-rss', 'hdr-approvals', 'logout', 'nav-runs', 'nav-decisions', 'nav-events', 'nav-health'].forEach(
    (id) => check(HTML.includes('id="' + id + '"'), 'index.html has #' + id)
  );
}

// --------------------------------------------------------------- scenarios

async function mainFlow() {
  const env = await boot({ hash: '#token=' + TOKEN });
  const login = env.calls[0];
  check(login.method === 'POST' && login.url === '/api/login', 'login is the first request');
  check(login.headers['Content-Type'] === 'application/json', 'login content type');
  check(JSON.parse(login.body).token === TOKEN, 'login body carries the token');
  check(login.credentials === 'same-origin' && login.cache === 'no-store', 'login fetch options');
  check(env.replaced.length === 1 && env.replaced[0][0] === null && env.replaced[0][2] === '/', 'fragment removed with replaceState(null, "", pathname)');
  check(env.location.hash === '', 'location hash is empty after login');
  check(!env.calls.some((c) => c.url === '/api/session'), 'no session call after a login');
  check(env.calls.every((c) => c.credentials === 'same-origin' && c.cache === 'no-store'), 'every fetch uses same-origin credentials and no-store');

  // runs view
  check(env.statusCalls().length >= 1 && env.statusCalls()[0].url === '/api/status', 'status polled without run on the runs page');
  check(env.text('hdr-version').includes('0.1.0'), 'header version');
  check(env.text('hdr-sessions') === '1/4 sessions', 'header sessions');
  check(env.text('hdr-rss') === 'rss 20.0 MiB', 'header rss');
  check(env.text('hdr-approvals') === '1 pending approval', 'header pending approvals count');
  check(hasTextNode(env, 'ship ' + IMG), 'runs: objective shown as a text node');
  check(env.registry.get('nav-runs').hasClass('active'), 'runs nav active');
  assertSafe(env, 'runs');

  // DAG
  const runButton = buttons(env, 'r1')[0];
  check(!!runButton, 'run id button');
  runButton.dispatch('click');
  await settle();
  check(env.location.hash === '#/run/r1', 'run button navigates');
  check(env.statusCalls().some((c) => c.url === '/api/status?run=r1'), 'status asks for the run');
  const dag = find(env, (n) => n.hasClass('dag'))[0];
  check(!!dag && dag.namespaceURI !== null, 'DAG svg exists in the svg namespace');
  const nodes = find(env, (n) => n.hasClass('node'));
  const lines = find(env, (n) => n.tagName === 'line');
  check(nodes.length === 3, 'three task boxes');
  check(lines.length === 3, 'three edges');
  const x = (id) => Number(nodes.find((n) => n.attrs.get('aria-label').includes('Task ' + id)).getAttribute('x'));
  check(x('api') < x('web') && x('web') < x('ui'), 'columns follow the longest dependency path');
  check(nodes.some((n) => n.hasClass('st-running')) && nodes.some((n) => n.hasClass('st-completed')) && nodes.some((n) => n.hasClass('st-pending')), 'status classes on boxes');
  check(hasTextNode(env, IMG), 'dag: hostile title is a text node');
  check(hasTextNode(env, SCRIPT), 'dag: script text is a text node');
  check(hasTextNode(env, '[U+001B][31mred'), 'dag: ESC shown visibly');
  check(hasTextNode(env, 'safe[U+202E]gnp.exe') || allText(env).includes('[U+202E]'), 'dag: bidi override shown visibly');
  check(find(env, (n) => n.tagName === 'text' && n.textContent.includes('worker-api')).length === 1, 'worker and attempt shown');
  assertSafe(env, 'dag');

  // task: stored output of a finished task
  nodes.find((n) => n.attrs.get('aria-label').includes('Task api')).dispatch('click');
  await settle();
  check(env.location.hash === '#/task/r1/api', 'box click opens the task page');
  const tail = env.calls.find((c) => c.url.startsWith('/api/tail'));
  check(tail.url === '/api/tail?task=r1%2Fapi&since=0&max=65536', 'tail url is encoded');
  check(env.sources.length === 0, 'no EventSource for a task that is not live');
  const pre = find(env, (n) => n.hasClass('output'))[0];
  await env.advance(100);
  check(pre.textContent.includes('stored ' + IMG), 'stored output is text');
  check(hasTextNode(env, SCRIPT + '\n[U+001B][31mred'), 'task: multi line summary as text with visible ESC');
  check(hasTextNode(env, 'fleet/r1/safe[U+202E]gnp.exe'), 'task: bidi in a branch name is made visible');
  check(find(env, (n) => n.hasClass('st-ok') && n.textContent === 'pass').length === 1 && find(env, (n) => n.hasClass('st-fail') && n.textContent === 'fail').length === 1, 'quality gate pass and fail badges');
  assertSafe(env, 'task api');

  // merge: confirm on second click, revert after 5 s, 409 shows the approval id
  const merge = buttons(env, 'Merge')[0];
  check(merge && merge.disabled === false, 'merge enabled for a completed, unmerged task');
  merge.dispatch('click');
  check(merge.textContent === 'Confirm?', 'first click asks for confirmation');
  check(env.posts().length === 0, 'no POST before the second click');
  await env.advance(5001);
  check(merge.textContent === 'Merge', 'confirm state reverts after 5 seconds');
  merge.dispatch('click');
  merge.dispatch('click');
  await settle();
  const mergePost = env.posts().find((c) => c.url === '/api/merge');
  check(!!mergePost, 'second click posts the merge');
  check(mergePost.headers['X-CSRF-Token'] === 'csrf-login' && mergePost.headers['Content-Type'] === 'application/json', 'merge carries the CSRF header');
  check(JSON.parse(mergePost.body).task === 'r1/api', 'merge body');
  check(allText(env).includes('Merge needs approval a1: approve it in the Decisions page, then press Merge again.'), 'approval_required message');

  // live task: stream, send box, output bound
  env.location.hash = '#/task/r1/web';
  await env.advance(100);
  check(env.sources.length === 1 && env.sources[0].url === '/api/stream?task=r1%2Fweb&since=12', 'stream opened after the tail, from its offset');
  const es = env.sources[0];
  const pre2 = find(env, (n) => n.hasClass('output'))[0];
  check(find(env, (n) => n.hasClass('send'))[0].hasClass('hidden') === false, 'send box visible for a live task');
  check(buttons(env, 'Merge')[0].disabled === true, 'merge disabled for a running task');
  es.emit('output', { text: ESC + ' ' + IMG + ' ' + BIDI + '\n', next: 40, lost: false });
  await env.advance(100);
  check(pre2.textContent.includes('[U+001B][31mred'), 'live output shows ESC visibly');
  check(pre2.textContent.includes(IMG), 'live output keeps markup as text');
  check(!/[\u202a-\u202e\u001b]/.test(pre2.textContent), 'live output has no raw ESC or bidi controls');
  const chunk = 'A'.repeat(100000);
  for (let i = 0; i < 5; i++) {
    es.emit('output', { text: chunk, next: 100 + i, lost: false });
  }
  es.emit('output', { text: 'END-MARK', next: 999, lost: true });
  const writesBefore = pre2.textWrites;
  await env.advance(100);
  check(pre2.textContent.length === 262144, 'live output is bounded to 256 KiB (got ' + pre2.textContent.length + ')');
  check(pre2.textContent.endsWith('END-MARK'), 'the tail of the output is kept');
  check(pre2.textWrites - writesBefore === 1, 'output writes are coalesced');
  check(!find(env, (n) => n.hasClass('notice') && n.hasClass('small'))[0].hasClass('hidden'), 'lost output shows a notice');
  es.onerror();
  check(find(env, (n) => n.tagName === 'p' && n.textContent.includes('retry')).length === 1, 'stream error shows a notice');
  es.emit('limit', {});
  check(find(env, (n) => n.tagName === 'p' && n.textContent === 'Reconnecting.').length === 1, 'limit shows reconnecting');

  // send
  const input = find(env, (n) => n.tagName === 'input' && n.getAttribute('type') === 'text')[0];
  check(input.getAttribute('maxlength') === '4096', 'send input has maxlength 4096');
  const send = buttons(env, 'Send')[0];
  input.value = '\u00e9'.repeat(2049);
  send.dispatch('click');
  send.dispatch('click');
  await settle();
  check(!env.posts().some((c) => c.url === '/api/send'), 'oversized text is refused before any request');
  check(allText(env).includes('4098 bytes'), 'oversize message names the byte count');
  input.value = 'yes please';
  send.dispatch('click');
  send.dispatch('click');
  await settle();
  const sendPost = env.posts().find((c) => c.url === '/api/send');
  check(!!sendPost && sendPost.headers['X-CSRF-Token'] === 'csrf-login', 'send carries the CSRF header');
  check(JSON.stringify(JSON.parse(sendPost.body)) === JSON.stringify({ task: 'r1/web', text: 'yes please', newline: true }), 'send body');

  // end of session closes the stream
  es.emit('end', { reason: 'exited' });
  check(es.closed === true, 'end closes the EventSource');
  check(allText(env).includes('Session ended: exited'), 'end shows session ended');
  assertSafe(env, 'task web');

  // leaving the task view closes a stream
  await env.advance(2100);
  env.location.hash = '#/task/r1/web';
  await env.advance(100);
  const es2 = env.sources[env.sources.length - 1];
  check(es2 !== es && !es2.closed, 'second visit opens a second stream');
  env.location.hash = '#/runs';
  await env.advance(100);
  check(es2.closed === true, 'leaving the task view closes the stream');

  // decisions
  env.location.hash = '#/decisions';
  await env.advance(100);
  check(allText(env).includes('Which branch? ' + IMG + ' safe[U+202E]gnp.exe'), 'gate text shown as text with visible bidi control');
  const choice = buttons(env, 'main')[0];
  choice.dispatch('click');
  const area = find(env, (n) => n.tagName === 'textarea')[0];
  check(area.value === 'main', 'a choice fills the answer box');
  const resolve = buttons(env, 'Resolve')[0];
  resolve.dispatch('click');
  resolve.dispatch('click');
  await settle();
  const gp = env.posts().find((c) => c.url === '/api/gate/resolve');
  check(!!gp && gp.headers['X-CSRF-Token'] === 'csrf-login', 'resolve carries the CSRF header');
  check(JSON.stringify(JSON.parse(gp.body)) === JSON.stringify({ id: 'g1', answer: 'main' }), 'resolve body');
  const blocks = find(env, (n) => n.tagName === 'pre');
  check(blocks.some((b) => b.textContent === '/usr/bin/tool --flag "two words" ' + SCRIPT + ' safe[U+202E]gnp.exe'), 'exact display text in a monospaced block');
  check(find(env, (n) => n.tagName === 'ol' && n.hasClass('args'))[0].childNodes.length === 4, 'every argument on its own line');
  check(find(env, (n) => n.tagName === 'li' && n.textContent === 'two words').length === 1, 'argument with a space stays one line');
  check(find(env, (n) => n.tagName === 'code' && n.textContent === DIGEST).length === 1, 'full digest shown');
  check(find(env, (n) => n.hasClass('countdown'))[0].textContent.endsWith('left'), 'countdown shown');
  check(buttons(env, 'Approve').length === 1 && buttons(env, 'Deny').length === 1, 'decision buttons only for the pending approval');
  const approve = buttons(env, 'Approve')[0];
  approve.dispatch('click');
  check(approve.textContent === 'Confirm?', 'approve needs a second click');
  approve.dispatch('click');
  await settle();
  const ap = env.posts().find((c) => c.url === '/api/approval/decide');
  check(!!ap && ap.headers['X-CSRF-Token'] === 'csrf-login', 'approve carries the CSRF header');
  check(JSON.stringify(JSON.parse(ap.body)) === JSON.stringify({ id: 'a1', approve: true, digest: DIGEST }), 'approve body has the displayed digest');
  const deny = buttons(env, 'Deny')[0];
  deny.dispatch('click');
  deny.dispatch('click');
  await settle();
  const dp = env.posts().filter((c) => c.url === '/api/approval/decide')[1];
  check(JSON.stringify(JSON.parse(dp.body)) === JSON.stringify({ id: 'a1', approve: false, digest: DIGEST }), 'deny body has the displayed digest');
  assertSafe(env, 'decisions');

  // every POST except the login has the CSRF token and JSON content type
  env.posts().filter((c) => c.url !== '/api/login').forEach((c) => {
    check(c.headers['X-CSRF-Token'] === 'csrf-login' && c.headers['Content-Type'] === 'application/json', 'POST ' + c.url + ' carries CSRF and content type');
  });

  // events: bounded list, chain status, polling every 5 s
  env.location.hash = '#/events';
  await env.advance(100);
  check(env.calls.some((c) => c.url === '/api/events?limit=500'), 'events requested with limit 500');
  const evList = find(env, (n) => n.hasClass('events'))[0];
  check(evList.childNodes.length === 500, 'at most 500 events are kept (got ' + evList.childNodes.length + ')');
  const banner = find(env, (n) => n.hasClass('chain'))[0];
  check(banner.hasClass('good') && banner.textContent.includes('42 records'), 'green banner when the chain verifies');
  check(hasTextNode(env, 'task ' + IMG + ' [U+001B][31mred[U+001B][0m safe[U+202E]gnp.exe'), 'event line is a text node with visible controls');
  const before = env.calls.filter((c) => c.url === '/api/events?limit=500').length;
  env.server.logOk = false;
  await env.advance(5100);
  check(env.calls.filter((c) => c.url === '/api/events?limit=500').length === before + 1, 'events poll every 5 seconds');
  check(banner.hasClass('bad') && banner.textContent.includes('BROKEN'), 'red banner when the chain is broken');
  assertSafe(env, 'events');

  // health
  env.location.hash = '#/health';
  await env.advance(100);
  const rows = find(env, (n) => n.tagName === 'tr');
  check(rows.length === 4, 'health table rows');
  check(find(env, (n) => n.hasClass('st-ok')).length >= 1 && find(env, (n) => n.hasClass('st-info')).length >= 1 && find(env, (n) => n.hasClass('st-fail')).length >= 1, 'health status classes');
  check(hasTextNode(env, SCRIPT), 'health detail shown as text');
  assertSafe(env, 'health');

  // polling: every 2 s while visible, never overlapping, paused while hidden
  env.location.hash = '#/runs';
  await env.advance(100);
  let n0 = env.statusCalls().length;
  await env.advance(2100);
  check(env.statusCalls().length === n0 + 1, 'status polled every 2 seconds');
  await env.setVisible('hidden');
  n0 = env.statusCalls().length;
  await env.advance(10000);
  check(env.statusCalls().length === n0, 'no polling while the tab is hidden');
  let release;
  env.server.statusGate = new Promise((r) => {
    release = r;
  });
  await env.setVisible('visible');
  await env.advance(10000);
  check(env.maxStatusInflight === 1 && env.statusInflight === 1, 'a slow status request is never overlapped');
  release();
  env.server.statusGate = null;
  await env.advance(100);

  // an expired session ends the polling and says so
  env.server.statusStatus = 401;
  await env.advance(2100);
  check(allText(env).includes('Not logged in. Open the login URL printed by `fleet daemon --web` in its terminal.'), 'expired session message');
  n0 = env.statusCalls().length;
  await env.advance(10000);
  check(env.statusCalls().length === n0, 'no polling after the session ended');

  check(violations.length === 0, 'no unsafe DOM API was used: ' + violations.join(', '));
}

async function sessionFlow() {
  const env = await boot({ hash: '' });
  check(env.calls[0].url === '/api/session' && env.calls[0].method === 'GET', 'without a fragment the session is asked');
  check(!env.calls.some((c) => c.url === '/api/login'), 'no login without a fragment');
  check(env.replaced.length === 0, 'history untouched without a fragment');
  check(allText(env).includes('ship '), 'runs view rendered from a cookie session');
  // logout
  const lo = env.registry.get('logout');
  lo.dispatch('click');
  check(lo.textContent === 'Confirm?', 'logout needs a second click');
  lo.dispatch('click');
  await settle();
  const post = env.posts().find((c) => c.url === '/api/logout');
  check(!!post && post.headers['X-CSRF-Token'] === 'csrf-session' && post.body === '{}', 'logout is a CSRF-protected POST');
  check(allText(env).includes('Logged out.'), 'logout message');
}

async function notLoggedIn() {
  const env = await boot({ hash: '', server: { sessionStatus: 401 } });
  check(allText(env) === 'Not logged in. Open the login URL printed by `fleet daemon --web` in its terminal.', 'plain not-logged-in message');
  check(env.statusCalls().length === 0, 'no polling without a session');
}

async function rateLimited() {
  const env = await boot({ hash: '#token=' + TOKEN, server: { loginStatus: 429 } });
  check(allText(env).includes('17 seconds'), 'Retry-After shown for a rate limited login');
  check(env.replaced.length === 1, 'fragment removed even when the login fails');
  check(env.statusCalls().length === 0, 'no polling after a failed login');
  const env2 = await boot({ hash: '#token=' + TOKEN, server: { loginStatus: 401 } });
  check(allText(env2).includes('bad token'), 'login failure shows the server message');
  check(env2.replaced.length === 1 && env2.location.hash === '', 'fragment removed after a rejected token');
  const env3 = await boot({ hash: '#token=nothex' });
  check(env3.calls[0].url === '/api/session', 'a malformed token is not sent to the server');
}

staticChecks();
await mainFlow();
await sessionFlow();
await notLoggedIn();
await rateLimited();
check(violations.length === 0, 'no unsafe DOM API was used: ' + violations.join(', '));
console.log('web-smoke: ok (' + checks + ' checks)');
