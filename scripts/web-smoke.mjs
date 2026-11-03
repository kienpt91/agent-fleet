// Smoke test of the dashboard script in crates/fleet-infra/web/app.js.
// No dependencies. It runs the real script in a vm context against a small fake DOM, a fake
// fetch with a streaming body and a fake session storage. Run it from the repository root: node scripts/web-smoke.mjs
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
const SESSION = '5e'.repeat(32);
const CSRF = 'c5'.repeat(32);
const CSRF2 = 'd6'.repeat(32);

// ----------------------------------------------------------------- fake DOM

const FORBIDDEN_ATTR = /^(style|src|srcdoc|on.*)$/i;
const HREF_OK = /^#\/(goal|task)\/[A-Za-z0-9._-]{1,40}(\/[A-Za-z0-9._-]{1,40})?$|^#\/goals$/;

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
    if (FORBIDDEN_ATTR.test(k) || (k.toLowerCase() === 'href' && !HREF_OK.test(String(v)))) {
      violations.push('setAttribute ' + k + ' ' + String(v).slice(0, 40));
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
    streams: [],
    goalFetches: [],
    unauth: [],
    storageKeys: new Set(),
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
        eventCount: 600,
        streamStatus: 200,
        streamThrow: 0,
        goalsStatus: 200,
        goalCount: 4,
        messagesStatus: 200
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

  class FakeStream {
    constructor(call, signal) {
      this.call = call;
      this.signal = signal;
      this.queue = [];
      this.waiter = null;
      this.done = false;
      this.failed = false;
      this.cancelled = false;
      signal.addEventListener('abort', () => this.wake());
    }
    wake() {
      const w = this.waiter;
      this.waiter = null;
      if (w) w();
    }
    push(data) {
      this.queue.push(typeof data === 'string' ? new TextEncoder().encode(data) : data);
      this.wake();
    }
    frame(event, data, id) {
      this.push((id === undefined ? '' : 'id: ' + id + '\n') + 'event: ' + event + '\ndata: ' + JSON.stringify(data) + '\n\n');
    }
    finish() {
      this.done = true;
      this.wake();
    }
    fail() {
      this.failed = true;
      this.wake();
    }
    reader() {
      const self = this;
      return {
        read() {
          return new Promise((resolve, reject) => {
            const attempt = () => {
              if (self.signal.aborted) reject(new Error('aborted'));
              else if (self.failed) reject(new Error('connection reset'));
              else if (self.queue.length) resolve({ value: self.queue.shift(), done: false });
              else if (self.done) resolve({ value: undefined, done: true });
              else self.waiter = attempt;
            };
            attempt();
          });
        },
        cancel() {
          self.cancelled = true;
          return Promise.resolve();
        }
      };
    }
  }

  const storage = { data: new Map() };
  if (opts.stored === true) {
    storage.data.set('fleet.session', JSON.stringify({ session: SESSION, csrf: CSRF }));
  } else if (typeof opts.stored === 'string') {
    storage.data.set('fleet.session', opts.stored);
  }
  env.storage = storage;
  const sessionStorage = {
    getItem(k) {
      env.storageKeys.add(k);
      return storage.data.has(k) ? storage.data.get(k) : null;
    },
    setItem(k, v) {
      env.storageKeys.add(k);
      storage.data.set(k, String(v));
    },
    removeItem(k) {
      env.storageKeys.add(k);
      storage.data.delete(k);
    }
  };

  const status = () => statusDoc(env);
  async function fetchF(url, init) {
    init = init || {};
    const call = {
      url: String(url),
      method: init.method || 'GET',
      headers: Object.assign({}, init.headers),
      body: init.body === undefined ? null : init.body,
      credentials: init.credentials,
      cache: init.cache,
      signal: init.signal
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
    const authFail = path !== '/api/login' && call.headers['X-Fleet-Session'] !== SESSION;
    if (authFail) {
      env.unauth.push(call.url);
      err(401, { code: 'unauthorized', message: 'missing or wrong session header' });
    } else if (path === '/api/stream') {
      if (S.streamThrow > 0) {
        S.streamThrow -= 1;
        throw new Error('network down');
      }
      if (S.streamStatus !== 200) {
        err(S.streamStatus, { code: 'stream', message: 'stream refused' });
      } else {
        const stream = new FakeStream(call, init.signal);
        env.streams.push(stream);
        return {
          ok: true,
          status: 200,
          headers: { get: () => null },
          body: { getReader: () => stream.reader() },
          json: async () => {
            throw new Error('not json');
          }
        };
      }
    } else if (path === '/api/session') {
      if (S.sessionStatus === 401) err(401, { code: 'unauthorized', message: 'no session' });
      else body = { schema: 'fleet.web.session/1', session: null, csrf: CSRF2, version: '0.1.0', bind: '127.0.0.1:7878' };
    } else if (path === '/api/login') {
      if (S.loginStatus === 429) {
        err(429, { code: 'rate_limited', message: 'slow down' });
        headers = { 'retry-after': '17' };
      } else if (S.loginStatus === 401) err(401, { code: 'unauthorized', message: 'bad token' });
      else body = { schema: 'fleet.web.session/1', session: SESSION, csrf: CSRF, version: '0.1.0', bind: '127.0.0.1:7878' };
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
    } else if (path === '/api/goals') {
      if (S.goalsStatus !== 200) err(S.goalsStatus, { code: 'goals', message: 'goals unavailable' });
      else body = goalsDoc(S.goalCount);
    } else if (path === '/api/remaining') {
      body = remainingDoc();
    } else if (path === '/api/messages') {
      if (S.messagesStatus !== 200) err(S.messagesStatus, { code: 'msgs', message: 'msgs unavailable' });
      else body = msgListDoc();
    } else if (path === '/api/message') {
      body = msgShowDoc(query.get('id'));
    } else if (path === '/api/goal') {
      const gid = query.get('id');
      env.goalFetches.push(gid);
      body = goalShowDoc(gid);
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
    sessionStorage,
    AbortController,
    TextDecoder,
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
  env.openStreams = () => env.streams.filter((s) => !s.signal.aborted && !s.cancelled && !s.done && !s.failed);
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

const COMMIT = 'a1'.repeat(20);
const TREE = 'b2'.repeat(20);
const CONTRACT_ID = 'c3'.repeat(20);

function goalsDoc(count) {
  const base = [
    { id: 'g1', parent: null, title: 'Ship ' + IMG, status: 'in_progress', reason: 'because ' + ESC, depth: 0, children: 2, contract: 'pending', contract_id: CONTRACT_ID, evidence: 3,
      tasks: [{ task: 'r1/api', status: 'completed', verification: 'verified' }, { task: 'r1/web', status: 'review', verification: 'unverified' }, { task: 'not a ref<x>', status: 'pending', verification: 'failed' }] },
    { id: 'g2', parent: 'g1', title: 'Child ' + SCRIPT, status: 'open', reason: '', depth: 1, children: 0, contract: 'none', contract_id: null, evidence: 0, tasks: [{ task: 'r1/ui', status: 'pending', verification: 'none' }] },
    { id: 'g3', parent: 'g1', title: 'Done ' + BIDI, status: 'verified', reason: '', depth: 1, children: 0, contract: 'passed', contract_id: CONTRACT_ID, evidence: 1, tasks: [] },
    { id: 'bad id<x>', parent: null, title: 'Weird ' + IMG, status: 'abandoned', reason: 'dropped', depth: 0, children: 0, contract: 'mystery', contract_id: null, evidence: 0, tasks: [] }
  ];
  const goals = base.slice();
  for (let i = 4; i < count; i++) {
    goals.push({ id: 'm' + i, parent: null, title: 'Many ' + i, status: 'open', reason: '', depth: 0, children: 0, contract: 'pending', contract_id: null, evidence: 1, tasks: [{ task: 'r1/api', status: 'completed', verification: 'pending' }] });
  }
  const verified = goals.filter((g) => g.status === 'verified').length;
  const abandoned = goals.filter((g) => g.status === 'abandoned').length;
  return { schema: 'fleet.goals.list/1', total: goals.length, verified, abandoned, goals };
}

function remainingDoc() {
  return {
    schema: 'fleet.goals.remaining/1',
    total: 4,
    verified: 1,
    abandoned: 1,
    open_leaves: [goalsDoc(4).goals[1]],
    blockers: [
      { kind: 'goal', goal: 'g1', title: 'Blocked ' + IMG, detail: SCRIPT + '\n' + ESC, status: 'blocked' },
      { kind: 'task', goal: 'g1', task: 'r1/web', detail: 'needs ' + BIDI }
    ],
    next_ready: [{ task: 'r1/ui', goal: 'g2', title: 'Do ' + BIDI }]
  };
}

function msgRow(id, label, kind, status, extra) {
  return Object.assign({
    id, run: 'r1', channel: 'dev ' + IMG, from: 'api', label, kind, subject: 'subject ' + id, status, reason: '', detail: '', bytes: 10,
    in_reply_to: null, depth: 0, at_ms: 1700000000000, expires_ms: 1700000600000, approval: null,
    to: [{ task: 'web', state: status === 'delivered' ? 'delivered' : 'queued', at_ms: null, reason: '' }]
  }, extra || {});
}

function msgRows() {
  return [
    msgRow('m3', LABEL_PEER, 'note', 'blocked', { subject: MSG_HOSTILE, reason: 'secret-detected', detail: 'matched a key shape' }),
    msgRow('m2', LABEL_OP, 'request', 'delivered', { from: 'operator' }),
    msgRow('m1', LABEL_PEER, 'reply', 'awaiting-approval'),
    msgRow(ODD_ID, LABEL_PEER, 'note', 'queued')
  ];
}

function msgListDoc() {
  const channel = (name, muted, counts) => ({
    run: 'r1', name, members: ['api', 'web'], lead: 'api', mode: 'peer', direction: 'both', requires_approval: false,
    max_message_bytes: 4096, max_messages_per_task: 20, max_messages_per_minute: 10, ttl_secs: 600,
    muted, mute_reason: muted ? 'endpoint-ended ' + SCRIPT : null, mute_by: muted ? 'operator' : null, mute_gate: null, messages: counts
  });
  return {
    schema: 'fleet.msg.list/1',
    channels: [
      channel('dev ' + IMG, true, { queued: 1, awaiting_approval: 2, delivered: 3, expired: 0, blocked: 1 }),
      channel('ops', false, { queued: 0, awaiting_approval: 0, delivered: 0, expired: 0, blocked: 0 })
    ],
    muted_tasks: [{ run: 'r1', task: 'web', channel: 'talk', reason: 'looping ' + ESC, by: 'operator', gate: null }],
    messages: msgRows().map((m) => Object.assign({}, m)),
    total: 4,
    truncated: false
  };
}

function msgShowDoc(id) {
  const m = msgRows().find((x) => x.id === id) || msgRows()[0];
  const body = m.id === 'm3' ? 'body ' + SCRIPT + '\n' + ESC : m.id === 'm2' ? 'Z'.repeat(30000) : 'plain body';
  return { schema: 'fleet.msg.show/1', message: Object.assign({}, m, { body, digest: 'ab'.repeat(32), source: 'worker' }) };
}

function evidence(id, check, verdict, task, extra) {
  return Object.assign({ id, check, verdict, exit_code: verdict === 'passed' ? 0 : 1, summary: 'summary ' + IMG + ' ' + ESC, output: 'out ' + SCRIPT + '\n' + ESC, commit: COMMIT, tree: TREE, dirty: false, counts: true, task, at_ms: 1700000000000 }, extra || {});
}

function goalShowDoc(id) {
  const g = goalsDoc(4).goals.find((x) => x.id === id) || goalsDoc(4).goals[0];
  const big = evidence('e2', 'tests', 'failed', 'r1/web', { dirty: true, counts: false, output: ESC + IMG + '\n' + 'Z'.repeat(30000) });
  const history = [
    evidence('e3', 'lint', 'passed', 'r1/api', { summary: 'lint ok' }),
    big,
    evidence('e1', 'tests', 'passed', 'r1/api', { summary: 'tests ok ' + BIDI })
  ];
  return Object.assign({}, g, {
    schema: 'fleet.goals.show/1',
    intent: 'intent ' + SCRIPT + '\nsecond line ' + ESC,
    child_ids: ['g2', 'g3', 'bad id<x>'],
    checks: [
      { check: 'tests', kind: 'command', mandatory: true, detail: 'cargo test ' + IMG, latest: history[2] },
      { check: 'lint', kind: 'refcheck-json', mandatory: false, detail: 'lint ' + BIDI, latest: null }
    ],
    history
  });
}

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
    'localStorage', 'EventSource', 'cookie', '.href', '.src ='
  ];
  banned.forEach((b) => check(!APP.includes(b), 'app.js does not contain ' + b));
  ['/api/msg', 'msg.send', 'msg.mute', 'msg.unmute', '/api/messages/', '/api/message/'].forEach((b) => check(!APP.includes(b), 'app.js has no way to send, mute or unmute: ' + b));
  check(APP.includes("'/api/messages?limit='") && APP.includes("'/api/message?id='"), 'app.js reads /api/messages and /api/message');
  check(APP.includes('X-Fleet-Session') && APP.includes('X-CSRF-Token'), 'app.js sends the session and CSRF headers');
  const storageLines = APP.split('\n').filter((l) => l.includes('sessionStorage'));
  check(storageLines.length > 0 && storageLines.every((l) => l.includes("'fleet.session'")), 'sessionStorage is only used with the key fleet.session');
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
  ['view', 'statusline', 'hdr-version', 'hdr-sessions', 'hdr-rss', 'hdr-approvals', 'hdr-goals', 'logout', 'nav-runs', 'nav-goals', 'nav-messages', 'nav-decisions', 'nav-events', 'nav-health'].forEach(
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
  check(login.credentials === 'omit' && login.cache === 'no-store' && !('X-Fleet-Session' in login.headers), 'login fetch options: no ambient credentials, no session header yet');
  check(env.replaced.length === 1 && env.replaced[0][0] === null && env.replaced[0][2] === '/#/runs', 'fragment replaced with replaceState(null, "", pathname + "#/runs")');
  check(env.location.hash === '#/runs' && !env.location.hash.includes('token') && !env.location.hash.includes(TOKEN), 'location holds no token after login and shows the default route');
  check(!env.calls.some((c) => c.url === '/api/session'), 'no session call after a login');
  check(env.calls.every((c) => c.credentials === 'omit' && c.cache === 'no-store'), 'no request relies on ambient credentials; all use no-store');
  check(env.calls.slice(1).every((c) => c.headers['X-Fleet-Session'] === SESSION) && env.unauth.length === 0, 'every request after the login carries X-Fleet-Session');
  const stored = JSON.parse(env.storage.data.get('fleet.session'));
  check(env.storage.data.size === 1 && stored.session === SESSION && stored.csrf === CSRF && Object.keys(stored).length === 2, 'session storage holds only session and csrf under fleet.session');
  check(env.storageKeys.size === 1 && env.storageKeys.has('fleet.session'), 'no other storage key is used');

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
  check(env.streams.length === 0, 'no stream for a task that is not live');
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
  check(mergePost.headers['X-CSRF-Token'] === CSRF && mergePost.headers['X-Fleet-Session'] === SESSION && mergePost.headers['Content-Type'] === 'application/json', 'merge carries the CSRF and session headers');
  check(JSON.parse(mergePost.body).task === 'r1/api', 'merge body');
  check(allText(env).includes('Merge needs approval a1: approve it in the Decisions page, then press Merge again.'), 'approval_required message');

  // live task: fetch based stream, send box, output bound
  env.location.hash = '#/task/r1/web';
  await env.advance(100);
  check(env.streams.length === 1 && env.streams[0].call.url === '/api/stream?task=r1%2Fweb&since=12', 'stream opened after the tail, from its offset');
  const s1 = env.streams[0];
  check(s1.call.headers['X-Fleet-Session'] === SESSION, 'stream request carries the session header');
  check(s1.call.headers['Last-Event-ID'] === undefined, 'first stream request has no Last-Event-ID');
  check(s1.call.credentials === 'omit' && s1.call.method === 'GET' && !!s1.call.signal, 'stream request: no ambient credentials, abortable');
  const pre2 = find(env, (n) => n.hasClass('output'))[0];
  check(find(env, (n) => n.hasClass('send'))[0].hasClass('hidden') === false, 'send box visible for a live task');
  check(buttons(env, 'Merge')[0].disabled === true, 'merge disabled for a running task');

  // a frame split across two chunks, inside a multi-byte character
  const euro = new TextEncoder().encode('id: 20\nevent: output\ndata: ' + JSON.stringify({ text: 'split-\u20ac-ok', next: 20, lost: false }) + '\n\n');
  const cut = euro.indexOf(0xe2) + 1;
  s1.push(euro.slice(0, cut));
  await env.advance(100);
  check(!pre2.textContent.includes('split'), 'a partial frame is not shown yet');
  s1.push(euro.slice(cut));
  await env.advance(100);
  check(pre2.textContent.includes('split-\u20ac-ok'), 'a frame split across two chunks parses');

  // comments, unknown events and CRLF line endings
  s1.push(': keep-alive\r\n\r\n');
  s1.push('event: ping\ndata: {}\n\n');
  s1.push('id: 40\r\nevent: output\r\ndata: ' + JSON.stringify({ text: ESC + ' ' + IMG + ' ' + BIDI + '\n', next: 40, lost: false }) + '\r\n\r\n');
  await env.advance(100);
  check(pre2.textContent.includes('[U+001B][31mred'), 'live output shows ESC visibly');
  check(pre2.textContent.includes(IMG), 'live output keeps markup as text');
  check(!/[\u202a-\u202e\u001b]/.test(pre2.textContent), 'live output has no raw ESC or bidi controls');
  const chunk = 'A'.repeat(100000);
  for (let i = 0; i < 5; i++) {
    s1.frame('output', { text: chunk, next: 100 + i, lost: false }, 100 + i);
  }
  s1.frame('output', { text: 'END-MARK', next: 999, lost: true }, 999);
  const writesBefore = pre2.textWrites;
  await env.advance(100);
  check(pre2.textContent.length === 262144, 'live output is bounded to 256 KiB (got ' + pre2.textContent.length + ')');
  check(pre2.textContent.endsWith('END-MARK'), 'the tail of the output is kept');
  check(pre2.textWrites - writesBefore === 1, 'output writes are coalesced');
  check(!find(env, (n) => n.hasClass('notice') && n.hasClass('small'))[0].hasClass('hidden'), 'lost output shows a notice');

  // limit: reconnect after 1 s, resuming from the last id
  s1.frame('limit', { next: 1000 }, 1000);
  await env.advance(100);
  check(find(env, (n) => n.tagName === 'p' && n.textContent === 'Reconnecting.').length === 1, 'limit shows reconnecting');
  check(env.streams.length === 1, 'no reconnect before one second');
  await env.advance(1000);
  check(env.streams.length === 2, 'limit reconnects after one second');
  const s2 = env.streams[1];
  check(s2.call.headers['Last-Event-ID'] === '1000' && s2.call.url === '/api/stream?task=r1%2Fweb&since=1000', 'the resumed request carries Last-Event-ID and since');
  check(s2.call.headers['X-Fleet-Session'] === SESSION, 'the resumed request carries the session header');
  check(s1.cancelled === true, 'the old stream body was cancelled');
  check(env.openStreams().length === 1, 'never two open streams for one view');

  // a failed connection is retried after 3 s, resuming from the last id
  s2.frame('output', { text: 'TAIL2', next: 1004, lost: false }, 1004);
  await env.advance(100);
  s2.fail();
  await env.advance(100);
  check(find(env, (n) => n.tagName === 'p' && n.textContent.includes('retrying in 3 s (1 of 5)')).length === 1, 'stream error shows a notice');
  await env.advance(2000);
  check(env.streams.length === 2, 'no retry before three seconds');
  await env.advance(1100);
  check(env.streams.length === 3, 'a failed stream is retried');
  const s3 = env.streams[2];
  check(s3.call.headers['Last-Event-ID'] === '1004', 'the retry resumes from the last id');

  // a body that ends without an end event is reconnected
  s3.finish();
  await env.advance(1100);
  check(env.streams.length === 4, 'a body that ends without an end event reconnects after one second');
  const s4 = env.streams[3];
  check(s4.call.headers['Last-Event-ID'] === '1004' && env.openStreams().length === 1, 'reconnect resumes and never overlaps');

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
  check(!!sendPost && sendPost.headers['X-CSRF-Token'] === CSRF && sendPost.headers['X-Fleet-Session'] === SESSION, 'send carries the CSRF and session headers');
  check(JSON.stringify(JSON.parse(sendPost.body)) === JSON.stringify({ task: 'r1/web', text: 'yes please', newline: true }), 'send body');

  // an end event stops the stream for good
  s4.frame('end', { reason: 'exited' }, 1010);
  await env.advance(100);
  check(s4.cancelled === true, 'end closes the stream');
  check(allText(env).includes('Session ended: exited'), 'end shows session ended');
  assertSafe(env, 'task web');
  await env.advance(5000);
  check(env.streams.length === 4 && env.openStreams().length === 0, 'no reconnect after an end event');

  // leaving the task view aborts the stream
  env.location.hash = '#/task/r1/web';
  await env.advance(100);
  const s5 = env.streams[env.streams.length - 1];
  check(env.streams.length === 5 && !s5.signal.aborted, 'second visit opens a second stream');
  env.location.hash = '#/runs';
  await env.advance(100);
  check(s5.signal.aborted === true, 'leaving the task view aborts the stream');

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
  check(!!gp && gp.headers['X-CSRF-Token'] === CSRF, 'resolve carries the CSRF header');
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
  check(!!ap && ap.headers['X-CSRF-Token'] === CSRF, 'approve carries the CSRF header');
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
    check(c.headers['X-CSRF-Token'] === CSRF && c.headers['X-Fleet-Session'] === SESSION && c.headers['Content-Type'] === 'application/json', 'POST ' + c.url + ' carries CSRF, session and content type');
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
  check(env.storage.data.has('fleet.session') === false, 'a 401 clears the stored session');
  check(env.unauth.length === 0, 'the server never saw a request without the session header');

  check(violations.length === 0, 'no unsafe DOM API was used: ' + violations.join(', '));
}

async function sessionFlow() {
  const env = await boot({ hash: '', stored: true });
  const first = env.calls[0];
  check(first.url === '/api/session' && first.method === 'GET' && first.headers['X-Fleet-Session'] === SESSION && first.credentials === 'omit', 'a stored session is validated with the session header');
  check(!env.calls.some((c) => c.url === '/api/login'), 'no login without a fragment');
  check(env.replaced.length === 0, 'history untouched without a fragment');
  check(allText(env).includes('ship '), 'runs view rendered from a stored session');
  const stored = JSON.parse(env.storage.data.get('fleet.session'));
  check(stored.session === SESSION && stored.csrf === CSRF2 && Object.keys(stored).length === 2, 'the refreshed csrf token is stored with the session');
  check(env.unauth.length === 0 && env.calls.every((c) => c.headers['X-Fleet-Session'] === SESSION), 'every request carries X-Fleet-Session');
  // logout
  const lo = env.registry.get('logout');
  lo.dispatch('click');
  check(lo.textContent === 'Confirm?', 'logout needs a second click');
  lo.dispatch('click');
  await settle();
  const post = env.posts().find((c) => c.url === '/api/logout');
  check(!!post && post.headers['X-CSRF-Token'] === CSRF2 && post.headers['X-Fleet-Session'] === SESSION && post.body === '{}', 'logout is a CSRF-protected POST with the session header');
  check(allText(env).includes('Logged out.'), 'logout message');
  check(env.storage.data.size === 0, 'logout clears the stored session');
}

const NOT_LOGGED_IN = 'Not logged in. Open the login URL printed by `fleet daemon --web` in its terminal.';

async function notLoggedIn() {
  const env = await boot({ hash: '' });
  check(allText(env) === NOT_LOGGED_IN, 'plain not-logged-in message');
  check(env.calls.length === 0, 'nothing stored and no fragment: no request at all');
  const env2 = await boot({ hash: '', stored: true, server: { sessionStatus: 401 } });
  check(allText(env2) === NOT_LOGGED_IN, 'a rejected stored session shows the not-logged-in message');
  check(env2.storage.data.size === 0, 'a 401 clears the stored session');
  check(env2.statusCalls().length === 0, 'no polling without a session');
  const env3 = await boot({ hash: '', stored: '{"session":"nope","csrf":1}' });
  check(allText(env3) === NOT_LOGGED_IN && env3.calls.length === 0, 'a malformed stored session is ignored');
}

async function rateLimited() {
  const env = await boot({ hash: '#token=' + TOKEN, server: { loginStatus: 429 } });
  check(allText(env).includes('17 seconds'), 'Retry-After shown for a rate limited login');
  check(env.replaced.length === 1, 'fragment removed even when the login fails');
  check(env.statusCalls().length === 0, 'no polling after a failed login');
  check(env.storage.data.size === 0, 'nothing is stored after a failed login');
  const env2 = await boot({ hash: '#token=' + TOKEN, server: { loginStatus: 401 } });
  check(allText(env2).includes('bad token'), 'login failure shows the server message');
  check(env2.replaced.length === 1 && env2.location.hash === '', 'fragment removed after a rejected token');
  const env3 = await boot({ hash: '#token=nothex' });
  check(env3.calls.length === 0 && allText(env3) === NOT_LOGGED_IN, 'a malformed token is not sent to the server');
}

async function nextRoute() {
  const valid = ['/decisions', '/events', '/health', '/run/r1', '/task/r1/api', '/runs'];
  for (const n of valid) {
    const env = await boot({ hash: '#token=' + TOKEN + '&next=' + n });
    check(env.replaced.length === 1 && env.replaced[0][2] === '/#' + n, 'valid next ' + n + ' becomes the hash');
    check(env.location.hash === '#' + n && !env.location.hash.includes('token'), 'token removed, route ' + n + ' kept in the location');
    check(JSON.parse(env.calls[0].body).token === TOKEN && env.calls[0].body.indexOf('next') < 0, 'next is not sent to the server');
  }
  const env = await boot({ hash: '#token=' + TOKEN + '&next=/run/r1' });
  check(find(env, (n) => n.hasClass('dag')).length === 1, 'the router shows the requested run view after the login');
  check(env.statusCalls()[0].url === '/api/status?run=r1', 'the first status request is for the requested run');
  const env2 = await boot({ hash: '#next=/events&token=' + TOKEN });
  check(env2.location.hash === '#/events' && find(env2, (n) => n.hasClass('events')).length === 1, 'next may come before the token');
  const bad = ['//evil', '/a<b', '/a:b', '/a%2Fb', '/a?b', '/' + 'a'.repeat(201), 'run/r1', '', '/a b', '//'];
  for (const n of bad) {
    const e = await boot({ hash: '#token=' + TOKEN + '&next=' + n });
    check(e.replaced.length === 1 && e.replaced[0][2] === '/#/runs', 'invalid next ' + JSON.stringify(n.slice(0, 12)) + ' falls back to /runs');
    check(e.location.hash === '#/runs' && !e.location.hash.includes('token'), 'token removed after an invalid next');
  }
  const bound = await boot({ hash: '#token=' + TOKEN + '&next=/' + 'a'.repeat(200) });
  check(bound.location.hash === '#/' + 'a'.repeat(200), 'next of 200 characters after the slash is accepted');
  const failed = await boot({ hash: '#token=' + TOKEN + '&next=/run/r1', server: { loginStatus: 401 } });
  check(failed.replaced.length === 1 && failed.replaced[0][2] === '/' && failed.location.hash === '', 'a failed login drops the fragment and does not follow next');
}

async function goalsFlow() {
  const env = await boot({ hash: '#/goals', stored: true });
  await env.advance(100);
  check(env.registry.get('nav-goals').hasClass('active'), 'goals nav active');
  check(env.calls.some((c) => c.url === '/api/goals') && env.calls.some((c) => c.url === '/api/remaining'), 'goals and remaining are requested');
  check(find(env, (n) => n.hasClass('goalsum'))[0].textContent === '4 goals, 1 verified, 1 abandoned', 'summary line');
  check(env.text('hdr-goals') === '2 goals left' && !env.registry.get('hdr-goals').hasClass('hidden'), 'header chip counts goals that are not verified or abandoned');
  check(hasTextNode(env, 'Ship ' + IMG) && hasTextNode(env, 'Child ' + SCRIPT), 'goal titles are text nodes');
  check(allText(env).includes('[U+001B]') && allText(env).includes('[U+202E]') && !/[\u001b\u202e]/.test(allText(env)), 'ESC and bidi controls are made visible');
  check(hasTextNode(env, 'because [U+001B][31mred[U+001B][0m'), 'goal reason is text');
  assertSafe(env, 'goals');
  // depth indent
  const rows = find(env, (n) => n.hasClass('goalrow'));
  check(rows.length === 4 && rows[0].hasClass('indent-0') && rows[1].hasClass('indent-1'), 'tree rows are indented by depth');
  // verification badges
  const unv = find(env, (n) => n.hasClass('vf-unverified'));
  check(unv.length === 1 && unv[0].textContent === 'unverified', 'exactly one unverified badge: the governed task in review');
  check(find(env, (n) => n.hasClass('vf-verified')).length === 1, 'a verified badge for the verified task');
  check(find(env, (n) => n.hasClass('vf-failed')).length === 1, 'a failed badge');
  check(find(env, (n) => n.hasClass('vf-none')).length === 0, 'no badge for verification none');
  check(find(env, (n) => n.hasClass('ct-unknown')).length === 1, 'an unknown contract value gets the neutral class');
  // links
  const links = find(env, (n) => n.tagName === 'a' && n.hasClass('rlink'));
  const hrefs = links.map((a) => a.getAttribute('href'));
  check(hrefs.every((h) => HREF_OK.test(h)), 'every link is a validated hash route');
  check(hrefs.includes('#/goal/g1') && hrefs.includes('#/task/r1/api') && hrefs.includes('#/goal/g3'), 'goal and task links are built');
  check(!hrefs.some((h) => h.includes('bad') || h.includes('<') || h.includes(' ')), 'invalid ids are never linked');
  check(!links.some((a) => a.textContent.includes('Weird')), 'a goal with an invalid id is shown as plain text');
  check(hasTextNode(env, 'Weird ' + IMG), 'the unlinked title is still shown');
  check(hasTextNode(env, 'not a ref<x>'), 'an invalid task ref is shown as plain text');
  // remaining work
  check(allText(env).includes('Open leaf goals (1)') && allText(env).includes('Blockers (2)') && allText(env).includes('Next ready tasks (1)'), 'remaining work sections');
  check(hasTextNode(env, 'Blocked ' + IMG) && allText(env).includes('Do safe[U+202E]gnp.exe'), 'remaining texts are text');
  check(hrefs.filter((h) => h === '#/task/r1/ui').length >= 1, 'next ready task links to its task page');
  // read-only
  const goalCalls = env.calls.filter((c) => /^\/api\/(goal|goals|remaining|verify)/.test(c.url));
  check(goalCalls.length > 0 && goalCalls.every((c) => c.method === 'GET' && c.body === null), 'only GET requests are made for goals');
  check(!buttons(env, 'Resolve').length && !find(env, (n) => n.tagName === 'button' && /verify/i.test(n.textContent)).length, 'no verify or change button on the goals page');
  const goalsPolls = env.calls.filter((c) => c.url === '/api/goals').length;
  const statusPolls = env.statusCalls().length;
  check(goalsPolls === statusPolls, 'goals are fetched exactly once per status poll (' + goalsPolls + ' and ' + statusPolls + ')');
  await env.setVisible('hidden');
  const before = env.calls.length;
  await env.advance(10000);
  check(env.calls.length === before, 'no goal requests while the tab is hidden');
  await env.setVisible('visible');

  // the goal page
  env.location.hash = '#/goal/g1';
  await env.advance(100);
  check(env.registry.get('nav-goals').hasClass('active'), 'goals nav stays active on a goal page');
  check(env.calls.some((c) => c.url === '/api/goal?id=g1'), 'the goal is requested by id');
  check(hasTextNode(env, 'intent ' + SCRIPT + '\nsecond line [U+001B][31mred[U+001B][0m'), 'intent is multi line text');
  check(find(env, (n) => n.hasClass('multi') && n.textContent.startsWith('intent '))[0] !== undefined, 'intent keeps line breaks in a pre-wrap block');
  check(find(env, (n) => n.tagName === 'code' && n.textContent === CONTRACT_ID).length >= 1, 'full contract id shown');
  check(allText(env).includes('short form ' + CONTRACT_ID.slice(0, 12)), 'short contract id shown');
  const links2 = find(env, (n) => n.tagName === 'a').map((a) => a.getAttribute('href'));
  check(links2.includes('#/goals') && links2.includes('#/goal/g2') && links2.includes('#/goal/g3') && !links2.some((h) => h.includes('bad')), 'children link by validated id only');
  check(hasTextNode(env, 'bad id<x>'), 'an invalid child id is plain text');
  const checksTable = find(env, (n) => n.tagName === 'table');
  check(checksTable.length === 3, 'tasks, checks and evidence tables');
  const checkRows = checksTable[1].all((n) => n.tagName === 'tr').slice(1);
  check(checkRows.length === 2, 'one row per check');
  check(checkRows[0].textContent.includes('tests') && checkRows[0].textContent.includes('yes'), 'check row shows mandatory');
  check(checkRows[0].textContent.includes(COMMIT.slice(0, 12)) && !checkRows[0].textContent.includes(COMMIT), 'short commit in the checks table');
  check(checkRows[1].textContent.includes('no evidence'), 'a check without evidence says so');
  check(find(env, (n) => n.hasClass('vd-passed')).length >= 1 && find(env, (n) => n.hasClass('vd-failed')).length >= 1, 'verdict badges');
  const evRows = find(env, (n) => n.hasClass('evrow'));
  check(evRows.length === 3, 'three evidence rows');
  check(evRows[1].hasClass('dim') && !evRows[0].hasClass('dim'), 'evidence that does not count is dimmed');
  check(allText(env).includes('Evidence that does not count (dirty worktree or not the reviewed commit) is shown dimmed.'), 'dim note shown');
  check(find(env, (n) => n.hasClass('evdetail') && !n.hasClass('hidden')).length === 0, 'output rows start collapsed');
  evRows[1].dispatch('click');
  const evOut = find(env, (n) => n.hasClass('evout'))[0];
  check(!!evOut && evOut.textContent.length <= 20000 + 20 && evOut.textContent.length >= 20000, 'the output shown is bounded to 20000 characters (got ' + (evOut ? evOut.textContent.length : -1) + ')');
  check(evOut.textContent.startsWith('[U+001B]' ) && evOut.textContent.includes(IMG), 'the output is text with visible controls');
  check(allText(env).includes('Output cut: showing the first 20000 of'), 'a notice says the output was cut');
  check(find(env, (n) => n.tagName === 'code' && n.textContent === COMMIT).length === 1 && find(env, (n) => n.tagName === 'code' && n.textContent === TREE).length === 1, 'full commit and tree shown when a row is opened');
  check(find(env, (n) => n.hasClass('evdetail') && !n.hasClass('hidden')).length === 1, 'one row is open');
  await env.advance(2100);
  check(find(env, (n) => n.hasClass('evdetail') && !n.hasClass('hidden')).length === 1, 'an open row stays open after a poll');
  evRows[1].dispatch('click');
  check(find(env, (n) => n.hasClass('evdetail') && !n.hasClass('hidden')).length === 0, 'a second click closes the row');
  assertSafe(env, 'goal');
  const goalGets = env.calls.filter((c) => c.url.startsWith('/api/goal?'));
  check(goalGets.length > 0 && goalGets.every((c) => c.method === 'GET'), 'goal requests are GET');

  // an invalid id never reaches the server
  const n0 = env.calls.filter((c) => c.url.startsWith('/api/goal?')).length;
  env.location.hash = '#/goal/bad%20id%3Cx%3E';
  await env.advance(2100);
  check(allText(env).includes('Not a valid goal id'), 'an invalid goal id is refused');
  check(env.calls.filter((c) => c.url.startsWith('/api/goal?')).length === n0, 'an invalid goal id makes no request');

  // task page: goals and evidence
  env.location.hash = '#/task/r1/api';
  await env.advance(300);
  check(allText(env).includes('Goals and evidence'), 'task page has a goals section');
  const gcards = find(env, (n) => n.hasClass('govbox'))[0];
  check(gcards.textContent.includes('Ship ' + IMG), 'the governing goal is listed');
  check(gcards.all((n) => n.hasClass('vf-verified')).length === 1, 'the verification badge for this task');
  const govRows = gcards.all((n) => n.tagName === 'tr').slice(1);
  check(govRows.length === 2, 'one row per check');
  check(govRows[0].textContent.includes('passed') && govRows[0].textContent.includes('tests ok'), 'the newest evidence for this task is shown (e1 for r1/api, not the failed e2 of r1/web)');
  check(!govRows[0].textContent.includes('failed'), 'evidence of another task is not shown');
  check(govRows[1].textContent.includes('lint ok'), 'the second check shows its own evidence');
  assertSafe(env, 'task goals');
  env.location.hash = '#/task/r1/web';
  await env.advance(300);
  const gcards2 = find(env, (n) => n.hasClass('govbox'))[0];
  check(gcards2.all((n) => n.hasClass('vf-unverified')).length === 1 && gcards2.all((n) => n.hasClass('vf-verified')).length === 0, 'unverified for the task in review, no verified badge');
  check(gcards2.textContent.includes('failed'), 'the failed evidence of r1/web is shown for r1/web');
  env.location.hash = '#/task/r1/ui';
  await env.advance(300);
  check(find(env, (n) => n.hasClass('govbox'))[0].all((n) => n.hasClass('vf-unverified') || n.hasClass('vf-verified')).length === 0, 'no verification badge for a task whose verification is none');

  // run DAG badges
  env.location.hash = '#/run/r1';
  await env.advance(300);
  const rects = find(env, (n) => n.tagName === 'rect');
  check(rects.filter((r) => r.hasClass('vf-unverified')).length === 1, 'DAG: the unverified task has an unverified badge');
  check(rects.filter((r) => r.hasClass('vf-verified')).length === 1, 'DAG: the verified task has a verified badge');
  check(rects.filter((r) => /vf-(none|pending|failed)/.test(r.className)).length === 0, 'DAG: no badge for ungoverned or none');
  check(find(env, (n) => n.tagName === 'text' && n.textContent === 'unverified').length === 1, 'DAG: badge text');
  assertSafe(env, 'dag goals');
  check(env.unauth.length === 0 && violations.length === 0, 'goal requests carry the session header and use no unsafe API');
}

async function goalsBounds() {
  const env = await boot({ hash: '#/task/r1/api', stored: true, server: { goalCount: 11 } });
  await env.advance(500);
  const fetched = env.calls.filter((c) => c.url.startsWith('/api/goal?'));
  check(fetched.length === 5, 'at most five goals are fetched for a task (got ' + fetched.length + ')');
  check(find(env, (n) => n.hasClass('govbox'))[0].textContent.includes('at most 5 goals are expanded'), 'goals beyond five say they are not expanded');
  const env2 = await boot({ hash: '#/goals', stored: true, server: { goalCount: 700 } });
  await env2.advance(300);
  check(find(env2, (n) => n.hasClass('goalrow')).length === 500, 'the goal tree shows at most 500 goals');
  check(allText(env2).includes('200 more goals are not shown.'), 'the cut is announced');
}

async function goalsUnavailable() {
  const env = await boot({ hash: '#/goals', stored: true, server: { goalsStatus: 404 } });
  await env.advance(300);
  check(allText(env).includes('Goals are not available: goals unavailable'), 'a missing goals endpoint is explained');
  check(env.registry.get('hdr-goals').hasClass('hidden') || env.text('hdr-goals') === '', 'no header chip without goals');
  const env2 = await boot({ hash: '#/goals', stored: true, server: { goalsStatus: 401 } });
  await env2.advance(300);
  check(allText(env2) === NOT_LOGGED_IN && env2.storage.data.size === 0, 'a 401 on the goals request logs out and clears the session');
}

const LABEL_PEER = 'from task api (unverified peer message)';
const LABEL_OP = 'from operator';
const MSG_HOSTILE = IMG + ' ' + ESC + ' ' + BIDI;
const MSG_HOSTILE_SEEN = IMG + ' [U+001B][31mred[U+001B][0m safe[U+202E]gnp.exe';
const ODD_ID = 'x y&z=1';

async function messagesFlow() {
  const env = await boot({ hash: '#/messages', stored: true });
  await env.advance(100);
  check(env.registry.get('nav-messages').hasClass('active') && !env.registry.get('nav-goals').hasClass('active'), 'messages nav active');
  check(env.calls.some((c) => c.url === '/api/messages?limit=100'), 'the newest messages are requested with a limit');
  check(find(env, (n) => n.hasClass('goalsum'))[0].textContent === '4 messages, 2 channels', 'messages summary line');
  // channels: counts, MUTED badge with the reason
  const cards = find(env, (n) => n.hasClass('goalrow'));
  check(cards.length === 3, 'two channel cards and one muted task row');
  check(hasTextNode(env, 'dev ' + IMG) && hasTextNode(env, 'ops'), 'channel names are text nodes');
  const mutedBadges = find(env, (n) => n.hasClass('ms-muted'));
  check(mutedBadges.length === 2 && mutedBadges.every((b) => b.textContent === 'MUTED'), 'MUTED badges: one for the channel, one for the task');
  check(allText(env).includes('Muted: endpoint-ended ' + SCRIPT + ' (by operator)'), 'the mute reason and who muted are shown, as text');
  check(allText(env).includes('looping [U+001B][31mred[U+001B][0m (by operator)'), 'the muted task shows its reason');
  check(allText(env).includes('on channel talk: looping'), 'a task muted on one channel names the channel');
  check(allText(env).includes('queued 1') && allText(env).includes('awaiting approval 2') && allText(env).includes('delivered 3') && allText(env).includes('blocked 1'), 'channel message counts');
  check(find(env, (n) => n.hasClass('ms-awaiting-approval')).length >= 2, 'awaiting-approval badges keep their own class');
  // messages: label next to every row, BLOCKED badge with the reason
  const rows = find(env, (n) => n.hasClass('msgrow'));
  check(rows.length === 4, 'one row per message');
  check(hasTextNode(env, LABEL_PEER) && hasTextNode(env, LABEL_OP), 'the label of every message is shown');
  check(rows.every((r) => r.textContent.includes(LABEL_PEER) || r.textContent.includes(LABEL_OP)), 'each row carries its label');
  const blocked = find(env, (n) => n.hasClass('ms-blocked') && n.textContent === 'BLOCKED');
  check(blocked.length === 1, 'exactly one BLOCKED badge');
  check(rows[0].textContent.includes('BLOCKED') && rows[0].textContent.includes('secret-detected'), 'the BLOCKED row shows the reason');
  check(hasTextNode(env, MSG_HOSTILE_SEEN), 'a hostile subject is inert text with control characters made visible');
  check(!/[\u001b\u202e]/.test(allText(env)), 'no raw ESC or bidi control reaches the page');
  assertSafe(env, 'messages');
  check(find(env, (n) => n.tagName === 'a' && n.hasClass('rlink')).every((a) => HREF_OK.test(a.getAttribute('href'))), 'links are validated hash routes');
  check(find(env, (n) => n.tagName === 'button').length === 0, 'no button on the messages page');
  check(allText(env).includes('unverified peer content'), 'the page says peer content is unverified');

  // one message, with its label above the body and the warning between them
  rows[0].dispatch('click');
  await env.advance(100);
  check(env.calls.some((c) => c.url === '/api/message?id=m3'), 'a click asks for the message by id');
  const label = find(env, (n) => n.hasClass('msglabel'));
  check(label.length === 1 && label[0].textContent === LABEL_PEER, 'the label is shown above the body');
  const body = find(env, (n) => n.tagName === 'pre' && n.hasClass('msgbody'));
  check(body.length === 1 && body[0].textContent === 'body ' + SCRIPT + '\n[U+001B][31mred[U+001B][0m', 'the body is text, with controls made visible');
  const text = allText(env);
  const at = (s) => text.lastIndexOf(s);
  check(at(LABEL_PEER) < at('Unverified peer content') && at('Unverified peer content') < at('body ' + SCRIPT), 'label, then the unverified note, then the body');
  check(text.includes('Reason: secret-detected - matched a key shape'), 'the block reason and detail are shown in the message');
  assertSafe(env, 'one message');
  // a large body is cut and says so; an odd id is encoded
  rows[1].dispatch('click');
  await env.advance(100);
  const big = find(env, (n) => n.tagName === 'pre' && n.hasClass('msgbody'));
  check(big.length === 1 && big[0].textContent.length === 20000 && allText(env).includes('Body cut: showing the first 20000 of 30000 characters.'), 'a long body is cut and says so');
  rows[3].dispatch('click');
  await env.advance(100);
  check(env.calls.some((c) => c.url === '/api/message?id=' + encodeURIComponent(ODD_ID)), 'message ids are URL-encoded');
  // read only: the page only ever reads
  const msgCalls = env.calls.filter((c) => /^\/api\/messages?(\?|$)/.test(c.url));
  check(msgCalls.length > 0 && msgCalls.every((c) => c.method === 'GET' && c.body === null), 'only GET requests are made for messages');
  check(!env.calls.some((c) => /msg|message/.test(c.url) && c.method !== 'GET'), 'nothing is posted to a message route');
  check(!env.calls.some((c) => /\/api\/msg/.test(c.url)), 'no send, mute or unmute route is called');
  check(!buttons(env, 'Send').length && !buttons(env, 'Mute').length && !buttons(env, 'Unmute').length, 'no send, mute or unmute button');
  await env.setVisible('hidden');
  const before = env.calls.length;
  await env.advance(10000);
  check(env.calls.length === before, 'no message requests while the tab is hidden');
}

async function messagesUnavailable() {
  const env = await boot({ hash: '#/messages', stored: true, server: { messagesStatus: 404 } });
  await env.advance(300);
  check(allText(env).includes('Messages are not available: msgs unavailable'), 'a missing messages endpoint is explained');
  const env2 = await boot({ hash: '#/messages', stored: true, server: { messagesStatus: 401 } });
  await env2.advance(300);
  check(allText(env2) === NOT_LOGGED_IN && env2.storage.data.size === 0, 'a 401 on the messages request logs out and clears the session');
}

async function streamRetries() {
  const env = await boot({ hash: '#/task/r1/web', stored: true, server: { streamThrow: 1000 } });
  await env.advance(40000);
  const attempts = env.calls.filter((c) => c.url.startsWith('/api/stream'));
  check(attempts.length === 6, 'one attempt and five retries, then the stream gives up (got ' + attempts.length + ')');
  check(allText(env).includes('Live output stopped after 5 failed retries.'), 'giving up is shown');
  await env.advance(20000);
  check(env.calls.filter((c) => c.url.startsWith('/api/stream')).length === 6, 'no further stream attempts after giving up');
  check(env.unauth.length === 0, 'stream attempts carry the session header');
}

async function streamOverflow() {
  const env = await boot({ hash: '#/task/r1/web', stored: true });
  await env.advance(100);
  const s = env.streams[0];
  s.push('data: ' + 'x'.repeat(1100000));
  await env.advance(100);
  check(allText(env).includes('more than 1 MiB'), 'an oversized pending frame stops the stream with a notice');
  check(s.cancelled === true, 'the oversized stream is cancelled');
  await env.advance(10000);
  check(env.streams.length === 1, 'no reconnect after an oversized frame');
}

async function streamAuth() {
  const env = await boot({ hash: '#/task/r1/web', stored: true, server: { streamStatus: 401 } });
  await env.advance(100);
  check(allText(env) === NOT_LOGGED_IN, 'HTTP 401 on the stream shows the not-logged-in message');
  check(env.storage.data.size === 0, 'HTTP 401 on the stream clears the stored session');
  await env.advance(10000);
  check(env.calls.filter((c) => c.url.startsWith('/api/stream')).length === 1 && env.statusCalls().length <= 1, 'nothing is retried after a 401');
}

staticChecks();
await mainFlow();
await sessionFlow();
await notLoggedIn();
await rateLimited();
await nextRoute();
await goalsFlow();
await goalsBounds();
await goalsUnavailable();
await messagesFlow();
await messagesUnavailable();
await streamRetries();
await streamOverflow();
await streamAuth();
check(violations.length === 0, 'no unsafe DOM API was used: ' + violations.join(', '));
console.log('web-smoke: ok (' + checks + ' checks)');
