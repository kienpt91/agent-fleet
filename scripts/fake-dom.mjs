// A small standalone fake DOM, fake fetch and fake session storage for the pane tests.
// It records every element created and every attribute written so that tests can assert allowlists.

const CTRL_ONE = /[\u0000-\u001f\u007f-\u009f\u00ad\u061c\u200b\u200e\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]/g;
const CTRL_MULTI = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u00ad\u061c\u200b\u200e\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]/g;

export class FakeText {
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

export function createFakeDom() {
  const dom = {
    violations: [],
    created: [],
    attrWrites: [],
    scrolled: [],
    activeElement: null,
    classTokens: new Set()
  };

  class FakeElement {
    constructor(tag) {
      this.nodeType = 1;
      this.tagName = tag.toUpperCase();
      this.childNodes = [];
      this.parentNode = null;
      this.attrs = new Map();
      this.listeners = new Map();
      this.disabled = false;
      this.naturalWidth = 0;
      this.naturalHeight = 0;
    }
    get tag() {
      return this.tagName.toLowerCase();
    }
    get className() {
      return this.attrs.get('class') || '';
    }
    set className(v) {
      this.setAttribute('class', String(v));
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
      const name = String(k);
      dom.attrWrites.push({ el: this, name, value: String(v) });
      if (name === 'class') String(v).split(/\s+/).filter(Boolean).forEach((t) => dom.classTokens.add(t));
      this.attrs.set(name, String(v));
    }
    getAttribute(k) {
      return this.attrs.has(k) ? this.attrs.get(k) : null;
    }
    removeAttribute(k) {
      this.attrs.delete(k);
    }
    get textContent() {
      return this.childNodes.map((c) => c.textContent).join('');
    }
    set textContent(v) {
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
    get children() {
      return this.childNodes.filter((c) => c.nodeType === 1);
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
    contains(n) {
      for (let c = n; c; c = c.parentNode) {
        if (c === this) return true;
      }
      return false;
    }
    addEventListener(type, fn) {
      if (!this.listeners.has(type)) this.listeners.set(type, []);
      this.listeners.get(type).push(fn);
    }
    // dispatches with bubbling; returns the event object
    dispatch(type, extra) {
      const ev = Object.assign(
        {
          type,
          target: this,
          defaultPrevented: false,
          stopped: false,
          preventDefault() {
            this.defaultPrevented = true;
          },
          stopPropagation() {
            this.stopped = true;
          }
        },
        extra || {}
      );
      if (type === 'click' && (this.disabled || this.attrs.has('disabled'))) return ev;
      for (let n = this; n && !ev.stopped; n = n.parentNode) {
        (n.listeners ? n.listeners.get(type) || [] : []).slice().forEach((fn) => fn(ev));
      }
      return ev;
    }
    focus() {
      dom.activeElement = this;
    }
    scrollIntoView(opts) {
      dom.scrolled.push({ el: this, opts });
    }
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
    byClass(c) {
      return this.all((n) => n.classList.contains(c));
    }
    byTag(t) {
      return this.all((n) => n.tag === t);
    }
    byText(s, tag) {
      return this.all((n) => (!tag || n.tag === tag) && n.textContent === s);
    }
    hasClass(c) {
      return this.classList.contains(c);
    }
    click() {
      return this.dispatch('click');
    }
  }

  for (const prop of ['innerHTML', 'outerHTML']) {
    Object.defineProperty(FakeElement.prototype, prop, {
      get() {
        dom.violations.push('read ' + prop);
        return '';
      },
      set() {
        dom.violations.push('write ' + prop);
      }
    });
  }
  // property writes of URL-carrying attributes are recorded as attribute writes too
  for (const prop of ['href', 'src', 'srcdoc', 'id', 'title']) {
    Object.defineProperty(FakeElement.prototype, prop, {
      get() {
        return this.attrs.get(prop) || '';
      },
      set(v) {
        this.setAttribute(prop, String(v));
      }
    });
  }
  FakeElement.prototype.insertAdjacentHTML = function () {
    dom.violations.push('insertAdjacentHTML');
  };
  Object.defineProperty(FakeElement.prototype, 'style', {
    get() {
      dom.violations.push('style access');
      return {};
    }
  });

  dom.FakeElement = FakeElement;
  dom.document = {
    createElement(tag) {
      dom.created.push(String(tag).toLowerCase());
      return new FakeElement(String(tag));
    },
    createElementNS() {
      dom.violations.push('createElementNS');
      return new FakeElement('svg');
    },
    createTextNode(t) {
      return new FakeText(t);
    },
    get activeElement() {
      return dom.activeElement;
    },
    getElementById() {
      return null;
    },
    write() {
      dom.violations.push('document.write');
    },
    set cookie(v) {
      dom.violations.push('document.cookie write');
    },
    get cookie() {
      dom.violations.push('document.cookie read');
      return '';
    }
  };
  return dom;
}

export class FakeSessionStorage {
  constructor() {
    this.data = new Map();
    this.log = [];
  }
  getItem(k) {
    this.log.push(['get', k]);
    return this.data.has(k) ? this.data.get(k) : null;
  }
  setItem(k, v) {
    this.log.push(['set', k]);
    this.data.set(k, String(v));
  }
  removeItem(k) {
    this.log.push(['remove', k]);
    this.data.delete(k);
  }
}

// A fake fetch: routes is a function (url, init) => {status, body, headers?, bytes?} or a Promise of it.
// Records every call in `calls` and answers with a minimal Response-like object.
export function makeFetch(routes) {
  const calls = [];
  async function fetchFn(url, init) {
    calls.push({ url: String(url), init: init || {} });
    const r = await routes(String(url), init || {});
    const status = r.status === undefined ? 200 : r.status;
    const headers = new Map(Object.entries(r.headers || { 'content-type': 'application/json' }));
    return {
      ok: status >= 200 && status < 300,
      status,
      headers: { get: (k) => headers.get(String(k).toLowerCase()) ?? null },
      async json() {
        return r.body;
      },
      async arrayBuffer() {
        return r.bytes;
      }
    };
  }
  fetchFn.calls = calls;
  return fetchFn;
}

// A stub of window.fleetHost over a fake DOM and a fake fetch (same clean/el/tagged rules as app.js).
export function makeHost(dom, fetchFn, extra) {
  const mark = (c) => '[U+' + c.charCodeAt(0).toString(16).toUpperCase().padStart(4, '0') + ']';
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
  function el(tag, cls, text, multi) {
    const e = dom.document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined && text !== null) e.textContent = clean(text, { multi: !!multi, max: multi ? 20000 : 4000 });
    return e;
  }
  const host = {
    el,
    clean,
    short: (v, n) => {
      const s = clean(v);
      return s.length > n ? s.slice(0, n - 1) + '\u2026' : s;
    },
    num: (v) => (typeof v === 'number' && Number.isFinite(v) ? String(v) : '?'),
    enc: (s) => encodeURIComponent(String(s)),
    badge: (s) => el('span', 'badge st-' + s, s),
    tagged: (prefix, set, value, label) => {
      const known = typeof value === 'string' && set.has(value);
      return el('span', 'badge ' + prefix + '-' + (known ? value : 'unknown'), label === undefined ? value : label);
    },
    shortHash: (v) => String(v).slice(0, 8),
    fmtTime: (ms) => String(ms),
    async api(method, path) {
      const res = await fetchFn(path, { method, headers: { 'X-Fleet-Session': 'x' }, credentials: 'omit' });
      let data = null;
      try {
        data = await res.json();
      } catch (e) {
        data = null;
      }
      return { ok: res.ok, status: res.status, data, networkError: false, retryAfter: null };
    },
    async apiBlob(path) {
      const res = await fetchFn(path, { method: 'GET', credentials: 'omit' });
      if (res.ok) {
        return { ok: true, status: res.status, type: res.headers.get('content-type') || '', bytes: await res.arrayBuffer(), data: null };
      }
      let data = null;
      try {
        data = await res.json();
      } catch (e) {
        data = null;
      }
      return { ok: false, status: res.status, type: '', bytes: null, data };
    },
    errText(res) {
      const m = res && res.data && res.data.error && res.data.error.message;
      return typeof m === 'string' ? m : 'HTTP ' + (res && res.status);
    },
    unauthorized() {
      host.unauthorizedCalls += 1;
    },
    unauthorizedCalls: 0,
    status: () => null,
    goals: () => null,
    taskInfo: () => null,
    taskVerification: async () => ({ state: 'none', dirty: null, commit: null }),
    go() {}
  };
  return Object.assign(host, extra || {});
}

// Settles all pending promise chains (the panes only chain microtasks).
export async function settle(times) {
  for (let i = 0; i < (times || 40); i += 1) {
    await new Promise((r) => setImmediate(r));
  }
}
