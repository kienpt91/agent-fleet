'use strict';
/* fleet workbench panes: explorer, file viewer, diff viewer, markdown preview, image viewer. Text is shown through textContent only. */
(function () {
  if (typeof window === 'undefined' || !window.fleetHost || !window.fleetDock || typeof window.fleetDock.registerPane !== 'function') {
    return;
  }
  const host = window.fleetHost;
  const dock = window.fleetDock;
  const el = host.el;
  const clean = host.clean;

  // ------------------------------------------------------------ constants

  const REF_RE = /^[A-Za-z0-9._-]{1,40}\/[A-Za-z0-9._-]{1,40}$/;
  const COMMIT_RE = /^(?:head|base|[0-9a-f]{40}|[0-9a-f]{64})$/;
  const SHA_RE = /^[0-9a-f]{40}(?:[0-9a-f]{24})?$/;
  const BAD_PATH_RE = /[\u0000-\u001f\u007f\\]/;
  const URL_RE = /^https?:\/\/[^\s\u0000-\u001f\u007f-\u009f\u00ad\u061c\u200b-\u200f\u2028-\u202e\u2060-\u2069\ufeff]+$/i;
  const MAX_PATH = 1024;
  const MAX_URL = 2048;
  const VF = new Set(['none', 'pending', 'unverified', 'failed', 'verified']);
  const FILE_STATUS = new Set(['added', 'modified', 'deleted', 'renamed', 'copied', 'mode_changed', 'type_changed']);
  const IMG_TYPES = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp']);
  const KIND_LABEL = { dir: 'dir', file: 'file', symlink: 'link', submodule: 'module' };
  const MAX_IMAGE = 4 * 1024 * 1024;
  const B64_STEP = 24576;
  const CHUNK_LINES = 500;
  const HARD_LINES = 10000;
  const MAX_TREE_ENTRIES = 2000;
  const MAX_TREE_ROWS = 4000;
  const DIFF_CHUNK = 3000;
  const MAX_DIFF_FILES = 2000;
  const MAX_DIFF_LINES = 50000;
  const MAX_LINE_CHARS = 4000;
  const KEEP_CONTEXT = 3;
  const MD_MAX_DEPTH = 40;
  const MD_MAX_NODES = 20000;
  const MD_MAX_IMAGES = 20;
  const MD_TAGS = new Set(['h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'p', 'pre', 'code', 'blockquote', 'ul', 'ol', 'li', 'table', 'thead', 'tbody', 'tr', 'th', 'td', 'hr', 'em', 'strong', 'del', 'br', 'a', 'img', 'span', 'input']);
  const HEADS = ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'];
  const ALIGN_CLASS = { left: 'md-al-left', center: 'md-al-center', right: 'md-al-right' };

  // -------------------------------------------------------------- helpers

  function isObj(x) {
    return x !== null && typeof x === 'object' && !Array.isArray(x);
  }

  function intOrNull(v) {
    return typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 9007199254740991 ? v : null;
  }

  function validPath(p, allowEmpty) {
    if (typeof p !== 'string' || p.length > MAX_PATH || BAD_PATH_RE.test(p)) {
      return false;
    }
    return allowEmpty || p.length > 0;
  }

  function lifecycle() {
    let gen = 0;
    let dead = false;
    return {
      gen() {
        return gen;
      },
      next() {
        gen += 1;
        return gen;
      },
      ok(g) {
        return !dead && g === gen;
      },
      kill() {
        dead = true;
        gen += 1;
      }
    };
  }

  function bad(error) {
    return { ok: false, error };
  }

  function normTarget(raw, needPath) {
    const r = isObj(raw) ? raw : {};
    const task = r.task;
    const commit = r.commit === undefined || r.commit === null ? 'head' : r.commit;
    const path = r.path === undefined || r.path === null ? '' : r.path;
    if (typeof task !== 'string' || !REF_RE.test(task)) {
      return bad('Invalid task reference (expected run/task).');
    }
    if (typeof commit !== 'string' || !COMMIT_RE.test(commit)) {
      return bad('Invalid commit selector (expected head, base or a full commit id).');
    }
    if (needPath && path === '') {
      return bad('No file selected. Pick a file in the explorer.');
    }
    if (!validPath(path, !needPath)) {
      return bad('Invalid path.');
    }
    let line = null;
    if (r.line !== undefined && r.line !== null) {
      const n = typeof r.line === 'string' && /^[0-9]{1,7}$/.test(r.line) ? Number(r.line) : r.line;
      if (Number.isInteger(n) && n >= 1 && n <= 10000000) {
        line = n;
      }
    }
    return { ok: true, v: { task, commit, path, line } };
  }

  function targetQuery(task, commit, path) {
    return 'task=' + encodeURIComponent(task) + '&commit=' + encodeURIComponent(commit) + '&path=' + encodeURIComponent(path);
  }

  function basename(path) {
    const s = typeof path === 'string' ? path : '';
    const i = s.lastIndexOf('/');
    return i >= 0 ? s.slice(i + 1) : s;
  }

  function isMarkdownName(name) {
    return /\.(?:md|markdown)$/i.test(name);
  }

  function isImageName(name) {
    return /\.(?:png|jpe?g|gif|webp)$/i.test(name);
  }

  function shortSha(v) {
    return typeof v === 'string' && SHA_RE.test(v) ? v.slice(0, 10) : '';
  }

  function fmtBytes(n) {
    if (typeof n !== 'number' || !Number.isFinite(n) || n < 0) {
      return '? bytes';
    }
    if (n < 1024) {
      return n + ' bytes';
    }
    const units = ['KiB', 'MiB', 'GiB'];
    let v = n / 1024;
    let u = 0;
    while (v >= 1024 && u < units.length - 1) {
      v /= 1024;
      u += 1;
    }
    return v.toFixed(1) + ' ' + units.at(u) + ' (' + n + ' bytes)';
  }

  function button(label, cls, onClick, title) {
    const b = el('button', 'btn small' + (cls ? ' ' + cls : ''), label);
    b.setAttribute('type', 'button');
    if (title) {
      b.setAttribute('title', clean(title, { max: 300 }));
    }
    if (onClick) {
      b.addEventListener('click', onClick);
    }
    return b;
  }

  function setPressed(b, on) {
    b.setAttribute('aria-pressed', on ? 'true' : 'false');
    b.classList.toggle('active', !!on);
  }

  function notice(cls, text) {
    const d = el('div', 'pane-notice ' + cls, text, true);
    d.setAttribute('role', cls === 'bad' ? 'alert' : 'status');
    return d;
  }

  function isButton(t) {
    return !!t && typeof t.tagName === 'string' && t.tagName.toLowerCase() === 'button';
  }

  function isTextInput(t) {
    const n = t && typeof t.tagName === 'string' ? t.tagName.toLowerCase() : '';
    return n === 'input' || n === 'select' || n === 'textarea';
  }

  function stop(e) {
    if (e && typeof e.stopPropagation === 'function') {
      e.stopPropagation();
    }
  }

  function prevent(e) {
    if (e && typeof e.preventDefault === 'function') {
      e.preventDefault();
    }
  }

  function open(ctx, kind, params, opts) {
    if (ctx && typeof ctx.open === 'function') {
      ctx.open(kind, params, opts);
    }
  }

  function setTitle(ctx, text) {
    if (ctx && typeof ctx.setTitle === 'function') {
      ctx.setTitle(clean(text, { max: 80 }));
    }
  }

  function errorOf(res) {
    try {
      const t = typeof host.errText === 'function' ? host.errText(res) : '';
      return clean(t || 'HTTP ' + (res && res.status), { max: 600 });
    } catch (e) {
      return 'request failed';
    }
  }

  async function getDoc(path, schema) {
    let res;
    try {
      res = await host.api('GET', path);
    } catch (e) {
      return { err: { status: 0, text: 'request failed' } };
    }
    if (!res) {
      return { err: { status: 0, text: 'request failed' } };
    }
    if (res.status === 401 && typeof host.unauthorized === 'function') {
      host.unauthorized();
    }
    if (!res.ok) {
      return { err: { status: res.status, text: errorOf(res) } };
    }
    if (!isObj(res.data) || res.data.schema !== schema) {
      return { err: { status: res.status, text: 'unexpected response from the daemon' } };
    }
    return { doc: res.data };
  }

  function errNotice(err, commit) {
    if (err.status === 404 && commit === 'head') {
      return notice('info', 'No commit is recorded for this task yet. ' + err.text);
    }
    return notice('bad', err.text);
  }

  // ---------------------------------------------------------------- header

  // selector: 'single' = head/base buttons; 'diff' = base and head labels
  function makeHeader(label, selector, onCommit) {
    const node = el('div', 'pane-head');
    const row = el('div', 'pane-head-row');
    const taskEl = el('span', 'pane-task mono');
    const sel = el('span', 'pane-commits');
    const sha = el('span', 'pane-sha mono');
    const badges = el('span', 'pane-badges');
    const btns = new Map();
    const custom = el('span', 'pane-custom mono');
    let vgen = 0;
    row.appendChild(el('span', 'pane-kind', label));
    row.appendChild(taskEl);
    if (selector === 'single') {
      for (const c of ['head', 'base']) {
        const b = button(c, 'seg', () => {
          if (onCommit) {
            onCommit(c);
          }
        });
        btns.set(c, b);
        sel.appendChild(b);
      }
      sel.appendChild(custom);
    }
    row.appendChild(sel);
    row.appendChild(sha);
    row.appendChild(badges);
    node.appendChild(row);
    node.appendChild(el('div', 'pane-note muted', 'committed state of the repository, not the worktree'));

    function verification(ref) {
      vgen += 1;
      const g = vgen;
      badges.replaceChildren();
      if (typeof host.taskVerification !== 'function') {
        return;
      }
      let pr;
      try {
        pr = host.taskVerification(ref);
      } catch (e) {
        return;
      }
      const show = (v) => {
        if (g !== vgen) {
          return;
        }
        badges.replaceChildren();
        const state = isObj(v) && VF.has(v.state) ? v.state : 'unknown';
        const text = state === 'none' ? 'unverified' : state === 'unknown' ? 'verification unknown' : state;
        const b = host.tagged('vf', VF, state, text);
        b.setAttribute('title', 'verification: ' + text);
        badges.appendChild(b);
        if (isObj(v) && v.dirty === true) {
          const d = el('span', 'badge pane-dirty', 'dirty');
          d.setAttribute('title', 'the evidence was recorded from a dirty worktree');
          badges.appendChild(d);
        }
      };
      Promise.resolve(pr).then(show, () => show(null));
    }

    return {
      node,
      setTask(ref) {
        taskEl.textContent = clean(ref, { max: 100 });
        verification(ref);
      },
      setCommit(c) {
        for (const [k, b] of btns) {
          setPressed(b, k === c);
        }
        custom.textContent = c === 'head' || c === 'base' ? '' : 'commit ' + (shortSha(c) || '?');
      },
      setSelector(base, head) {
        sel.textContent = 'base ' + clean(base, { max: 12 }) + ' to head ' + clean(head, { max: 12 });
      },
      setShas(list) {
        const parts = list.map((s) => shortSha(s)).filter((s) => s !== '');
        if (list.length === 2) {
          sha.textContent = parts.length === 2 ? 'base ' + parts.at(0) + ' \u2192 head ' + parts.at(1) : '';
        } else {
          sha.textContent = parts.length ? 'commit ' + parts.at(0) : '';
        }
      }
    };
  }

  // ---------------------------------------------------------------- images

  function toBase64(bytes) {
    const parts = [];
    for (let i = 0; i < bytes.length; i += B64_STEP) {
      parts.push(btoa(String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + B64_STEP, bytes.length)))));
    }
    return parts.join('');
  }

  // Loads a repository image through the authenticated blob API and returns a data: URL, never a remote or object URL.
  async function loadImageData(task, commit, path) {
    let res;
    try {
      res = await host.apiBlob('/api/repo/blob?' + targetQuery(task, commit, path));
    } catch (e) {
      return { ok: false, reason: 'request failed' };
    }
    if (!res) {
      return { ok: false, reason: 'request failed' };
    }
    if (!res.ok) {
      if (res.status === 404) {
        return { ok: false, reason: 'not found' };
      }
      if (res.status === 413) {
        return { ok: false, reason: 'too large (limit 4 MiB)' };
      }
      if (res.status === 415) {
        return { ok: false, reason: 'not an image' };
      }
      return { ok: false, reason: errorOf(res) };
    }
    const type = String(res.type === undefined || res.type === null ? '' : res.type).split(';').at(0).trim().toLowerCase();
    if (!IMG_TYPES.has(type)) {
      return { ok: false, reason: 'not an image' };
    }
    const b = res.bytes;
    let view = null;
    if (Object.prototype.toString.call(b) === '[object ArrayBuffer]') {
      view = new Uint8Array(b);
    } else if (ArrayBuffer.isView(b)) {
      view = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
    }
    if (!view || view.length === 0) {
      return { ok: false, reason: 'no image data' };
    }
    if (view.length > MAX_IMAGE) {
      return { ok: false, reason: 'too large (limit 4 MiB)' };
    }
    let b64;
    try {
      b64 = toBase64(view);
    } catch (e) {
      return { ok: false, reason: 'could not encode the image' };
    }
    return { ok: true, url: 'data:' + type + ';base64,' + b64, type, size: view.length };
  }

  function imageEl(url, alt, cls) {
    const img = el('img', cls);
    img.setAttribute('src', url);
    img.setAttribute('alt', clean(alt, { max: 300 }));
    return img;
  }

  function imageView(v, life, g, altText) {
    const box = el('div', 'img-view');
    const bar = el('div', 'pane-tools');
    const info = el('span', 'muted', 'Loading image...');
    const dims = el('span', 'muted');
    const stage = el('div', 'img-stage');
    const fit = button('Fit to width', '', () => {
      const on = !stage.classList.contains('fit');
      stage.classList.toggle('fit', on);
      setPressed(fit, on);
    });
    setPressed(fit, false);
    bar.appendChild(fit);
    bar.appendChild(info);
    bar.appendChild(dims);
    box.appendChild(bar);
    box.appendChild(stage);
    loadImageData(v.task, v.commit, v.path).then((r) => {
      if (!life.ok(g)) {
        return;
      }
      if (!r.ok) {
        info.textContent = '';
        stage.appendChild(notice('bad', 'Image not shown: ' + r.reason));
        return;
      }
      const img = imageEl(r.url, altText, 'pane-img');
      img.addEventListener('load', () => {
        const w = img.naturalWidth;
        const h = img.naturalHeight;
        if (Number.isInteger(w) && Number.isInteger(h) && w > 0 && h > 0) {
          dims.textContent = w + ' \u00d7 ' + h + ' px';
        }
      });
      stage.appendChild(img);
      info.textContent = r.type + ', ' + fmtBytes(r.size);
    });
    return box;
  }

  // -------------------------------------------------------------- explorer

  function normTree(doc) {
    const list = Array.isArray(doc.entries) ? doc.entries : [];
    const entries = [];
    for (let i = 0; i < list.length && entries.length < MAX_TREE_ENTRIES; i += 1) {
      const e = list.at(i);
      if (!isObj(e)) {
        continue;
      }
      entries.push({
        name: typeof e.name === 'string' ? e.name : '?',
        path: validPath(e.path, false) ? e.path : null,
        kind: typeof e.kind === 'string' && Object.prototype.hasOwnProperty.call(KIND_LABEL, e.kind) ? e.kind : 'other',
        size: typeof e.size === 'number' && Number.isFinite(e.size) ? e.size : null,
        mode: typeof e.mode === 'string' ? e.mode : ''
      });
    }
    return { entries, extra: list.length > entries.length, truncated: doc.truncated === true, limit: intOrNull(doc.limit), commit: doc.commit };
  }

  function explorerPane(ctx, raw) {
    const life = lifecycle();
    let p = null;
    let cache = new Map();
    let errors = new Map();
    let pending = new Set();
    let expanded = new Set();
    let rootPath = '';
    let active = null;
    let rows = [];
    const node = el('div', 'pane pane-explorer');
    const crumbs = el('nav', 'pane-crumbs');
    crumbs.setAttribute('aria-label', 'Path');
    const tools = el('div', 'pane-tools');
    const msg = el('div', 'pane-msgs');
    const tree = el('div', 'tree');
    tree.setAttribute('role', 'tree');
    tree.setAttribute('aria-label', 'Repository tree');
    const head = makeHeader('Explorer', 'single', (c) => {
      if (p && p.ok && c !== p.v.commit) {
        init({ task: p.v.task, commit: c, path: rootPath });
      }
    });
    tools.appendChild(
      button('Diff', '', () => {
        if (p && p.ok) {
          open(ctx, 'diff', { task: p.v.task, base: 'base', head: 'head' }, { reuse: 'diff' });
        }
      }, 'Open the diff of this task')
    );

    function treeHasFocus() {
      const a = typeof document !== 'undefined' ? document.activeElement : null;
      return !!a && typeof tree.contains === 'function' && tree.contains(a);
    }

    function entryIndex(delta, from) {
      let i = from + delta;
      while (i >= 0 && i < rows.length) {
        if (rows.at(i).type === 'entry') {
          return i;
        }
        i += delta;
      }
      return -1;
    }

    function activeIndex() {
      let i = rows.findIndex((r) => r.type === 'entry' && r.key === active);
      if (i < 0) {
        i = entryIndex(1, -1);
      }
      return i;
    }

    function setActive(i, focus) {
      const r = rows.at(i);
      if (!r || r.type !== 'entry') {
        return;
      }
      for (const o of rows) {
        if (o.el) {
          o.el.setAttribute('tabindex', '-1');
          o.el.setAttribute('aria-selected', 'false');
        }
      }
      active = r.key;
      if (r.el) {
        r.el.setAttribute('tabindex', '0');
        r.el.setAttribute('aria-selected', 'true');
        if (focus && typeof r.el.focus === 'function') {
          r.el.focus();
        }
      }
    }

    function loadDir(path) {
      if (cache.has(path) || pending.has(path)) {
        return;
      }
      const g = life.gen();
      pending.add(path);
      errors.delete(path);
      getDoc('/api/repo/tree?' + targetQuery(p.v.task, p.v.commit, path), 'fleet.repo.tree/1').then((r) => {
        if (!life.ok(g)) {
          return;
        }
        pending.delete(path);
        if (r.err) {
          errors.set(path, r.err);
        } else {
          const t = normTree(r.doc);
          cache.set(path, t);
          if (path === rootPath) {
            head.setShas([t.commit]);
            setTitle(ctx, 'Explorer ' + p.v.task);
          }
        }
        render();
      });
    }

    function collect(dir, depth) {
      const d = cache.get(dir);
      if (!d) {
        if (errors.has(dir)) {
          rows.push({ type: 'note', depth, cls: 'bad', text: 'Could not load this directory: ' + errors.get(dir).text + ' (collapse and expand to retry)' });
        } else {
          rows.push({ type: 'note', depth, cls: '', text: 'Loading...' });
        }
        return;
      }
      for (let i = 0; i < d.entries.length; i += 1) {
        if (rows.length >= MAX_TREE_ROWS) {
          return;
        }
        const e = d.entries.at(i);
        const row = { type: 'entry', depth, e, key: e.path === null ? 'n:' + dir + '#' + i : 'p:' + e.path, el: null };
        rows.push(row);
        if (e.kind === 'dir' && e.path !== null && expanded.has(e.path)) {
          collect(e.path, depth + 1);
        }
      }
      if (d.truncated || d.extra) {
        rows.push({
          type: 'note',
          depth,
          cls: 'warn',
          text: 'Truncated: this directory has more entries than are listed' + (d.limit === null ? '.' : ' (limit ' + d.limit + ').')
        });
      }
    }

    function openEntry(r, preview) {
      const e = r.e;
      if (e.path === null || e.kind !== 'file') {
        return;
      }
      const params = { task: p.v.task, commit: p.v.commit, path: e.path };
      if (preview && isMarkdownName(e.name)) {
        open(ctx, 'markdown', params, { reuse: 'markdown' });
      } else if (isImageName(e.name)) {
        open(ctx, 'image', params, { reuse: 'image' });
      } else {
        open(ctx, 'file', params, { reuse: 'file' });
      }
    }

    function expand(r) {
      expanded.add(r.e.path);
      errors.delete(r.e.path);
      loadDir(r.e.path);
      active = r.key;
      render(true);
    }

    function collapse(r) {
      expanded.delete(r.e.path);
      active = r.key;
      render(true);
    }

    function activate(r) {
      if (r.e.kind === 'dir' && r.e.path !== null) {
        if (expanded.has(r.e.path)) {
          collapse(r);
        } else {
          expand(r);
        }
      } else {
        openEntry(r, false);
      }
    }

    function rowEl(r, i) {
      const d = el('div', 'tree-row depth-' + Math.min(r.depth, 12) + (r.type === 'note' ? ' tree-note ' + r.cls : ''));
      d.setAttribute('role', 'treeitem');
      d.setAttribute('aria-level', String(r.depth + 1));
      d.setAttribute('tabindex', '-1');
      if (r.type === 'note') {
        d.setAttribute('aria-disabled', 'true');
        d.appendChild(el('span', 'tree-name', r.text));
        return d;
      }
      const e = r.e;
      const kind = KIND_LABEL[e.kind] || 'other';
      d.classList.add('kind-' + (KIND_LABEL[e.kind] ? e.kind : 'other'));
      d.appendChild(el('span', 'tree-kind', kind));
      d.appendChild(el('span', 'tree-name', e.name));
      if (e.kind === 'symlink') {
        d.appendChild(el('span', 'tree-meta muted', 'symbolic link, not followed'));
        d.classList.add('tree-inert');
      } else if (e.kind === 'submodule') {
        d.appendChild(el('span', 'tree-meta muted', 'submodule, not opened'));
        d.classList.add('tree-inert');
      } else if (e.kind === 'file' && e.size !== null) {
        d.appendChild(el('span', 'tree-meta muted', fmtBytes(e.size)));
      }
      if (e.path === null) {
        d.setAttribute('aria-disabled', 'true');
        d.setAttribute('title', 'This name cannot be opened');
        d.classList.add('tree-disabled');
        d.appendChild(el('span', 'tree-meta muted', 'not addressable'));
      } else if (e.kind === 'dir') {
        d.setAttribute('aria-expanded', expanded.has(e.path) ? 'true' : 'false');
      } else if (e.kind === 'file' && isMarkdownName(e.name)) {
        d.appendChild(
          button('Preview', 'tree-action', (ev) => {
            stop(ev);
            openEntry(r, true);
          }, 'Preview as Markdown')
        );
      }
      d.addEventListener('click', (ev) => {
        if (isButton(ev && ev.target)) {
          return;
        }
        setActive(i, true);
        if (e.path !== null) {
          activate(r);
        }
      });
      return d;
    }

    function render(focus) {
      const had = focus === true || treeHasFocus();
      msg.replaceChildren();
      tree.replaceChildren();
      rows = [];
      const rootErr = errors.get(rootPath);
      if (rootErr) {
        msg.appendChild(errNotice(rootErr, p.v.commit));
        return;
      }
      if (!cache.has(rootPath)) {
        msg.appendChild(notice('info', 'Loading...'));
        return;
      }
      collect(rootPath, 0);
      if (rows.length >= MAX_TREE_ROWS) {
        msg.appendChild(notice('warn', 'Showing the first ' + MAX_TREE_ROWS + ' rows only; collapse some directories.'));
      }
      if (rows.length === 0) {
        msg.appendChild(notice('info', 'This directory is empty.'));
        return;
      }
      rows.forEach((r, i) => {
        r.el = rowEl(r, i);
        tree.appendChild(r.el);
      });
      const i = activeIndex();
      if (i >= 0) {
        setActive(i, had);
      }
    }

    function renderCrumbs() {
      crumbs.replaceChildren();
      const root = button('repository root', 'crumb', () => setRoot(''));
      crumbs.appendChild(root);
      const segs = rootPath === '' ? [] : rootPath.split('/');
      let acc = '';
      for (const s of segs) {
        acc = acc === '' ? s : acc + '/' + s;
        const target = acc;
        crumbs.appendChild(el('span', 'crumb-sep muted', '/'));
        crumbs.appendChild(button(s, 'crumb', () => setRoot(target)));
      }
    }

    function setRoot(path) {
      rootPath = path;
      active = null;
      renderCrumbs();
      loadDir(path);
      render();
    }

    tree.addEventListener('keydown', (e) => {
      if (!e || e.ctrlKey || e.metaKey || e.altKey || isButton(e.target)) {
        return;
      }
      const i = activeIndex();
      if (i < 0) {
        return;
      }
      const r = rows.at(i);
      const isDir = r.e.kind === 'dir' && r.e.path !== null;
      let handled = true;
      switch (e.key) {
        case 'ArrowDown':
          setActive(entryIndex(1, i), true);
          break;
        case 'ArrowUp':
          setActive(entryIndex(-1, i), true);
          break;
        case 'Home':
          setActive(entryIndex(1, -1), true);
          break;
        case 'End':
          setActive(entryIndex(-1, rows.length), true);
          break;
        case 'ArrowRight':
          if (isDir) {
            if (!expanded.has(r.e.path)) {
              expand(r);
            } else {
              setActive(entryIndex(1, i), true);
            }
          }
          break;
        case 'ArrowLeft':
          if (isDir && expanded.has(r.e.path)) {
            collapse(r);
          } else {
            for (let j = i - 1; j >= 0; j -= 1) {
              if (rows.at(j).type === 'entry' && rows.at(j).depth < r.depth) {
                setActive(j, true);
                break;
              }
            }
          }
          break;
        case 'Enter':
        case ' ':
          activate(r);
          break;
        case 'p':
        case 'P':
          openEntry(r, true);
          break;
        default:
          handled = false;
      }
      if (handled) {
        prevent(e);
      }
    });

    function init(params) {
      p = normTarget(params, false);
      life.next();
      cache = new Map();
      errors = new Map();
      pending = new Set();
      expanded = new Set();
      active = null;
      rows = [];
      if (!p.ok) {
        node.replaceChildren(notice('bad', p.error));
        return;
      }
      rootPath = p.v.path;
      head.setTask(p.v.task);
      head.setCommit(p.v.commit);
      head.setShas([]);
      node.replaceChildren(head.node, tools, crumbs, msg, tree);
      renderCrumbs();
      loadDir(rootPath);
      render();
    }

    init(raw);
    return {
      node,
      setParams: init,
      stop() {
        life.kill();
      }
    };
  }

  // ------------------------------------------------------------------ file

  function codeRow(n, text, hl) {
    const r = el('div', 'code-row' + (hl ? ' hl' : ''));
    r.appendChild(el('span', 'code-no', String(n)));
    r.appendChild(el('span', 'code-text', typeof text === 'string' ? text : '', true));
    if (hl) {
      r.setAttribute('aria-current', 'true');
    }
    return r;
  }

  function codeView(lines, hl) {
    const wrap = el('div', 'code-view');
    wrap.setAttribute('tabindex', '0');
    wrap.setAttribute('aria-label', 'File contents');
    const box = el('div', 'code-rows');
    const more = el('div', 'code-more');
    wrap.appendChild(box);
    wrap.appendChild(more);
    const total = Math.min(lines.length, HARD_LINES);
    let shown = 0;
    let target = null;

    function addUpTo(limit) {
      const to = Math.min(limit, total);
      for (; shown < to; shown += 1) {
        const mark = hl !== null && shown + 1 === hl;
        const row = codeRow(shown + 1, lines.at(shown), mark);
        if (mark) {
          target = row;
        }
        box.appendChild(row);
      }
      more.replaceChildren();
      if (shown < total) {
        const left = total - shown;
        more.appendChild(
          button('Show more (' + Math.min(CHUNK_LINES, left) + ' of ' + left + ' remaining lines)', '', () => {
            addUpTo(shown + CHUNK_LINES);
          })
        );
      } else if (lines.length > HARD_LINES) {
        more.appendChild(notice('warn', 'Only the first ' + HARD_LINES + ' lines are rendered.'));
      }
    }

    let first = CHUNK_LINES;
    if (hl !== null && hl > first) {
      first = Math.ceil(hl / CHUNK_LINES) * CHUNK_LINES;
    }
    addUpTo(first);
    if (hl !== null && hl > HARD_LINES) {
      more.appendChild(notice('warn', 'Line ' + hl + ' is beyond the rendered limit.'));
    }
    if (target && typeof target.scrollIntoView === 'function') {
      target.scrollIntoView({ block: 'center' });
    }
    return wrap;
  }

  function filePane(ctx, raw) {
    const life = lifecycle();
    let p = null;
    const node = el('div', 'pane pane-file');
    const meta = el('div', 'pane-tools');
    const msgs = el('div', 'pane-msgs');
    const body = el('div', 'pane-body');
    const head = makeHeader('File', 'single', (c) => {
      if (p && p.ok && c !== p.v.commit) {
        init({ task: p.v.task, commit: c, path: p.v.path, line: p.v.line });
      }
    });

    function show(doc) {
      const v = p.v;
      head.setShas([doc.commit]);
      setTitle(ctx, basename(v.path));
      const size = typeof doc.size === 'number' ? doc.size : null;
      meta.appendChild(el('span', 'pane-path mono', v.path));
      if (size !== null) {
        meta.appendChild(el('span', 'muted', fmtBytes(size)));
      }
      if (typeof doc.mode === 'string' && doc.mode !== 'regular') {
        meta.appendChild(el('span', 'muted', 'mode ' + doc.mode));
      }
      if (doc.markdown === true) {
        meta.appendChild(
          button('Preview', '', () => open(ctx, 'markdown', { task: v.task, commit: v.commit, path: v.path }, { reuse: 'markdown' }), 'Preview as Markdown')
        );
      }
      if (typeof doc.notice === 'string' && doc.notice !== '') {
        msgs.appendChild(notice('info', doc.notice));
      }
      const view = doc.view;
      if (view === 'text') {
        const lines = Array.isArray(doc.lines) ? doc.lines : [];
        const count = intOrNull(doc.line_count);
        if (doc.truncated === true) {
          msgs.appendChild(notice('warn', 'Truncated: showing the first ' + lines.length + ' of ' + (count === null ? '?' : count) + ' lines.'));
        }
        const clipped = intOrNull(doc.clipped);
        if (clipped !== null && clipped > 0) {
          msgs.appendChild(notice('warn', clipped + ' very long line(s) were clipped (marked in the text).'));
        }
        if (doc.lossy === true) {
          msgs.appendChild(notice('warn', 'The file is not valid UTF-8; invalid bytes were replaced.'));
        }
        if (doc.crlf === true) {
          msgs.appendChild(notice('info', 'The file uses CRLF line endings.'));
        }
        if (lines.length === 0) {
          body.appendChild(notice('info', 'The file is empty.'));
        } else {
          body.appendChild(codeView(lines, v.line));
        }
      } else if (view === 'binary') {
        body.appendChild(notice('info', 'Binary file, not shown. ' + fmtBytes(size) + (typeof doc.class === 'string' ? ', class ' + clean(doc.class, { max: 20 }) : '') + '.'));
      } else if (view === 'too_large') {
        const limit = typeof doc.limit === 'number' ? fmtBytes(doc.limit) : '?';
        body.appendChild(notice('warn', 'File too large to show: ' + fmtBytes(size) + ', limit ' + limit + '.'));
      } else if (view === 'symlink') {
        body.appendChild(el('div', 'file-link', 'Symbolic link to'));
        body.appendChild(el('pre', 'file-target mono', typeof doc.target === 'string' ? doc.target : '?', true));
        if (validPath(doc.target_path, false)) {
          const tp = doc.target_path;
          body.appendChild(button('Open target', '', () => open(ctx, 'file', { task: v.task, commit: v.commit, path: tp }, { reuse: 'file' }), 'Open the link target (it is not opened automatically)'));
        } else {
          body.appendChild(el('div', 'muted', 'The target is not a plain path inside the repository and is never followed.'));
        }
      } else if (view === 'submodule') {
        body.appendChild(el('div', 'file-link', 'Submodule at commit'));
        body.appendChild(el('pre', 'file-target mono', typeof doc.submodule_commit === 'string' ? doc.submodule_commit : '?'));
        body.appendChild(el('div', 'muted', 'The submodule is not opened.'));
      } else if (view === 'image') {
        body.appendChild(imageView(v, life, life.gen(), basename(v.path)));
      } else {
        body.appendChild(notice('warn', 'Unsupported view type.'));
      }
    }

    function init(params) {
      p = normTarget(params, true);
      const g = life.next();
      if (!p.ok) {
        node.replaceChildren(notice('bad', p.error));
        return;
      }
      meta.replaceChildren();
      msgs.replaceChildren();
      body.replaceChildren(notice('info', 'Loading...'));
      head.setTask(p.v.task);
      head.setCommit(p.v.commit);
      head.setShas([]);
      node.replaceChildren(head.node, meta, msgs, body);
      getDoc('/api/repo/file?' + targetQuery(p.v.task, p.v.commit, p.v.path), 'fleet.repo.file/1').then((r) => {
        if (!life.ok(g)) {
          return;
        }
        body.replaceChildren();
        if (r.err) {
          body.appendChild(errNotice(r.err, p.v.commit));
          return;
        }
        show(r.doc);
      });
    }

    init(raw);
    return {
      node,
      setParams: init,
      stop() {
        life.kill();
      }
    };
  }

  // ------------------------------------------------------------------ diff

  function normDiffParams(raw) {
    const r = isObj(raw) ? raw : {};
    const task = r.task;
    const base = r.base === undefined || r.base === null ? 'base' : r.base;
    const head = r.head === undefined || r.head === null ? 'head' : r.head;
    if (typeof task !== 'string' || !REF_RE.test(task)) {
      return bad('Invalid task reference (expected run/task).');
    }
    if (typeof base !== 'string' || !COMMIT_RE.test(base) || typeof head !== 'string' || !COMMIT_RE.test(head)) {
      return bad('Invalid commit selector (expected head, base or a full commit id).');
    }
    const mode = r.mode === 'split' ? 'split' : 'unified';
    const c = typeof r.context === 'string' && /^[0-9]{1,2}$/.test(r.context) ? Number(r.context) : r.context;
    const context = c === 0 || c === 3 || c === 10 || c === 25 ? c : 3;
    let file = null;
    if (r.file !== undefined && r.file !== null) {
      if (!validPath(r.file, false)) {
        return bad('Invalid path.');
      }
      file = r.file;
    }
    return { ok: true, v: { task, base, head, mode, context, file } };
  }

  function normDiffFile(f) {
    const s = (v) => (typeof v === 'string' ? v : null);
    return {
      path: typeof f.path === 'string' ? f.path : '?',
      old_path: s(f.old_path),
      path_ok: f.path_ok === true && validPath(f.path, false),
      status: f.status,
      similarity: intOrNull(f.similarity),
      old_mode: s(f.old_mode),
      new_mode: s(f.new_mode),
      binary: f.binary === true,
      submodule: f.submodule === true,
      additions: intOrNull(f.additions),
      deletions: intOrNull(f.deletions),
      truncated: f.truncated === true,
      note: s(f.note),
      hunks: Array.isArray(f.hunks) ? f.hunks : []
    };
  }

  function normDiffLine(l) {
    if (!isObj(l) || (l.k !== 'ctx' && l.k !== 'add' && l.k !== 'del')) {
      return null;
    }
    return { k: l.k, o: intOrNull(l.o), n: intOrNull(l.n), t: typeof l.t === 'string' ? l.t : '', cr: l.cr === true, nonl: l.nonl === true, w: l.w };
  }

  function cleanRanges(w, len) {
    if (!Array.isArray(w)) {
      return [];
    }
    const out = [];
    for (let i = 0; i < w.length && i < 100; i += 1) {
      const r = w.at(i);
      if (!Array.isArray(r) || r.length !== 2) {
        continue;
      }
      const a = r.at(0);
      const b = r.at(1);
      if (!Number.isInteger(a) || !Number.isInteger(b) || a < 0 || a >= len || b <= a) {
        continue;
      }
      out.push([a, Math.min(b, len)]);
    }
    out.sort((x, y) => x.at(0) - y.at(0));
    const merged = [];
    let end = 0;
    for (const r of out) {
      if (r.at(0) >= end) {
        merged.push(r);
        end = r.at(1);
      }
    }
    return merged;
  }

  function textCell(L) {
    const s = el('span', 'dtext');
    let rawText = L.t;
    let cut = false;
    if (rawText.length > MAX_LINE_CHARS) {
      rawText = rawText.slice(0, MAX_LINE_CHARS);
      cut = true;
    }
    const ranges = cleanRanges(L.w, rawText.length);
    const piece = (str) => clean(str, { multi: true, max: MAX_LINE_CHARS });
    if (ranges.length === 0) {
      s.textContent = piece(rawText);
    } else {
      let pos = 0;
      for (const r of ranges) {
        if (r.at(0) > pos) {
          s.appendChild(document.createTextNode(piece(rawText.slice(pos, r.at(0)))));
        }
        s.appendChild(el('span', 'dw', piece(rawText.slice(r.at(0), r.at(1))), true));
        pos = r.at(1);
      }
      if (pos < rawText.length) {
        s.appendChild(document.createTextNode(piece(rawText.slice(pos))));
      }
    }
    if (cut) {
      s.appendChild(el('span', 'dmark', ' ...[line cut]'));
    }
    if (L.cr) {
      s.appendChild(el('span', 'dmark', ' \u240d'));
    }
    if (L.nonl) {
      s.appendChild(el('span', 'dmark', ' [no newline at end of file]'));
    }
    return s;
  }

  function foldLines(L) {
    const out = [];
    let i = 0;
    while (i < L.length) {
      if (L.at(i).k !== 'ctx') {
        out.push(L.at(i));
        i += 1;
        continue;
      }
      let j = i;
      while (j < L.length && L.at(j).k === 'ctx') {
        j += 1;
      }
      const lead = i === 0 ? 0 : KEEP_CONTEXT;
      const trail = j === L.length ? 0 : KEEP_CONTEXT;
      const hidden = j - i - lead - trail;
      if (hidden >= 2 && (lead > 0 || trail > 0)) {
        for (let k = i; k < i + lead; k += 1) {
          out.push(L.at(k));
        }
        out.push({ fold: L.slice(i + lead, j - trail) });
        for (let k = j - trail; k < j; k += 1) {
          out.push(L.at(k));
        }
      } else {
        for (let k = i; k < j; k += 1) {
          out.push(L.at(k));
        }
      }
      i = j;
    }
    return out;
  }

  function toRows(items, mode) {
    const rows = [];
    if (mode === 'unified') {
      for (const it of items) {
        rows.push(it.fold ? { type: 'fold', count: it.fold.length, rows: toRows(it.fold, mode) } : { type: 'line', L: it });
      }
      return rows;
    }
    let dels = [];
    let adds = [];
    const flush = () => {
      const n = Math.max(dels.length, adds.length);
      for (let i = 0; i < n; i += 1) {
        rows.push({ type: 'pair', l: dels.at(i) || null, r: adds.at(i) || null });
      }
      dels = [];
      adds = [];
    };
    for (const it of items) {
      if (it.fold) {
        flush();
        rows.push({ type: 'fold', count: it.fold.length, rows: toRows(it.fold, mode) });
      } else if (it.k === 'ctx') {
        flush();
        rows.push({ type: 'pair', l: it, r: it });
      } else if (it.k === 'del') {
        dels.push(it);
      } else {
        adds.push(it);
      }
    }
    flush();
    return rows;
  }

  function buildModel(f, mode, collapse) {
    const rows = [];
    let used = 0;
    let cut = 0;
    f.hunks.forEach((h, idx) => {
      if (!isObj(h)) {
        return;
      }
      const lines = [];
      const src = Array.isArray(h.lines) ? h.lines : [];
      for (let i = 0; i < src.length; i += 1) {
        if (used >= MAX_DIFF_LINES) {
          cut += src.length - i;
          break;
        }
        const L = normDiffLine(src.at(i));
        if (L) {
          lines.push(L);
          used += 1;
        }
      }
      const section = typeof h.section === 'string' && h.section !== '' ? ' ' + h.section : '';
      const text = '@@ -' + host.num(h.old_start) + ',' + host.num(h.old_count) + ' +' + host.num(h.new_start) + ',' + host.num(h.new_count) + ' @@' + section;
      rows.push({ type: 'hunk', text, idx });
      for (const r of toRows(collapse ? foldLines(lines) : lines, mode)) {
        rows.push(r);
      }
    });
    return { rows, cut };
  }

  function diffPane(ctx, raw) {
    const life = lifecycle();
    let p = null;
    let files = [];
    let sel = 0;
    let mode = 'unified';
    let context = 3;
    let collapse = false;
    let view = null;
    const node = el('div', 'pane pane-diff');
    const toolbar = el('div', 'pane-tools');
    const summary = el('div', 'pane-summary muted');
    const msgs = el('div', 'pane-msgs');
    const layout = el('div', 'diff-layout');
    const list = el('div', 'diff-files');
    list.setAttribute('role', 'listbox');
    list.setAttribute('aria-label', 'Changed files');
    const pane = el('div', 'diff-view');
    layout.appendChild(list);
    layout.appendChild(pane);
    const head = makeHeader('Diff', 'diff', null);
    const modeBtns = new Map();
    const ctxBtns = new Map();
    for (const m of [['unified', 'Unified'], ['split', 'Side by side']]) {
      const b = button(m.at(1), 'seg', () => setMode(m.at(0)));
      modeBtns.set(m.at(0), b);
      toolbar.appendChild(b);
    }
    toolbar.appendChild(el('span', 'muted', 'Context lines'));
    for (const c of [0, 3, 10, 25]) {
      const b = button(String(c), 'seg', () => {
        if (c !== context) {
          context = c;
          syncToolbar();
          load();
        }
      });
      ctxBtns.set(c, b);
      toolbar.appendChild(b);
    }
    const foldBtn = button('Collapse unchanged context', '', () => {
      collapse = !collapse;
      syncToolbar();
      renderFile();
    });
    toolbar.appendChild(foldBtn);

    function syncToolbar() {
      for (const [k, b] of modeBtns) {
        setPressed(b, k === mode);
      }
      for (const [k, b] of ctxBtns) {
        setPressed(b, k === context);
      }
      setPressed(foldBtn, collapse);
    }

    function setMode(m) {
      if (m !== mode) {
        mode = m;
        syncToolbar();
        renderFile();
      }
    }

    function renderRow(r) {
      if (r.type === 'hunk') {
        const h = el('div', 'diff-hunk', r.text, true);
        h.setAttribute('tabindex', '-1');
        view.hunkEls.set(r.idx, h);
        return h;
      }
      if (r.type === 'line') {
        const L = r.L;
        const d = el('div', 'diff-row diff-' + L.k);
        d.appendChild(el('span', 'dno', L.o === null ? '' : String(L.o)));
        d.appendChild(el('span', 'dno', L.n === null ? '' : String(L.n)));
        d.appendChild(el('span', 'dsign', L.k === 'add' ? '+' : L.k === 'del' ? '-' : ' '));
        d.appendChild(textCell(L));
        return d;
      }
      if (r.type === 'pair') {
        const d = el('div', 'diff-srow');
        d.appendChild(cell(r.l, 'o'));
        d.appendChild(cell(r.r, 'n'));
        return d;
      }
      const box = el('div', 'diff-foldbox');
      box.appendChild(
        button(r.count + ' unchanged lines', 'diff-fold', () => {
          const parts = r.rows.map((x) => renderRow(x));
          box.replaceChildren(...parts);
        }, 'Show the unchanged lines')
      );
      return box;
    }

    function cell(L, side) {
      if (!L) {
        return el('div', 'diff-cell diff-empty');
      }
      const c = el('div', 'diff-cell diff-' + L.k);
      const n = side === 'o' ? L.o : L.n;
      c.appendChild(el('span', 'dno', n === null ? '' : String(n)));
      c.appendChild(textCell(L));
      return c;
    }

    function renderMore() {
      if (!view) {
        return;
      }
      const end = Math.min(view.pos + DIFF_CHUNK, view.rows.length);
      for (; view.pos < end; view.pos += 1) {
        view.box.appendChild(renderRow(view.rows.at(view.pos)));
      }
      view.more.replaceChildren();
      if (view.pos < view.rows.length) {
        const left = view.rows.length - view.pos;
        view.more.appendChild(button('Show more (' + Math.min(DIFF_CHUNK, left) + ' of ' + left + ' remaining rows)', '', renderMore));
      }
    }

    function fileHead(f) {
      const h = el('div', 'diff-fhead');
      h.appendChild(el('span', 'diff-path mono', f.path));
      h.appendChild(host.tagged('ds', FILE_STATUS, f.status, typeof f.status === 'string' ? f.status : 'unknown'));
      h.appendChild(el('span', 'diff-add', '+' + host.num(f.additions)));
      h.appendChild(el('span', 'diff-del', '-' + host.num(f.deletions)));
      const openable = f.path_ok && f.status !== 'deleted' && !f.submodule;
      const b = button('Open file', '', () => open(ctx, 'file', { task: p.v.task, commit: p.v.head, path: f.path }, { reuse: 'file' }), openable ? 'Open the file at the head commit' : 'This file cannot be opened');
      if (!openable) {
        b.disabled = true;
        b.setAttribute('disabled', 'disabled');
      }
      h.appendChild(b);
      return h;
    }

    function renderFile() {
      pane.replaceChildren();
      view = null;
      const f = files.at(sel);
      if (!f) {
        pane.appendChild(notice('info', 'No changed files.'));
        return;
      }
      pane.appendChild(fileHead(f));
      if (f.old_path !== null && (f.status === 'renamed' || f.status === 'copied')) {
        const sim = f.similarity === null ? '' : ' (' + f.similarity + '% similar)';
        pane.appendChild(notice('info', (f.status === 'renamed' ? 'Renamed: ' : 'Copied: ') + f.old_path + ' \u2192 ' + f.path + sim));
      }
      if (f.status === 'mode_changed' || f.status === 'type_changed') {
        pane.appendChild(notice('info', (f.status === 'mode_changed' ? 'Mode changed: ' : 'Type changed: ') + (f.old_mode || '?') + ' \u2192 ' + (f.new_mode || '?')));
      } else if (f.old_mode !== null && f.new_mode !== null && f.old_mode !== f.new_mode && f.status !== 'added' && f.status !== 'deleted') {
        pane.appendChild(notice('info', 'Mode: ' + f.old_mode + ' \u2192 ' + f.new_mode));
      }
      if (f.submodule) {
        pane.appendChild(notice('info', 'Submodule change (the submodule is not opened).'));
      }
      if (f.binary) {
        pane.appendChild(notice('info', 'Binary file, not shown.'));
      }
      if (f.note !== null && f.note !== '') {
        pane.appendChild(notice(f.truncated ? 'warn' : 'info', f.note));
      } else if (f.truncated) {
        pane.appendChild(notice('warn', 'This file diff is truncated.'));
      }
      if (f.binary || f.hunks.length === 0) {
        return;
      }
      const model = buildModel(f, mode, collapse);
      if (model.cut > 0) {
        pane.appendChild(notice('warn', model.cut + ' diff lines are not shown (limit ' + MAX_DIFF_LINES + ' per file).'));
      }
      const box = el('div', 'diff-body diff-' + mode);
      const more = el('div', 'code-more');
      view = { rows: model.rows, pos: 0, box, more, hunkEls: new Map(), hunkRows: [], cur: -1 };
      model.rows.forEach((r, i) => {
        if (r.type === 'hunk') {
          view.hunkRows.push({ row: i, idx: r.idx });
        }
      });
      pane.appendChild(box);
      pane.appendChild(more);
      renderMore();
    }

    function renderList() {
      list.replaceChildren();
      files.forEach((f, i) => {
        const b = el('button', 'diff-file' + (i === sel ? ' selected' : ''));
        b.setAttribute('type', 'button');
        b.setAttribute('role', 'option');
        b.setAttribute('aria-selected', i === sel ? 'true' : 'false');
        b.appendChild(host.tagged('ds', FILE_STATUS, f.status, typeof f.status === 'string' ? f.status : 'unknown'));
        b.appendChild(el('span', 'diff-name mono', f.path));
        b.appendChild(el('span', 'diff-add', '+' + host.num(f.additions)));
        b.appendChild(el('span', 'diff-del', '-' + host.num(f.deletions)));
        if (f.binary) {
          b.appendChild(el('span', 'diff-flag', 'binary'));
        }
        if (f.submodule) {
          b.appendChild(el('span', 'diff-flag', 'submodule'));
        }
        if (f.truncated) {
          b.appendChild(el('span', 'diff-flag', 'truncated'));
        }
        if (!f.path_ok) {
          b.appendChild(el('span', 'diff-flag', 'not openable'));
        }
        b.addEventListener('click', () => selectFile(i));
        list.appendChild(b);
      });
    }

    function selectFile(i) {
      if (i < 0 || i >= files.length || i === sel) {
        return;
      }
      sel = i;
      renderList();
      renderFile();
    }

    function gotoHunk(delta) {
      if (!view || view.hunkRows.length === 0) {
        return;
      }
      const t = Math.max(0, Math.min(view.hunkRows.length - 1, view.cur + delta));
      const target = view.hunkRows.at(t);
      while (view.pos <= target.row && view.pos < view.rows.length) {
        renderMore();
      }
      view.cur = t;
      const h = view.hunkEls.get(target.idx);
      if (h) {
        if (typeof h.scrollIntoView === 'function') {
          h.scrollIntoView({ block: 'start' });
        }
        if (typeof h.focus === 'function') {
          h.focus();
        }
      }
    }

    node.addEventListener('keydown', (e) => {
      if (!e || e.ctrlKey || e.metaKey || e.altKey || isTextInput(e.target)) {
        return;
      }
      let handled = true;
      switch (e.key) {
        case 'j':
        case 'ArrowDown':
          selectFile(sel + 1);
          break;
        case 'k':
        case 'ArrowUp':
          selectFile(sel - 1);
          break;
        case 'n':
        case ']':
          gotoHunk(1);
          break;
        case 'p':
        case '[':
          gotoHunk(-1);
          break;
        case 'u':
          setMode(mode === 'unified' ? 'split' : 'unified');
          break;
        default:
          handled = false;
      }
      if (handled) {
        prevent(e);
      }
    });

    function load() {
      const g = life.next();
      const prevPath = files.at(sel) ? files.at(sel).path : p.v.file;
      msgs.replaceChildren();
      list.replaceChildren();
      pane.replaceChildren(notice('info', 'Loading...'));
      view = null;
      const url = '/api/repo/diff?task=' + encodeURIComponent(p.v.task) + '&base=' + encodeURIComponent(p.v.base) + '&head=' + encodeURIComponent(p.v.head) + '&context=' + context;
      getDoc(url, 'fleet.repo.diff/1').then((r) => {
        if (!life.ok(g)) {
          return;
        }
        pane.replaceChildren();
        if (r.err) {
          msgs.appendChild(errNotice(r.err, p.v.head));
          return;
        }
        const doc = r.doc;
        head.setShas([doc.base, doc.head]);
        setTitle(ctx, 'Diff ' + p.v.task);
        const src = Array.isArray(doc.files) ? doc.files : [];
        files = [];
        for (let i = 0; i < src.length && files.length < MAX_DIFF_FILES; i += 1) {
          if (isObj(src.at(i))) {
            files.push(normDiffFile(src.at(i)));
          }
        }
        const st = isObj(doc.stats) ? doc.stats : {};
        summary.textContent = host.num(st.files) + ' files, +' + host.num(st.additions) + ' -' + host.num(st.deletions) + ', ' + host.num(st.binary) + ' binary';
        if (doc.truncated === true) {
          msgs.appendChild(notice('warn', 'Truncated: some files were left out of this diff.' + (typeof doc.notice === 'string' && doc.notice !== '' ? ' ' + doc.notice : '')));
        } else if (typeof doc.notice === 'string' && doc.notice !== '') {
          msgs.appendChild(notice('info', doc.notice));
        }
        if (src.length > files.length) {
          msgs.appendChild(notice('warn', 'Only the first ' + MAX_DIFF_FILES + ' files are listed.'));
        }
        let at = prevPath === null || prevPath === undefined ? -1 : files.findIndex((f) => f.path === prevPath);
        if (at < 0) {
          at = 0;
        }
        sel = at;
        renderList();
        renderFile();
      });
    }

    function init(params) {
      p = normDiffParams(params);
      life.next();
      files = [];
      sel = 0;
      view = null;
      if (!p.ok) {
        node.replaceChildren(notice('bad', p.error));
        return;
      }
      mode = p.v.mode;
      context = p.v.context;
      collapse = false;
      syncToolbar();
      head.setTask(p.v.task);
      head.setSelector(p.v.base, p.v.head);
      head.setShas([]);
      node.replaceChildren(head.node, toolbar, summary, msgs, layout);
      load();
    }

    init(raw);
    return {
      node,
      setParams: init,
      stop() {
        life.kill();
      }
    };
  }

  // -------------------------------------------------------------- markdown

  // extra = text nodes the caller will add through textContent, counted with the element
  function mdMake(st, tag, cls, extra) {
    if (st.stop || !MD_TAGS.has(tag)) {
      return null;
    }
    st.nodes += 1 + (extra || 0);
    if (st.nodes > MD_MAX_NODES) {
      st.stop = true;
      st.capped = true;
      return null;
    }
    return el(tag, cls);
  }

  function mdText(st, s) {
    if (st.stop) {
      return null;
    }
    st.nodes += 1;
    if (st.nodes > MD_MAX_NODES) {
      st.stop = true;
      st.capped = true;
      return null;
    }
    return document.createTextNode(clean(s, { multi: true, max: 20000 }));
  }

  function mdInlines(arr, parent, depth, st, inLink) {
    if (!Array.isArray(arr)) {
      return;
    }
    for (let i = 0; i < arr.length; i += 1) {
      if (st.stop) {
        return;
      }
      const n = mdInline(arr[i], depth, st, inLink);
      if (n) {
        parent.appendChild(n);
      }
    }
  }

  function mdWrap(tag, cls, n, depth, st, inLink) {
    const e = mdMake(st, tag, cls);
    if (e) {
      mdInlines(n.c, e, depth + 1, st, inLink);
    }
    return e;
  }

  function mdInline(n, depth, st, inLink) {
    if (!isObj(n)) {
      return null;
    }
    if (depth > MD_MAX_DEPTH) {
      st.deep = true;
      return null;
    }
    switch (n.t) {
      case 'text':
        return typeof n.s === 'string' ? mdText(st, n.s) : null;
      case 'em':
        return mdWrap('em', '', n, depth, st, inLink);
      case 'strong':
        return mdWrap('strong', '', n, depth, st, inLink);
      case 'del':
        return mdWrap('del', '', n, depth, st, inLink);
      case 'code': {
        const e = mdMake(st, 'code', 'md-code', 1);
        if (e && typeof n.s === 'string') {
          e.textContent = clean(n.s, { multi: true, max: 20000 });
        }
        return e;
      }
      case 'br':
        return mdMake(st, 'br', '');
      case 'link':
        return mdLink(n, depth, st, inLink);
      case 'image':
        return mdImage(n, st);
      default:
        return null;
    }
  }

  function mdLink(n, depth, st, inLink) {
    const kind = n.kind;
    let target = '';
    let url = null;
    let path = null;
    if (!inLink && kind === 'external' && typeof n.url === 'string' && n.url.length <= MAX_URL && URL_RE.test(n.url)) {
      url = n.url;
      target = n.url;
    } else if (!inLink && kind === 'repo' && validPath(n.path, false)) {
      path = n.path;
      target = n.path;
    }
    if (url === null && path === null) {
      const e = mdMake(st, 'span', 'md-link md-dead');
      if (e) {
        mdInlines(n.c, e, depth + 1, st, true);
      }
      return e;
    }
    const a = mdMake(st, 'a', 'md-link ' + (url !== null ? 'md-external' : 'md-repo'));
    if (!a) {
      return null;
    }
    const shown = clean(target, { max: 600 });
    const own = typeof n.title === 'string' && n.title !== '' ? clean(n.title, { max: 200 }) + ' (' + shown + ')' : shown;
    a.setAttribute('title', own);
    if (url !== null) {
      a.setAttribute('href', url);
      a.setAttribute('rel', 'noopener noreferrer');
      const ask = (ev) => {
        prevent(ev);
        st.confirm(url);
      };
      a.addEventListener('click', ask);
      a.addEventListener('auxclick', ask);
    } else {
      a.setAttribute('tabindex', '0');
      a.addEventListener('click', (ev) => {
        prevent(ev);
        st.openRepo(path);
      });
      a.addEventListener('keydown', (ev) => {
        if (ev && ev.key === 'Enter') {
          prevent(ev);
          st.openRepo(path);
        }
      });
    }
    const on = () => st.status(shown);
    const off = () => st.status('');
    a.addEventListener('mouseover', on);
    a.addEventListener('focus', on);
    a.addEventListener('mouseout', off);
    a.addEventListener('blur', off);
    mdInlines(n.c, a, depth + 1, st, true);
    return a;
  }

  function mdImage(n, st) {
    const holder = mdMake(st, 'span', 'md-image');
    if (!holder) {
      return null;
    }
    const alt = typeof n.alt === 'string' ? n.alt : '';
    const note = typeof n.note === 'string' && n.note !== '' ? n.note : '';
    const label = mdMake(st, 'span', 'md-alt', 1);
    if (!label) {
      return holder;
    }
    let reason = note;
    if (validPath(n.path, false)) {
      if (st.images.length < MD_MAX_IMAGES) {
        st.images.push({ holder, label, alt, note, path: n.path });
        reason = 'loading image';
      } else {
        reason = 'image not loaded (at most ' + MD_MAX_IMAGES + ' images per document)';
      }
    } else if (reason === '') {
      reason = 'image not loaded';
    }
    label.textContent = clean(alt, { max: 300 }) + ' [' + clean(reason, { max: 200 }) + ']';
    holder.appendChild(label);
    return holder;
  }

  function mdBlocks(arr, parent, depth, st) {
    if (!Array.isArray(arr)) {
      return;
    }
    for (let i = 0; i < arr.length; i += 1) {
      if (st.stop) {
        return;
      }
      const n = mdBlock(arr[i], depth, st);
      if (n) {
        parent.appendChild(n);
      }
    }
  }

  function mdItems(n, tag, depth, st) {
    const e = mdMake(st, tag, '');
    if (!e) {
      return null;
    }
    if (tag === 'ol' && Number.isInteger(n.start) && n.start >= 0 && n.start <= 1000000000 && n.start !== 1) {
      e.setAttribute('start', String(n.start));
    }
    const items = Array.isArray(n.c) ? n.c : [];
    for (let i = 0; i < items.length; i += 1) {
      if (st.stop) {
        break;
      }
      const it = items[i];
      if (!isObj(it)) {
        continue;
      }
      if (depth + 1 > MD_MAX_DEPTH) {
        st.deep = true;
        break;
      }
      const li = mdMake(st, 'li', typeof it.checked === 'boolean' ? 'md-task' : '');
      if (!li) {
        break;
      }
      if (typeof it.checked === 'boolean') {
        const box = mdMake(st, 'input', 'md-check');
        if (box) {
          box.setAttribute('type', 'checkbox');
          box.setAttribute('disabled', 'disabled');
          box.setAttribute('title', it.checked ? 'done' : 'not done');
          if (it.checked) {
            box.setAttribute('checked', 'checked');
          }
          li.appendChild(box);
        }
      }
      mdBlocks(it.c, li, depth + 2, st);
      e.appendChild(li);
    }
    return e;
  }

  function mdRow(cells, tag, align, parent, depth, st) {
    const tr = mdMake(st, 'tr', '');
    if (!tr) {
      return;
    }
    if (Array.isArray(cells)) {
      for (let i = 0; i < cells.length && i < 200; i += 1) {
        const a = Array.isArray(align) ? align[i] : null;
        const c = mdMake(st, tag, typeof a === 'string' && Object.prototype.hasOwnProperty.call(ALIGN_CLASS, a) ? ALIGN_CLASS[a] : '');
        if (!c) {
          break;
        }
        mdInlines(cells[i], c, depth + 1, st, false);
        tr.appendChild(c);
      }
    }
    parent.appendChild(tr);
  }

  function mdTable(n, depth, st) {
    const t = mdMake(st, 'table', 'md-table');
    if (!t) {
      return null;
    }
    const thead = mdMake(st, 'thead', '');
    if (thead) {
      mdRow(n.head, 'th', n.align, thead, depth + 1, st);
      t.appendChild(thead);
    }
    const tbody = mdMake(st, 'tbody', '');
    if (tbody) {
      const rows = Array.isArray(n.rows) ? n.rows : [];
      for (let i = 0; i < rows.length && !st.stop; i += 1) {
        mdRow(rows[i], 'td', n.align, tbody, depth + 1, st);
      }
      t.appendChild(tbody);
    }
    return t;
  }

  function mdBlock(n, depth, st) {
    if (!isObj(n)) {
      return null;
    }
    if (depth > MD_MAX_DEPTH) {
      st.deep = true;
      return null;
    }
    switch (n.t) {
      case 'h': {
        const level = Number.isInteger(n.level) && n.level >= 1 && n.level <= 6 ? n.level : 6;
        const e = mdMake(st, HEADS.at(level - 1), 'md-h');
        if (e) {
          mdInlines(n.c, e, depth + 1, st, false);
        }
        return e;
      }
      case 'p': {
        const e = mdMake(st, 'p', '');
        if (e) {
          mdInlines(n.c, e, depth + 1, st, false);
        }
        return e;
      }
      case 'code': {
        const pre = mdMake(st, 'pre', 'md-pre');
        const code = pre ? mdMake(st, 'code', '', 1) : null;
        if (!pre || !code) {
          return pre;
        }
        pre.setAttribute('tabindex', '0');
        if (typeof n.lang === 'string' && n.lang !== '') {
          pre.setAttribute('title', clean(n.lang, { max: 40 }));
        }
        code.textContent = typeof n.text === 'string' ? clean(n.text, { multi: true, max: 100000 }) : '';
        pre.appendChild(code);
        return pre;
      }
      case 'quote': {
        const e = mdMake(st, 'blockquote', '');
        if (e) {
          mdBlocks(n.c, e, depth + 1, st);
        }
        return e;
      }
      case 'ul':
        return mdItems(n, 'ul', depth, st);
      case 'ol':
        return mdItems(n, 'ol', depth, st);
      case 'table':
        return mdTable(n, depth, st);
      case 'hr':
        return mdMake(st, 'hr', '');
      default:
        return null;
    }
  }

  function markdownPane(ctx, raw) {
    const life = lifecycle();
    let p = null;
    const node = el('div', 'pane pane-markdown');
    const tools = el('div', 'pane-tools');
    const msgs = el('div', 'pane-msgs');
    const status = el('div', 'md-status mono');
    const bar = el('div', 'md-confirm-slot');
    const docEl = el('div', 'md-doc');
    const head = makeHeader('Preview', 'single', (c) => {
      if (p && p.ok && c !== p.v.commit) {
        init({ task: p.v.task, commit: c, path: p.v.path });
      }
    });

    function confirm(url) {
      bar.replaceChildren();
      const box = el('div', 'md-confirm');
      box.setAttribute('role', 'alertdialog');
      box.setAttribute('aria-label', 'Open external link');
      box.appendChild(el('span', 'md-confirm-text', 'Open '));
      box.appendChild(el('span', 'md-confirm-url mono', url));
      box.appendChild(el('span', 'md-confirm-text', ' in a new tab?'));
      const close = () => bar.replaceChildren();
      box.appendChild(
        button('Open', 'primary', () => {
          close();
          window.open(url, '_blank', 'noopener,noreferrer');
        })
      );
      const cancel = button('Cancel', '', close);
      box.appendChild(cancel);
      box.addEventListener('keydown', (ev) => {
        if (ev && ev.key === 'Escape') {
          close();
        }
      });
      bar.appendChild(box);
      if (typeof cancel.focus === 'function') {
        cancel.focus();
      }
    }

    async function loadImages(list, g) {
      for (const it of list) {
        if (!life.ok(g)) {
          return;
        }
        const r = await loadImageData(p.v.task, p.v.commit, it.path);
        if (!life.ok(g)) {
          return;
        }
        if (r.ok) {
          const img = imageEl(r.url, it.alt, 'md-img');
          it.holder.replaceChildren(img);
        } else {
          it.label.textContent = clean(it.alt, { max: 300 }) + ' [image not loaded: ' + clean(r.reason, { max: 120 }) + ']';
        }
      }
    }

    function render(doc, g) {
      const st = {
        nodes: 0,
        stop: false,
        capped: false,
        deep: false,
        images: [],
        confirm,
        status: (t) => {
          status.textContent = t === '' ? '' : 'Link: ' + t;
        },
        openRepo: (path) => {
          const kind = isMarkdownName(path) ? 'markdown' : 'file';
          open(ctx, kind, { task: p.v.task, commit: p.v.commit, path }, { reuse: kind });
        }
      };
      docEl.replaceChildren();
      mdBlocks(doc.blocks, docEl, 1, st);
      if (doc.truncated === true) {
        msgs.appendChild(notice('warn', 'Truncated: the document is longer than what is shown.'));
      }
      if (typeof doc.notice === 'string' && doc.notice !== '') {
        msgs.appendChild(notice('info', doc.notice));
      }
      if (st.capped || st.deep) {
        msgs.appendChild(notice('warn', 'Part of this document is not shown: it is too large or nested too deeply.'));
      }
      loadImages(st.images, g);
    }

    function init(params) {
      p = normTarget(params, true);
      const g = life.next();
      if (!p.ok) {
        node.replaceChildren(notice('bad', p.error));
        return;
      }
      tools.replaceChildren();
      msgs.replaceChildren();
      bar.replaceChildren();
      status.textContent = '';
      docEl.replaceChildren(notice('info', 'Loading...'));
      head.setTask(p.v.task);
      head.setCommit(p.v.commit);
      head.setShas([]);
      tools.appendChild(el('span', 'pane-path mono', p.v.path));
      tools.appendChild(button('View source', '', () => open(ctx, 'file', { task: p.v.task, commit: p.v.commit, path: p.v.path }, { reuse: 'file' })));
      node.replaceChildren(head.node, tools, msgs, bar, status, docEl);
      getDoc('/api/repo/markdown?' + targetQuery(p.v.task, p.v.commit, p.v.path), 'fleet.repo.markdown/1').then((r) => {
        if (!life.ok(g)) {
          return;
        }
        docEl.replaceChildren();
        if (r.err) {
          msgs.appendChild(errNotice(r.err, p.v.commit));
          return;
        }
        head.setShas([r.doc.commit]);
        setTitle(ctx, 'Preview ' + basename(p.v.path));
        render(r.doc, g);
      });
    }

    init(raw);
    return {
      node,
      setParams: init,
      stop() {
        life.kill();
      }
    };
  }

  // ----------------------------------------------------------------- image

  function imagePane(ctx, raw) {
    const life = lifecycle();
    let p = null;
    const node = el('div', 'pane pane-image');
    const tools = el('div', 'pane-tools');
    const body = el('div', 'pane-body');
    const head = makeHeader('Image', 'single', (c) => {
      if (p && p.ok && c !== p.v.commit) {
        init({ task: p.v.task, commit: c, path: p.v.path });
      }
    });

    function init(params) {
      p = normTarget(params, true);
      const g = life.next();
      if (!p.ok) {
        node.replaceChildren(notice('bad', p.error));
        return;
      }
      tools.replaceChildren();
      head.setTask(p.v.task);
      head.setCommit(p.v.commit);
      head.setShas([]);
      tools.appendChild(el('span', 'pane-path mono', p.v.path));
      body.replaceChildren(imageView(p.v, life, g, basename(p.v.path)));
      node.replaceChildren(head.node, tools, body);
      setTitle(ctx, basename(p.v.path));
    }

    init(raw);
    return {
      node,
      setParams: init,
      stop() {
        life.kill();
      }
    };
  }

  // -------------------------------------------------------------- register

  function titled(label, usePath) {
    return (params) => {
      const r = isObj(params) ? params : {};
      if (usePath && validPath(r.path, false)) {
        return clean(label + ' ' + basename(r.path), { max: 80 });
      }
      if (typeof r.task === 'string' && REF_RE.test(r.task)) {
        return label + ' ' + r.task;
      }
      return label;
    };
  }

  function register(kind, label, usePath, make) {
    dock.registerPane(kind, {
      title: titled(label, usePath),
      create(ctx, params) {
        return make(ctx, params === undefined ? ctx && ctx.params : params);
      }
    });
  }

  register('explorer', 'Explorer', false, explorerPane);
  register('file', 'File', true, filePane);
  register('diff', 'Diff', false, diffPane);
  register('markdown', 'Preview', true, markdownPane);
  register('image', 'Image', true, imagePane);
})();
