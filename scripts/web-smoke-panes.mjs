// Smoke tests of the five workbench panes (explorer, file, diff, markdown, image).
// Run from the repo root: node scripts/web-smoke-panes.mjs   (no dependencies)
// It loads the real panes.js into a vm context with a fake DOM, a stub fleetHost and a capturing fleetDock.
import vm from 'node:vm';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFakeDom, FakeSessionStorage, makeFetch, makeHost, settle } from './fake-dom.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const JS_PATH = path.join(here, '..', 'crates', 'fleet-infra', 'web', 'panes.js');
const CSS_PATH = path.join(here, '..', 'crates', 'fleet-infra', 'web', 'panes.css');
const SRC = fs.readFileSync(JS_PATH, 'utf8');
const CSS = fs.readFileSync(CSS_PATH, 'utf8');

let checks = 0;
const failures = [];
function check(name, cond, detail) {
  checks += 1;
  if (!cond) {
    failures.push(name + (detail === undefined ? '' : ' :: ' + String(detail).slice(0, 300)));
  }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want), 'got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want));
}
process.on('unhandledRejection', (e) => {
  failures.push('unhandled rejection: ' + (e && e.stack ? e.stack : e));
});

const SHA = 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678';
const SHA2 = 'f'.repeat(40);
const REF = 'r1/api';
const allDoms = [];

// ------------------------------------------------------------------ harness

function boot(routes, opts) {
  const o = opts || {};
  const dom = createFakeDom();
  allDoms.push(dom);
  const fetchFn = makeFetch(routes || (() => ({ status: 404, body: { schema: 'fleet.error/1', error: { code: 'not_found', message: 'nothing here' } } })));
  const host = makeHost(dom, fetchFn, o.host);
  const storage = new FakeSessionStorage();
  const registered = new Map();
  const dock = { registerPane: (kind, spec) => registered.set(kind, spec) };
  const opened = [];
  const win = { fleetHost: host, fleetDock: dock, sessionStorage: storage, opens: [], open: (...a) => win.opens.push(a) };
  if (o.noHost) delete win.fleetHost;
  if (o.noDock) delete win.fleetDock;
  const sandbox = { window: win, document: dom.document, btoa: (s) => Buffer.from(s, 'latin1').toString('base64'), console };
  const ctx = vm.createContext(sandbox);
  vm.runInContext(SRC, ctx, { filename: 'panes.js' });
  const b = { dom, fetchFn, host, storage, registered, win, opened, ctx };
  b.mount = async (kind, params, extra) => {
    const titles = [];
    const pctx = Object.assign(
      {
        host,
        params,
        open: (k, p, op) => opened.push({ kind: k, params: p, opts: op }),
        setTitle: (t) => titles.push(t),
        close() {},
        isActive: () => true
      },
      extra || {}
    );
    const spec = registered.get(kind);
    const inst = spec.create(pctx, params);
    await settle(20);
    return { inst, node: inst.node, titles, pctx };
  };
  b.calls = (re) => fetchFn.calls.filter((c) => (re ? re.test(c.url) : true)).map((c) => c.url);
  return b;
}

function json(body, status) {
  return { status: status || 200, body };
}
function errDoc(status, code, message) {
  return { status, body: { schema: 'fleet.error/1', error: { code, message, subject: null } } };
}
function qp(url) {
  const u = new URL(url, 'http://x.invalid');
  return { p: u.pathname, q: u.searchParams };
}
function routesOf(map) {
  return (url) => {
    const { p, q } = qp(url);
    const h = map[p];
    if (!h) return errDoc(404, 'not_found', 'no route ' + p);
    return h(q, url);
  };
}
const verif = (state, dirty) => ({ taskVerification: async () => ({ state, dirty, commit: null }) });

function treeDoc(p, entries, extra) {
  return Object.assign({ schema: 'fleet.repo.tree/1', task: REF, at: 'head', commit: SHA, path: p, entries, truncated: false, limit: 2000 }, extra || {});
}
function fileDoc(p, extra) {
  return Object.assign(
    {
      schema: 'fleet.repo.file/1', task: REF, at: 'head', commit: SHA, path: p, kind: 'file', size: 120, mode: 'regular', view: 'text', class: 'text', image: null,
      lines: ['fn main() {', '}'], line_count: 2, truncated: false, clipped: 0, lossy: false, crlf: false, target: null, target_path: null, submodule_commit: null, markdown: false, limit: 1048576, notice: null
    },
    extra || {}
  );
}
function mdDoc(blocks, extra) {
  return Object.assign({ schema: 'fleet.repo.markdown/1', task: REF, at: 'head', commit: SHA, path: 'README.md', truncated: false, notice: null, blocks }, extra || {});
}
function ent(name, kind, p, size) {
  return { name, path: p === undefined ? name : p, kind, size: size === undefined ? (kind === 'dir' || kind === 'submodule' ? null : 10) : size, mode: 'regular' };
}
const text = (n) => n.textContent;

const PNG = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4, 5]);
function pngResponse(bytes, type) {
  const ab = new ArrayBuffer(bytes.length);
  new Uint8Array(ab).set(bytes);
  return { status: 200, headers: { 'content-type': type || 'image/png' }, bytes: ab };
}

// ------------------------------------------------------------ static checks

{
  const FORBIDDEN = [
    'innerHTML', 'outerHTML', 'insertAdjacentHTML', 'insertAdjacentElement', 'document.write', 'eval(', 'new Function', 'Function(',
    'createContextualFragment', 'DOMParser', 'srcdoc', 'javascript:', 'vbscript:', "setAttribute('style'", 'setAttribute("style"', 'cssText',
    "setTimeout(\"", "setTimeout('", "setInterval(\"", "setInterval('", 'localStorage', 'document.cookie', 'document.domain', 'EventSource', 'import(',
    'importScripts', 'XMLHttpRequest', 'WebSocket', 'SharedWorker', 'ServiceWorker', 'serviceWorker', 'sendBeacon', 'createObjectURL',
    'location.assign', 'location.replace', 'location.href', 'http://', 'https://', "setAttribute('on", 'setAttribute("on', "setAttribute('srcdoc",
    "setAttribute('formaction", 'setAttributeNS'
  ];
  for (const bad of FORBIDDEN) check('panes.js has no ' + bad, !SRC.includes(bad));
  check('panes.js uses textContent', SRC.includes('textContent'));
  check("panes.js is strict", SRC.includes("'use strict'"));
  check('panes.js has no fetch(', !SRC.includes('fetch('));
  check('panes.js has no btoa of whole data, only in chunks', (SRC.match(/btoa\(/g) || []).length === 1);
  check('panes.js < 128 KiB', Buffer.byteLength(SRC) < 128 * 1024, Buffer.byteLength(SRC));
  check('panes.css < 48 KiB', Buffer.byteLength(CSS) < 48 * 1024, Buffer.byteLength(CSS));
  check('panes.css has no //', !CSS.includes('//'));
  check('panes.css has no @import', !CSS.includes('@import'));
  for (const m of CSS.matchAll(/url\(/g)) {
    const after = CSS.slice(m.index + 4).replace(/^["' ]+/, '');
    check('css url() is data:', after.startsWith('data:'));
  }
  // every sessionStorage mention must name an allowed key
  for (const line of SRC.split('\n')) if (line.includes('sessionStorage')) check('sessionStorage key', /fleet\.session|fleet\.layout|STORE_KEY|LAYOUT_KEY/.test(line));
  check('panes.js has no top-level leak', SRC.trimStart().startsWith("'use strict';") && /\(function \(\) \{/.test(SRC) && !/^(var|let|const|function) /m.test(SRC));
}

// ---------------------------------------------------------------- bootstrap

{
  const none = boot(null, { noHost: true });
  eq('no fleetHost: nothing registered', none.registered.size, 0);
  const nodock = boot(null, { noDock: true });
  eq('no fleetDock: no throw', nodock.registered.size, 0);
  const sandbox = { window: {}, document: createFakeDom().document };
  let threw = false;
  try {
    vm.runInContext(SRC, vm.createContext(sandbox));
  } catch (e) {
    threw = true;
  }
  check('empty window: no throw', !threw);
  threw = false;
  try {
    vm.runInContext(SRC, vm.createContext({}));
  } catch (e) {
    threw = true;
  }
  check('no window at all: no throw', !threw);

  const b = boot();
  eq('five kinds registered', [...b.registered.keys()].sort(), ['diff', 'explorer', 'file', 'image', 'markdown']);
  eq('explorer title', b.registered.get('explorer').title({ task: REF }), 'Explorer r1/api');
  eq('file title is the file name', b.registered.get('file').title({ task: REF, path: 'src/main.rs' }), 'File main.rs');
  check('title never throws on garbage', typeof b.registered.get('diff').title(null) === 'string' && typeof b.registered.get('image').title({ path: 5 }) === 'string');
  check('title escapes hostile names', !/[\u202e]/.test(b.registered.get('file').title({ task: REF, path: 'a\u202etxt.exe' })));
}

// ------------------------------------------------------- params validation

{
  const badTasks = ['', 'nope', 'a/b/c', 'a/', '/b', 'a b/c', '../x/y', 'a/b\u0000', 'x'.repeat(41) + '/y', 5, null, { a: 1 }, 'r1/api?x=1', 'r1/a%2fb'];
  const badCommits = ['HEAD', 'abc', 'g'.repeat(40), 'a'.repeat(39), 'a'.repeat(41), 'head ', '../base', 7];
  const badPaths = ['a\u0000b', 'a\\b', 'a\nb', 'a\u001bb', 'x'.repeat(1025), 'a\u007f', 7];
  for (const kind of ['explorer', 'file', 'diff', 'markdown', 'image']) {
    const b = boot();
    const base = kind === 'diff' ? { task: REF } : { task: REF, commit: 'head', path: 'a.txt' };
    for (const t of badTasks) {
      const m = await b.mount(kind, Object.assign({}, base, { task: t }));
      check(kind + ' refuses task ' + JSON.stringify(t), /Invalid task/.test(text(m.node)), text(m.node));
    }
    const commitKeys = kind === 'diff' ? ['base', 'head'] : ['commit'];
    for (const key of commitKeys) {
      for (const c of badCommits) {
        const m = await b.mount(kind, Object.assign({}, base, { [key]: c }));
        check(kind + ' refuses ' + key + ' ' + JSON.stringify(c), /Invalid commit/.test(text(m.node)), text(m.node));
      }
    }
    for (const pth of badPaths) {
      const key = kind === 'diff' ? 'file' : 'path';
      const m = await b.mount(kind, Object.assign({}, base, { [key]: pth }));
      check(kind + ' refuses path ' + JSON.stringify(pth).slice(0, 20), /Invalid path/.test(text(m.node)), text(m.node));
    }
    const m0 = await b.mount(kind, undefined);
    check(kind + ' refuses undefined params', /Invalid/.test(text(m0.node)));
    eq(kind + ' made no request for invalid params', b.calls().length, 0);
  }
  const b = boot();
  const m = await b.mount('file', { task: REF, commit: 'head', path: '' });
  check('file without a path says so, makes no request', /No file selected/.test(text(m.node)) && b.calls().length === 0);
  const e = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', [])) }));
  const me = await e.mount('explorer', { task: REF, commit: SHA, path: '' });
  check('explorer accepts empty path and sha', e.calls().length === 1 && !/Invalid/.test(text(me.node)));
  const e64 = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', [])) }));
  await e64.mount('explorer', { task: REF, commit: 'b'.repeat(64) });
  check('64 hex commit accepted', e64.calls().length === 1);
}

// ----------------------------------------------------------------- explorer

{
  const root = [
    ent('src', 'dir', 'src'),
    ent('docs', 'dir', 'docs'),
    ent('README.md', 'file', 'README.md', 20),
    ent('logo.png', 'file', 'logo.png', 100),
    ent('main.rs', 'file', 'main.rs', 5),
    ent('lnk', 'symlink', 'lnk'),
    ent('sub', 'submodule', 'sub'),
    ent('a\u202etxt.exe', 'file', null),
    ent('<img src=x onerror=alert(1)>', 'file', null),
    ent('\\u{202e}named', 'file', null)
  ];
  const src = [ent('lib.rs', 'file', 'src/lib.rs'), ent('inner', 'dir', 'src/inner')];
  const inner = [ent('deep.txt', 'file', 'src/inner/deep.txt')];
  let srcFail = false;
  const b = boot(
    routesOf({
      '/api/repo/tree': (q) => {
        const p = q.get('path');
        if (q.get('task') !== REF) return errDoc(400, 'invalid', 'bad task');
        if (p === '') return json(treeDoc('', root, { truncated: true, commit: q.get('commit') === 'base' ? SHA2 : SHA }));
        if (p === 'src') return srcFail ? errDoc(503, 'busy', 'busy now') : json(treeDoc('src', src));
        if (p === 'src/inner') return json(treeDoc('src/inner', inner));
        if (p === 'docs') return json(treeDoc('docs', []));
        return errDoc(404, 'not_found', 'gone');
      }
    }),
    { host: verif('failed', true) }
  );
  const m = await b.mount('explorer', { task: REF, commit: 'head', path: '' });
  const tree = m.node.byClass('tree')[0];
  const rowsOf = () => m.node.byClass('tree-row');
  eq('explorer requested the root once', b.calls(/tree/).length, 1);
  check('root request is encoded', b.calls(/tree/)[0] === '/api/repo/tree?task=r1%2Fapi&commit=head&path=', b.calls(/tree/)[0]);
  eq('tree role', tree.getAttribute('role'), 'tree');
  check('rows are treeitems', rowsOf().length === 10 + 1 && rowsOf().every((r) => r.getAttribute('role') === 'treeitem'), rowsOf().length);
  check('truncated notice shown', /Truncated/.test(text(rowsOf().at(-1))) && /limit 2000/.test(text(rowsOf().at(-1))));
  eq('dirs collapsed by default', rowsOf().at(0).getAttribute('aria-expanded'), 'false');
  check('kind labels', text(rowsOf().at(0).byClass('tree-kind')[0]) === 'dir' && text(rowsOf().at(2).byClass('tree-kind')[0]) === 'file' && text(rowsOf().at(5).byClass('tree-kind')[0]) === 'link' && text(rowsOf().at(6).byClass('tree-kind')[0]) === 'module');
  check('symlink labelled and not followed', /not followed/.test(text(rowsOf().at(5))));
  check('submodule labelled and not opened', /not opened/.test(text(rowsOf().at(6))));
  check('path:null rows are disabled', [7, 8, 9].every((i) => rowsOf().at(i).getAttribute('aria-disabled') === 'true' && rowsOf().at(i).hasClass('tree-disabled')));
  check('bidi name escaped', text(rowsOf().at(7).byClass('tree-name')[0]) === 'a[U+202E]txt.exe', text(rowsOf().at(7).byClass('tree-name')[0]));
  eq('markup name shown as text', text(rowsOf().at(8).byClass('tree-name')[0]), '<img src=x onerror=alert(1)>');
  check('header: task ref', text(m.node.byClass('pane-task')[0]) === REF);
  check('header: short real commit', text(m.node.byClass('pane-sha')[0]) === 'commit ' + SHA.slice(0, 10), text(m.node.byClass('pane-sha')[0]));
  check('header: sentence', text(m.node).includes('committed state of the repository, not the worktree'));
  const vf = m.node.byClass('badge').filter((x) => x.hasClass('vf-failed'));
  check('header: verification badge', vf.length === 1 && text(vf[0]) === 'failed');
  check('header: dirty badge', m.node.byClass('pane-dirty').length === 1 && text(m.node.byClass('pane-dirty')[0]) === 'dirty');
  const headBtn = m.node.byClass('pane-commits')[0].byTag('button');
  check('commit selector pressed state', headBtn[0].getAttribute('aria-pressed') === 'true' && headBtn[1].getAttribute('aria-pressed') === 'false');
  eq('breadcrumbs: root only', m.node.byClass('pane-crumbs')[0].byTag('button').length, 1);
  eq('title set', m.titles.at(-1), 'Explorer r1/api');
  eq('exactly one tabbable row', rowsOf().filter((r) => r.getAttribute('tabindex') === '0').length, 1);
  eq('first entry is the tab stop', rowsOf().at(0).getAttribute('tabindex'), '0');

  const key = (k, extra) => tree.dispatch('keydown', Object.assign({ key: k }, extra || {}));
  // expand, collapse, re-expand reuses the cache
  let ev = key('ArrowRight');
  check('Right preventDefault', ev.defaultPrevented);
  await settle(10);
  eq('src requested once', b.calls(/path=src$/).length, 1);
  eq('src expanded', rowsOf().at(0).getAttribute('aria-expanded'), 'true');
  check('children shown at depth 2', rowsOf().at(1).getAttribute('aria-level') === '2' && text(rowsOf().at(1).byClass('tree-name')[0]) === 'lib.rs');
  check('focus kept on the active row', b.dom.activeElement === rowsOf().at(0), 'active ' + (b.dom.activeElement && b.dom.activeElement.className));
  key('ArrowLeft');
  eq('Left collapses', rowsOf().at(0).getAttribute('aria-expanded'), 'false');
  key('ArrowRight');
  await settle(5);
  eq('re-expand uses the cache', b.calls(/path=src$/).length, 1);
  key('ArrowRight'); // already open: moves to the first child
  eq('Right on an open dir moves in', rowsOf().filter((r) => r.getAttribute('tabindex') === '0')[0] === rowsOf().at(1), true);
  key('ArrowLeft'); // child: moves to the parent
  eq('Left on a child moves to the parent', rowsOf().at(0).getAttribute('tabindex'), '0');
  key('ArrowDown');
  key('ArrowDown');
  key('ArrowUp');
  eq('arrows move', rowsOf().at(1).getAttribute('tabindex'), '0');
  key('ArrowDown'); // inner
  key('Enter'); // expand inner
  await settle(10);
  eq('Enter expands a directory', b.calls(/path=src%2Finner$/).length, 1);
  check('nested rows at depth 3', rowsOf().some((r) => r.getAttribute('aria-level') === '3'));
  key('Home');
  eq('Home', rowsOf().at(0).getAttribute('tabindex'), '0');
  key('End');
  const act = rowsOf().filter((r) => r.getAttribute('tabindex') === '0')[0];
  check('End goes to the last entry, not the notice', text(act.byClass('tree-name')[0]) === '\\u{202e}named', text(act));
  // enter on disabled / symlink / submodule opens nothing
  const rowIdx = (name) => rowsOf().findIndex((r) => r.byClass('tree-name').length && text(r.byClass('tree-name')[0]) === name);
  const goTo = (name) => {
    key('Home');
    for (let i = 0; i < 40 && text(rowsOf().filter((r) => r.getAttribute('tabindex') === '0')[0].byClass('tree-name')[0]) !== name; i += 1) key('ArrowDown');
  };
  goTo('a[U+202E]txt.exe');
  key('Enter');
  goTo('lnk');
  key('Enter');
  goTo('sub');
  key('Enter');
  eq('inert rows open nothing', b.opened.length, 0);
  // files
  goTo('main.rs');
  key('Enter');
  eq('Enter opens a file in the file pane', b.opened.at(-1), { kind: 'file', params: { task: REF, commit: 'head', path: 'main.rs' }, opts: { reuse: 'file' } });
  goTo('logo.png');
  key('Enter');
  eq('images open the image pane', b.opened.at(-1).kind, 'image');
  goTo('README.md');
  key('Enter');
  eq('md Enter opens the file', b.opened.at(-1).kind, 'file');
  key('p');
  eq('p previews markdown', b.opened.at(-1), { kind: 'markdown', params: { task: REF, commit: 'head', path: 'README.md' }, opts: { reuse: 'markdown' } });
  const row = rowsOf().at(rowIdx('README.md'));
  const pv = row.byText('Preview', 'button');
  check('md row has a Preview button', pv.length === 1);
  const before = b.opened.length;
  pv[0].dispatch('click');
  eq('Preview button opens markdown only', [b.opened.length - before, b.opened.at(-1).kind], [1, 'markdown']);
  const mainRow = rowsOf().at(rowIdx('main.rs'));
  check('non-md file has no Preview', mainRow.byText('Preview', 'button').length === 0);
  mainRow.dispatch('click');
  eq('click opens a file', b.opened.at(-1).params.path, 'main.rs');
  // diff button
  m.node.byText('Diff', 'button')[0].dispatch('click');
  eq('Diff button', b.opened.at(-1), { kind: 'diff', params: { task: REF, base: 'base', head: 'head' }, opts: { reuse: 'diff' } });
  // breadcrumbs: re-root
  const srcRow = rowsOf().at(rowIdx('src'));
  check('keys with a modifier are ignored', !key('ArrowDown', { ctrlKey: true }).defaultPrevented);
  // commit switch clears the cache and reloads
  const treeCalls = b.calls(/tree/).length;
  m.node.byClass('pane-commits')[0].byTag('button')[1].dispatch('click');
  await settle(10);
  check('commit switch re-requests the root with base', b.calls(/tree/).length === treeCalls + 1 && /commit=base/.test(b.calls(/tree/).at(-1)));
  eq('commit switch collapses dirs', rowsOf().at(0).getAttribute('aria-expanded'), 'false');
  check('real commit of base shown', text(m.node.byClass('pane-sha')[0]) === 'commit ' + SHA2.slice(0, 10));
  m.node.byClass('tree')[0].dispatch('keydown', { key: 'ArrowRight' });
  await settle(10);
  check('src is requested again after the switch', b.calls(/path=src$/).length === 2);
  // error on a directory is a note and a retry on re-expand
  srcFail = true;
  m.node.byClass('tree')[0].dispatch('keydown', { key: 'ArrowLeft' });
  m.inst.setParams({ task: REF, commit: 'head', path: '' });
  await settle(10);
  m.node.byClass('tree')[0].dispatch('keydown', { key: 'ArrowRight' });
  await settle(10);
  check('directory error shown as a note', m.node.byClass('tree-note').some((n) => /busy now/.test(text(n)) && n.hasClass('bad')));
  srcFail = false;
  m.node.byClass('tree')[0].dispatch('keydown', { key: 'ArrowLeft' });
  m.node.byClass('tree')[0].dispatch('keydown', { key: 'ArrowRight' });
  await settle(10);
  check('retry after collapse works', m.node.byClass('tree-row').some((r) => text(r).includes('lib.rs')));
  m.inst.stop();
  check('hostile dir markup produced no element', b.dom.created.every((t) => t !== 'script' && t !== 'iframe'));
  eq('no session storage use', b.storage.log.length, 0);
}

// explorer: breadcrumbs and path param, empty dir, 404 head, stale responses
{
  const b = boot(
    routesOf({
      '/api/repo/tree': (q) => {
        const p = q.get('path');
        if (p === 'src/inner') return json(treeDoc(p, [ent('deep.txt', 'file', 'src/inner/deep.txt')]));
        if (p === 'src') return json(treeDoc(p, []));
        if (p === '') return json(treeDoc('', [ent('src', 'dir', 'src')]));
        return errDoc(404, 'not_found', 'gone');
      }
    })
  );
  const m = await b.mount('explorer', { task: REF, commit: 'head', path: 'src/inner' });
  const crumbs = m.node.byClass('pane-crumbs')[0].byTag('button').map(text);
  eq('breadcrumbs for a path', crumbs, ['repository root', 'src', 'inner']);
  check('shows the dir at the path', text(m.node).includes('deep.txt'));
  m.node.byClass('pane-crumbs')[0].byTag('button')[1].dispatch('click');
  await settle(10);
  check('breadcrumb re-roots', text(m.node).includes('This directory is empty'), text(m.node));
  m.node.byClass('pane-crumbs')[0].byTag('button')[0].dispatch('click');
  await settle(10);
  check('root crumb shows the root', text(m.node.byClass('tree-name')[0]) === 'src');

  const nf = boot(routesOf({ '/api/repo/tree': () => errDoc(404, 'not_found', 'task r1/api has no recorded head commit') }));
  const mn = await nf.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('404 on head is an info notice', mn.node.byClass('pane-notice').some((n) => n.hasClass('info') && /No commit is recorded/.test(text(n)) && /no recorded head/.test(text(n))));
  const bad = boot(routesOf({ '/api/repo/tree': () => errDoc(503, 'busy', 'daemon busy') }));
  const mb = await bad.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('503 is an error notice', mb.node.byClass('pane-notice').some((n) => n.hasClass('bad') && /daemon busy/.test(text(n))));
  const wrong = boot(routesOf({ '/api/repo/tree': () => json({ schema: 'something/1' }) }));
  const mw = await wrong.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('wrong schema refused', /unexpected response/.test(text(mw.node)));

  // stale response is ignored after a commit switch
  let release;
  const gate = new Promise((r) => (release = r));
  const stale = boot(async (url) => {
    const { q } = qp(url);
    if (q.get('commit') === 'head') await gate;
    return json(treeDoc('', [ent(q.get('commit') === 'head' ? 'old.txt' : 'new.txt', 'file')]));
  });
  const ms = await stale.mount('explorer', { task: REF, commit: 'head', path: '' });
  ms.inst.setParams({ task: REF, commit: 'base', path: '' });
  await settle(10);
  release();
  await settle(10);
  check('stale response ignored', text(ms.node).includes('new.txt') && !text(ms.node).includes('old.txt'));
}

// explorer: huge directory is capped
{
  const many = [];
  for (let i = 0; i < 5000; i += 1) many.push(ent('f' + i, 'file'));
  const b = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', many)) }));
  const m = await b.mount('explorer', { task: REF, commit: 'head', path: '' });
  const n = m.node.byClass('tree-row').length;
  check('huge tree capped', n <= 2100 && n >= 2000, n);
  check('huge tree shows truncated notice', m.node.byClass('tree-note').some((x) => /Truncated/.test(text(x))));
}

// --------------------------------------------------------------------- file

{
  const lines = [];
  for (let i = 1; i <= 1200; i += 1) lines.push('line ' + i + ' <b>x</b>');
  const hostile = ['<img src=x onerror=alert(1)>', '<script>alert(1)</script>', '\u001b[31mred\u001b[0m', 'a\u202etxt', 'tab\there', ''];
  const docs = {
    'src/main.rs': fileDoc('src/main.rs', { markdown: true, lines: hostile, line_count: hostile.length }),
    'big.txt': fileDoc('big.txt', { lines, line_count: 1200, truncated: true, clipped: 2, lossy: true, crlf: true, notice: 'server says hi' }),
    'bin.dat': fileDoc('bin.dat', { view: 'binary', class: 'binary', lines: [], size: 4096 }),
    'huge.log': fileDoc('huge.log', { view: 'too_large', class: 'text', lines: [], size: 5242880, limit: 1048576 }),
    lnk: fileDoc('lnk', { view: 'symlink', kind: 'symlink', lines: [], target: '../x/<b>y', target_path: 'x/y' }),
    lnk2: fileDoc('lnk2', { view: 'symlink', kind: 'symlink', lines: [], target: '/etc/passwd', target_path: null }),
    sub: fileDoc('sub', { view: 'submodule', kind: 'submodule', lines: [], submodule_commit: SHA }),
    'logo.png': fileDoc('logo.png', { view: 'image', class: 'image', image: 'png', lines: [] }),
    'empty.txt': fileDoc('empty.txt', { lines: [], line_count: 0, size: 0 })
  };
  const b = boot(
    routesOf({
      '/api/repo/file': (q) => {
        const d = docs[q.get('path')];
        if (q.get('path') === 'huge-lines') {
          const ls = [];
          for (let i = 0; i < 12000; i += 1) ls.push('l' + i);
          return json(fileDoc('huge-lines', { lines: ls, line_count: 12000 }));
        }
        return d ? json(d) : errDoc(404, 'not_found', 'no such file');
      },
      '/api/repo/blob': () => pngResponse(PNG)
    }),
    { host: verif('verified', false) }
  );
  const open = (p, extra) => b.mount('file', Object.assign({ task: REF, commit: 'head', path: p }, extra || {}));
  let m = await open('src/main.rs');
  const rows = m.node.byClass('code-row');
  eq('hostile lines: row count', rows.length, 6);
  eq('gutter numbers', rows.map((r) => text(r.byClass('code-no')[0])), ['1', '2', '3', '4', '5', '6']);
  check('hostile lines are text', text(rows[0].byClass('code-text')[0]) === '<img src=x onerror=alert(1)>' && text(rows[1].byClass('code-text')[0]) === '<script>alert(1)</script>');
  check('ANSI escape visible', text(rows[2].byClass('code-text')[0]) === '[U+001B][31mred[U+001B][0m');
  check('bidi visible', text(rows[3].byClass('code-text')[0]) === 'a[U+202E]txt');
  eq('tab preserved', text(rows[4].byClass('code-text')[0]), 'tab\there');
  check('gutter is a separate cell', text(rows[5].byClass('code-text')[0]) === '' && text(rows[5].byClass('code-no')[0]) === '6');
  check('header shows sha and verification', text(m.node.byClass('pane-sha')[0]).includes(SHA.slice(0, 10)) && m.node.byClass('vf-verified').length === 1 && m.node.byClass('pane-dirty').length === 0);
  check('sentence', text(m.node).includes('committed state of the repository, not the worktree'));
  eq('file request', b.calls(/file/).at(-1), '/api/repo/file?task=r1%2Fapi&commit=head&path=src%2Fmain.rs');
  const pv = m.node.byText('Preview', 'button');
  eq('markdown file has Preview', pv.length, 1);
  pv[0].dispatch('click');
  eq('Preview opens markdown', b.opened.at(-1), { kind: 'markdown', params: { task: REF, commit: 'head', path: 'src/main.rs' }, opts: { reuse: 'markdown' } });
  eq('title is the file name', m.titles.at(-1), 'main.rs');

  m = await open('big.txt');
  eq('first chunk is 500 rows', m.node.byClass('code-row').length, 500);
  const more = () => m.node.byClass('code-more')[0].byTag('button');
  eq('show more button', more().length, 1);
  check('show more says how many remain', /500 of 700/.test(text(more()[0])), text(more()[0]));
  more()[0].dispatch('click');
  eq('second chunk', m.node.byClass('code-row').length, 1000);
  more()[0].dispatch('click');
  eq('all rows', m.node.byClass('code-row').length, 1200);
  eq('no more button at the end', more().length, 0);
  const notes = m.node.byClass('pane-notice').map(text).join('|');
  check('truncated notice', /showing the first 1200 of 1200/i.test(notes), notes);
  check('clipped notice', /2 very long line/.test(notes));
  check('lossy notice', /not valid UTF-8/.test(notes));
  check('crlf notice', /CRLF/.test(notes));
  check('server notice', /server says hi/.test(notes));

  m = await open('big.txt', { line: 900 });
  eq('line=900 renders enough rows', m.node.byClass('code-row').length, 1000);
  const hl = m.node.byClass('hl');
  check('line highlight', hl.length === 1 && text(hl[0].byClass('code-no')[0]) === '900' && hl[0].getAttribute('aria-current') === 'true');
  check('scrolled to the line', b.dom.scrolled.some((s) => s.el === hl[0]));
  m = await open('big.txt', { line: '3' });
  check('line as digit string', m.node.byClass('hl').length === 1 && text(m.node.byClass('hl')[0].byClass('code-no')[0]) === '3');
  m = await open('big.txt', { line: -4 });
  eq('invalid line ignored', m.node.byClass('hl').length, 0);
  m = await open('big.txt', { line: 'abc' });
  eq('non numeric line ignored', m.node.byClass('hl').length, 0);

  m = await open('huge-lines');
  for (let i = 0; i < 40 && m.node.byClass('code-more')[0].byTag('button').length; i += 1) m.node.byClass('code-more')[0].byTag('button')[0].dispatch('click');
  eq('hard cap of rendered rows', m.node.byClass('code-row').length, 10000);
  check('hard cap notice', /Only the first 10000 lines/.test(text(m.node.byClass('code-more')[0])));
  m = await open('huge-lines', { line: 11000 });
  check('line beyond the cap is flagged', /beyond the rendered limit/.test(text(m.node)) && m.node.byClass('hl').length === 0);

  m = await open('bin.dat');
  check('binary placeholder', /Binary file, not shown/.test(text(m.node)) && /4\.0 KiB/.test(text(m.node)) && m.node.byClass('code-row').length === 0);
  m = await open('huge.log');
  check('too_large placeholder shows size and limit', /too large/.test(text(m.node)) && /5\.0 MiB/.test(text(m.node)) && /1\.0 MiB/.test(text(m.node)), text(m.node));
  m = await open('empty.txt');
  check('empty file note', /file is empty/.test(text(m.node)));

  m = await open('lnk');
  check('symlink shows the target as text', text(m.node.byClass('file-target')[0]) === '../x/<b>y');
  const callsBefore = b.calls().length;
  const openedBefore = b.opened.length;
  check('symlink target is never opened automatically', b.opened.length === openedBefore && b.calls().length === callsBefore);
  const ot = m.node.byText('Open target', 'button');
  eq('Open target button', ot.length, 1);
  ot[0].dispatch('click');
  eq('Open target opens on click', b.opened.at(-1), { kind: 'file', params: { task: REF, commit: 'head', path: 'x/y' }, opts: { reuse: 'file' } });
  m = await open('lnk2');
  check('symlink with no target path has no button', m.node.byText('Open target', 'button').length === 0 && /never followed/.test(text(m.node)));
  m = await open('sub');
  check('submodule shows the commit id', text(m.node.byClass('file-target')[0]) === SHA && /not opened/.test(text(m.node)));

  m = await open('logo.png');
  const imgs = m.node.byTag('img');
  eq('image view creates one img', imgs.length, 1);
  check('image src is a data URL of the bytes', imgs[0].getAttribute('src') === 'data:image/png;base64,' + Buffer.from(PNG).toString('base64'));
  check('image alt is the file name', imgs[0].getAttribute('alt') === 'logo.png');
  check('blob request is encoded', b.calls(/blob/).at(-1) === '/api/repo/blob?task=r1%2Fapi&commit=head&path=logo.png');
  imgs[0].naturalWidth = 16;
  imgs[0].naturalHeight = 9;
  imgs[0].dispatch('load');
  check('dimensions shown after load', /16 \u00d7 9 px/.test(text(m.node)));
  m = await open('missing.txt');
  check('404 not found message', /no such file/.test(text(m.node)));
  // commit selector on a file pane
  m = await open('src/main.rs');
  m.node.byClass('pane-commits')[0].byTag('button')[1].dispatch('click');
  await settle(10);
  check('file commit switch re-requests', /commit=base/.test(b.calls(/file/).at(-1)));
}

// --------------------------------------------------------------------- diff

function dline(k, o, n, t, extra) {
  return Object.assign({ k, o, n, t }, extra || {});
}
function mkFile(p, extra) {
  return Object.assign(
    { path: p, old_path: null, path_ok: true, old_path_ok: false, status: 'modified', similarity: null, old_mode: '100644', new_mode: '100644', binary: false, submodule: false, additions: 1, deletions: 1, truncated: false, note: null, hunks: [] },
    extra || {}
  );
}
function diffDoc(files, extra) {
  return Object.assign(
    { schema: 'fleet.repo.diff/1', task: REF, base: SHA2, head: SHA, context: 3, stats: { files: files.length, additions: 10, deletions: 3, binary: 1 }, truncated: false, notice: null, files },
    extra || {}
  );
}
{
  const ctxRun = [];
  for (let i = 0; i < 20; i += 1) ctxRun.push(dline('ctx', 10 + i, 10 + i, 'same ' + i));
  const hunkA = {
    old_start: 1,
    old_count: 3,
    new_start: 1,
    new_count: 4,
    section: 'fn <main>()',
    lines: [
      dline('ctx', 1, 1, 'fn main() {'),
      dline('del', 2, null, 'old line one'),
      dline('del', 3, null, 'old line two', { w: [[0, 3], [100, 120], [5, 2], 'x', [1.5, 2], [-1, 2]] }),
      dline('add', null, 2, 'new line one', { cr: true, nonl: true, w: [[0, 3]] }),
      dline('add', null, 3, 'new line two'),
      dline('add', null, 4, '<script>alert(1)</script>', { w: [[0, 8]] }),
      dline('ctx', 4, 5, '}')
    ]
  };
  const hunkB = {
    old_start: 30,
    old_count: 24,
    new_start: 31,
    new_count: 24,
    section: '',
    lines: [dline('del', 30, null, 'x'), dline('add', null, 31, 'y'), ...ctxRun, dline('del', 50, null, 'z'), dline('add', null, 51, 'zz')]
  };
  const files = [
    mkFile('src/a.rs', { additions: 4, deletions: 2, hunks: [hunkA, hunkB] }),
    mkFile('img/logo.png', { binary: true, status: 'modified', additions: 0, deletions: 0 }),
    mkFile('new/name.rs', { old_path: 'old/name.rs', old_path_ok: true, status: 'renamed', similarity: 87, additions: 0, deletions: 0 }),
    mkFile('run.sh', { status: 'mode_changed', old_mode: '100644', new_mode: '100755', additions: 0, deletions: 0 }),
    mkFile('link', { status: 'type_changed', old_mode: '100644', new_mode: '120000', additions: 0, deletions: 0 }),
    mkFile('vendor/lib', { submodule: true, additions: 1, deletions: 1, note: 'submodule ' + SHA2.slice(0, 8) + ' to ' + SHA.slice(0, 8) }),
    mkFile('big.txt', { truncated: true, note: 'showing the first 4000 lines of 9000', hunks: [{ old_start: 1, old_count: 1, new_start: 1, new_count: 1, section: null, lines: [dline('add', null, 1, 'q')] }] }),
    mkFile('a\u202etxt.exe', { path_ok: false, status: 'added', additions: 1, deletions: 0, hunks: [{ old_start: 0, old_count: 0, new_start: 1, new_count: 1, section: '', lines: [dline('add', null, 1, '<img src=x onerror=alert(1)>')] }] }),
    mkFile('gone.txt', { status: 'deleted', additions: 0, deletions: 1, hunks: [{ old_start: 1, old_count: 1, new_start: 0, new_count: 0, section: '', lines: [dline('del', 1, null, 'bye')] }] })
  ];
  const requests = [];
  const b = boot(
    routesOf({
      '/api/repo/diff': (q, url) => {
        requests.push(url);
        return json(diffDoc(files, { truncated: true, notice: 'only the first 9 files were listed' }));
      }
    }),
    { host: verif('unverified', true) }
  );
  const m = await b.mount('diff', { task: REF, base: 'base', head: 'head' });
  eq('diff request', requests.at(0), '/api/repo/diff?task=r1%2Fapi&base=base&head=head&context=3');
  check('header shows both short commits', text(m.node.byClass('pane-sha')[0]) === 'base ' + SHA2.slice(0, 10) + ' \u2192 head ' + SHA.slice(0, 10), text(m.node.byClass('pane-sha')[0]));
  check('header sentence + badges', text(m.node).includes('committed state of the repository, not the worktree') && m.node.byClass('vf-unverified').length === 1 && m.node.byClass('pane-dirty').length === 1);
  const names = () => m.node.byClass('diff-file').map((x) => text(x.byClass('diff-name')[0]));
  eq('file list', names(), ['src/a.rs', 'img/logo.png', 'new/name.rs', 'run.sh', 'link', 'vendor/lib', 'big.txt', 'a[U+202E]txt.exe', 'gone.txt']);
  const f0 = m.node.byClass('diff-file')[0];
  check('file list has status badge and counts', f0.byClass('ds-modified').length === 1 && text(f0).includes('+4') && text(f0).includes('-2'));
  check('list markers', /binary/.test(text(m.node.byClass('diff-file')[1])) && /submodule/.test(text(m.node.byClass('diff-file')[5])) && /truncated/.test(text(m.node.byClass('diff-file')[6])) && /not openable/.test(text(m.node.byClass('diff-file')[7])));
  check('top-level truncated notice', m.node.byClass('pane-notice').some((n) => /Truncated/.test(text(n)) && /only the first 9 files/.test(text(n))));
  check('summary', /9 files, \+10 -3, 1 binary/.test(text(m.node.byClass('pane-summary')[0])));
  // unified
  const urows = () => m.node.byClass('diff-row');
  eq('unified rows of file 1', urows().length, 7 + 2 + 20 + 2);
  eq('hunk headers', m.node.byClass('diff-hunk').map(text), ['@@ -1,3 +1,4 @@ fn <main>()', '@@ -30,24 +31,24 @@']);
  const r1 = urows()[1];
  check('del row classes', r1.hasClass('diff-del') && text(r1.byClass('dsign')[0]) === '-' && text(r1.byClass('dno')[0]) === '2' && text(r1.byClass('dno')[1]) === '');
  const r3 = urows()[3];
  check('add row', r3.hasClass('diff-add') && text(r3.byClass('dno')[1]) === '2' && text(r3.byClass('dsign')[0]) === '+');
  check('cr and no newline markers', /\u240d/.test(text(r3)) && /no newline at end of file/.test(text(r3)));
  const w3 = r3.byClass('dw');
  check('intra-line range rendered as a span', w3.length === 1 && text(w3[0]) === 'new');
  check('line text split into pieces', r3.byClass('dtext')[0].childNodes[1].data === ' line one', r3.byClass('dtext')[0].childNodes.map((c) => c.textContent).join('|'));
  const w2 = urows()[2].byClass('dw');
  check('invalid ranges ignored, valid kept', w2.length === 1 && text(w2[0]) === 'old', w2.map(text).join('|'));
  check('script text stays text', urows()[5].byClass('dtext')[0].textContent.startsWith('<script>alert(1)</script>') && b.dom.created.every((t) => t !== 'script'));
  check('script text partially marked', text(urows()[5].byClass('dw')[0]) === '<script>');
  check('section header is escaped text', text(m.node.byClass('diff-hunk')[0]).includes('fn <main>()'));
  // keyboard: hunks
  const keyOn = (k, extra) => m.node.dispatch('keydown', Object.assign({ key: k, target: m.node }, extra || {}));
  const hunks = m.node.byClass('diff-hunk');
  let ev = keyOn('n');
  check('n focuses the first hunk', ev.defaultPrevented && b.dom.activeElement === hunks[0] && b.dom.scrolled.at(-1).el === hunks[0]);
  keyOn(']');
  check('] goes to the next hunk', b.dom.activeElement === hunks[1]);
  keyOn('n');
  check('n stays on the last hunk', b.dom.activeElement === hunks[1]);
  keyOn('p');
  check('p goes back', b.dom.activeElement === hunks[0]);
  keyOn('[');
  check('[ clamps at the first', b.dom.activeElement === hunks[0]);
  // keyboard: files
  keyOn('j');
  check('j selects the next file', m.node.byClass('diff-file')[1].getAttribute('aria-selected') === 'true' && /Binary file, not shown/.test(text(m.node.byClass('diff-view')[0])));
  keyOn('k');
  check('k selects the previous', m.node.byClass('diff-file')[0].hasClass('selected'));
  keyOn('k');
  check('k clamps at the first', m.node.byClass('diff-file')[0].hasClass('selected'));
  keyOn('ArrowDown');
  keyOn('ArrowDown');
  check('Down moves between files: rename notice', /Renamed: old\/name\.rs \u2192 new\/name\.rs \(87% similar\)/.test(text(m.node.byClass('diff-view')[0])), text(m.node.byClass('diff-view')[0]));
  keyOn('ArrowDown');
  check('mode-only notice', /Mode changed: 100644 \u2192 100755/.test(text(m.node.byClass('diff-view')[0])));
  keyOn('ArrowDown');
  check('type change notice', /Type changed: 100644 \u2192 120000/.test(text(m.node.byClass('diff-view')[0])));
  keyOn('ArrowDown');
  const sv = text(m.node.byClass('diff-view')[0]);
  check('submodule note', /Submodule change/.test(sv) && sv.includes('submodule ' + SHA2.slice(0, 8)) && m.node.byClass('diff-view')[0].byText('Open file', 'button')[0].disabled === true);
  keyOn('ArrowDown');
  check('per-file truncated note', /showing the first 4000 lines of 9000/.test(text(m.node.byClass('diff-view')[0])) && m.node.byClass('diff-view')[0].byClass('warn').length >= 1);
  keyOn('j');
  const hv = m.node.byClass('diff-view')[0];
  check('path_ok false is not openable', hv.byText('Open file', 'button')[0].disabled === true && hv.byText('Open file', 'button')[0].getAttribute('disabled') === 'disabled');
  check('hostile path shown escaped', text(hv.byClass('diff-path')[0]) === 'a[U+202E]txt.exe');
  hv.byText('Open file', 'button')[0].dispatch('click');
  eq('disabled Open file opens nothing', b.opened.length, 0);
  keyOn('j');
  check('deleted file is not openable', m.node.byClass('diff-view')[0].byText('Open file', 'button')[0].disabled === true);
  keyOn('j');
  check('j clamps at the last file', m.node.byClass('diff-file')[8].hasClass('selected'));
  m.node.byClass('diff-file')[0].dispatch('click');
  const openBtn = m.node.byClass('diff-view')[0].byText('Open file', 'button')[0];
  openBtn.dispatch('click');
  eq('Open file opens at the head commit', b.opened.at(-1), { kind: 'file', params: { task: REF, commit: 'head', path: 'src/a.rs' }, opts: { reuse: 'file' } });
  // ignored keys
  const preSel = m.node.byClass('diff-file').findIndex((x) => x.hasClass('selected'));
  keyOn('j', { ctrlKey: true });
  keyOn('j', { target: { tagName: 'INPUT' } });
  keyOn('x');
  eq('modifier / input / unknown keys ignored', m.node.byClass('diff-file').findIndex((x) => x.hasClass('selected')), preSel);
  // split mode
  ev = keyOn('u');
  check('u toggles to split', ev.defaultPrevented && m.node.byClass('diff-srow').length > 0 && m.node.byClass('diff-row').length === 0);
  const srows = () => m.node.byClass('diff-srow');
  eq('split rows of file 1', srows().length, 1 + 3 + 1 + 1 + 20 + 1);
  const cells = (i) => srows()[i].byClass('diff-cell');
  check('context on both sides', cells(0)[0].hasClass('diff-ctx') && cells(0)[1].hasClass('diff-ctx') && text(cells(0)[0].byClass('dno')[0]) === '1' && text(cells(0)[1].byClass('dno')[0]) === '1');
  check('del/add runs paired row by row', cells(1)[0].hasClass('diff-del') && cells(1)[1].hasClass('diff-add') && text(cells(1)[0].byClass('dno')[0]) === '2' && text(cells(1)[1].byClass('dno')[0]) === '2' && cells(2)[0].hasClass('diff-del') && cells(2)[1].hasClass('diff-add'));
  check('leftover add pairs with an empty cell', cells(3)[0].hasClass('diff-empty') && cells(3)[1].hasClass('diff-add') && text(cells(3)[1].byClass('dno')[0]) === '4');
  check('old numbers left, new right', text(cells(2)[0].byClass('dno')[0]) === '3' && text(cells(2)[1].byClass('dno')[0]) === '3');
  check('split keeps intra-line spans', cells(1)[1].byClass('dw').length === 1);
  keyOn('n');
  check('hunk keys work in split', b.dom.activeElement && b.dom.activeElement.hasClass('diff-hunk'));
  m.node.byText('Unified', 'button')[0].dispatch('click');
  check('mode button toggles back', m.node.byClass('diff-row').length > 0 && m.node.byText('Unified', 'button')[0].getAttribute('aria-pressed') === 'true');
  // fold
  const fold = m.node.byText('Collapse unchanged context', 'button')[0];
  fold.dispatch('click');
  const folds = m.node.byClass('diff-fold');
  eq('one fold button for the long run', folds.length, 1);
  check('fold says how many lines', text(folds[0]) === '14 unchanged lines', text(folds[0]));
  check('fold keeps 3 lines around changes', m.node.byClass('diff-row').length === 7 + 2 + 6 + 2);
  folds[0].dispatch('click');
  check('fold expands in place', m.node.byClass('diff-fold').length === 0 && m.node.byClass('diff-row').length === 7 + 2 + 20 + 2);
  check('collapse toggle state', fold.getAttribute('aria-pressed') === 'true');
  fold.dispatch('click');
  check('toggle off unfolds', m.node.byClass('diff-fold').length === 0);
  // context selector
  const ctxBtn = (n) => m.node.byClass('pane-tools')[0].byText(String(n), 'button')[0];
  ctxBtn(10).dispatch('click');
  await settle(10);
  check('context re-requests', /context=10$/.test(requests.at(-1)), requests.at(-1));
  check('selection survives the reload', m.node.byClass('diff-file')[0].hasClass('selected') && ctxBtn(10).getAttribute('aria-pressed') === 'true');
  ctxBtn(0).dispatch('click');
  await settle(10);
  ctxBtn(25).dispatch('click');
  await settle(10);
  check('context 0 and 25', requests.some((r) => /context=0$/.test(r)) && requests.some((r) => /context=25$/.test(r)));
  // deep link params: mode and file
  const m2 = await b.mount('diff', { task: REF, base: 'base', head: SHA, mode: 'split', file: 'run.sh', context: 10 });
  check('mode param honoured', m2.node.byText('Side by side', 'button')[0].getAttribute('aria-pressed') === 'true');
  check('file param selects the file', m2.node.byClass('diff-file')[3].hasClass('selected'));
  check('context param sent', /head=a1b2c3d4e5f60718293a4b5c6d7e8f9012345678&context=10$/.test(requests.at(-1)), requests.at(-1));
  const m3 = await b.mount('diff', { task: REF, mode: 'weird' });
  check('bad mode falls back to unified', m3.node.byText('Unified', 'button')[0].getAttribute('aria-pressed') === 'true');
  check('base and head default', /base=base&head=head/.test(requests.at(-1)));
  m.inst.stop();
}

// diff: bounded rendering, errors, empty, hostile
{
  const lines = [];
  for (let i = 0; i < 7000; i += 1) lines.push(dline(i % 2 ? 'add' : 'del', i % 2 ? null : i, i % 2 ? i : null, 'line ' + i));
  const hunks = [];
  for (let h = 0; h < 5; h += 1) hunks.push({ old_start: 1, old_count: 1, new_start: 1, new_count: 1, section: 's' + h, lines: lines.slice(h * 1400, (h + 1) * 1400) });
  const b = boot(routesOf({ '/api/repo/diff': () => json(diffDoc([mkFile('huge.txt', { hunks })])) }));
  const m = await b.mount('diff', { task: REF });
  eq('7000 lines render at most 3000 rows at a time', m.node.byClass('diff-row').length + m.node.byClass('diff-hunk').length, 3000);
  const more = () => m.node.byClass('code-more').at(-1).byTag('button');
  check('show more continuation', more().length === 1 && /remaining rows/.test(text(more()[0])), text(m.node.byClass('code-more').at(-1)));
  more()[0].dispatch('click');
  eq('continuation renders the next chunk', m.node.byClass('diff-row').length + m.node.byClass('diff-hunk').length, 6000);
  m.node.dispatch('keydown', { key: ']', target: m.node });
  m.node.dispatch('keydown', { key: ']', target: m.node });
  m.node.dispatch('keydown', { key: ']', target: m.node });
  m.node.dispatch('keydown', { key: ']', target: m.node });
  m.node.dispatch('keydown', { key: ']', target: m.node });
  check('hunk navigation renders the rows it needs', b.dom.activeElement === m.node.byClass('diff-hunk')[4], 'active ' + (b.dom.activeElement && b.dom.activeElement.className));
  m.node.byText('Side by side', 'button')[0].dispatch('click');
  check('split render is bounded too', m.node.byClass('diff-srow').length + m.node.byClass('diff-hunk').length <= 3000);

  const e = boot(routesOf({ '/api/repo/diff': () => errDoc(404, 'not_found', 'no head commit yet') }));
  const me = await e.mount('diff', { task: REF });
  check('diff 404 on head is an info notice', me.node.byClass('pane-notice').some((n) => n.hasClass('info') && /no head commit yet/.test(text(n))));
  const e2 = boot(routesOf({ '/api/repo/diff': () => json(diffDoc([])) }));
  const me2 = await e2.mount('diff', { task: REF });
  check('no files', /No changed files/.test(text(me2.node)));
  const e3 = boot(routesOf({ '/api/repo/diff': () => json(diffDoc([null, 5, 'x', { path: 7, hunks: 'zz', status: { a: 1 }, additions: 'lots' }, mkFile('ok.txt', { hunks: [{ lines: [null, { k: 'zzz' }, { k: 'add', t: 5, o: 'x', n: -1 }, { k: 'ctx', t: 'c', w: 'oops' }] }] })])) }));
  const me3 = await e3.mount('diff', { task: REF });
  check('garbage in the diff document does not throw', me3.node.byClass('diff-file').length === 2, me3.node.byClass('diff-file').length);
  me3.node.dispatch('keydown', { key: 'j', target: me3.node });
  check('garbage file renders', me3.node.byClass('diff-row').length === 2);
  const e4 = boot(routesOf({ '/api/repo/diff': () => json(diffDoc([mkFile('x', { hunks: [{ old_start: 1, old_count: 1, new_start: 1, new_count: 1, section: '', lines: [dline('add', null, 1, 'y'.repeat(9000), { w: [[0, 9000], [8999, 9001]] })] }] })])) }));
  const me4 = await e4.mount('diff', { task: REF });
  check('very long line is cut with a marker', /\[line cut\]/.test(text(me4.node.byClass('diff-row')[0])) && text(me4.node.byClass('diff-row')[0]).length < 4200);
}

// ----------------------------------------------------------------- markdown

const MD_TAGS = new Set(['h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'p', 'pre', 'code', 'blockquote', 'ul', 'ol', 'li', 'table', 'thead', 'tbody', 'tr', 'th', 'td', 'hr', 'em', 'strong', 'del', 'br', 'a', 'img', 'span', 'input']);
const MD_ATTRS = new Set(['class', 'href', 'rel', 'title', 'alt', 'src', 'start', 'type', 'disabled', 'checked', 'tabindex']);
const MD_CLASSES = new Set(['md-h', 'md-code', 'md-pre', 'md-table', 'md-al-left', 'md-al-center', 'md-al-right', 'md-task', 'md-check', 'md-link', 'md-external', 'md-repo', 'md-dead', 'md-image', 'md-alt', 'md-img']);
const DATA_URL = /^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/]*={0,2}$/;
const HTTP_URL = /^https?:\/\/[^\s]+$/i;

// the audit of a markdown subtree: returns a list of problems
function auditMd(root, label) {
  const problems = [];
  let nodes = 0;
  let maxDepth = 0;
  let imgs = 0;
  function walk(n, depth) {
    nodes += 1;
    maxDepth = Math.max(maxDepth, depth);
    if (n.nodeType === 3) return;
    if (!MD_TAGS.has(n.tag)) problems.push(label + ': tag ' + n.tag);
    if (n.tag === 'img') imgs += 1;
    for (const [k, v] of n.attrs) {
      if (!MD_ATTRS.has(k)) problems.push(label + ': attribute ' + k);
      if (k === 'href' && !HTTP_URL.test(v)) problems.push(label + ': href ' + v.slice(0, 60));
      if (k === 'src' && !DATA_URL.test(v)) problems.push(label + ': src ' + v.slice(0, 60));
      if (k === 'class') for (const t of v.split(/\s+/).filter(Boolean)) if (!MD_CLASSES.has(t)) problems.push(label + ': class ' + t);
      if (k === 'type' && !(n.tag === 'input' && v === 'checkbox')) problems.push(label + ': type on ' + n.tag);
      if (k === 'rel' && v !== 'noopener noreferrer') problems.push(label + ': rel ' + v);
      if (k === 'href' && n.tag !== 'a') problems.push(label + ': href on ' + n.tag);
      if (k === 'src' && n.tag !== 'img') problems.push(label + ': src on ' + n.tag);
    }
    if (n.tag === 'a' && n.attrs.has('href') && n.attrs.get('rel') !== 'noopener noreferrer') problems.push(label + ': a without rel');
    if (n.attrs.has('target')) problems.push(label + ': target');
    n.childNodes.forEach((c) => walk(c, depth + 1));
  }
  root.childNodes.forEach((c) => walk(c, 1));
  return { problems, nodes, maxDepth, imgs };
}

const HOSTILE_URLS = [
  'javascript:alert(1)', 'JaVaScRiPt:alert(1)', ' javascript:alert(1)', 'data:text/html,<script>alert(1)</script>', 'data:image/png;base64,AAAA', '//evil.example/x', '\\\\evil\\x',
  'vbscript:msgbox(1)', 'file:///etc/passwd', 'http:evil', 'https:\\evil', 'http://ok.example/\u202e', 'http://ok.example/a b', 'http://ok.example/\u0000', 'blob:x', 'ftp://x/y',
  'http://' + 'a'.repeat(3000), '', null, 5, { a: 1 }, ['http://x'], 'http://good.example/path?q=1#h', 'https://good.example/'
];

{
  const blocks = [
    { t: 'h', level: 1, c: [{ t: 'text', s: 'Title <b>' }] },
    { t: 'h', level: 3, c: [{ t: 'em', c: [{ t: 'text', s: 'x' }] }] },
    { t: 'p', c: [{ t: 'text', s: 'plain ' }, { t: 'strong', c: [{ t: 'text', s: 'bold' }] }, { t: 'del', c: [{ t: 'text', s: 'gone' }] }, { t: 'code', s: '<script>' }, { t: 'br' }, { t: 'text', s: 'next\u202e' }] },
    { t: 'code', lang: 'rust', text: 'fn main() {}\n<img src=x onerror=alert(1)>' },
    { t: 'quote', c: [{ t: 'p', c: [{ t: 'text', s: 'quoted' }] }] },
    { t: 'ul', c: [{ checked: null, c: [{ t: 'p', c: [{ t: 'text', s: 'one' }] }] }, { checked: true, c: [{ t: 'p', c: [{ t: 'text', s: 'done' }] }] }, { checked: false, c: [] }] },
    { t: 'ol', start: 5, c: [{ checked: null, c: [{ t: 'p', c: [{ t: 'text', s: 'five' }] }] }] },
    { t: 'table', align: ['left', 'center', 'right', null, 'bogus'], head: [[{ t: 'text', s: 'a' }], [{ t: 'text', s: 'b' }]], rows: [[[{ t: 'text', s: '1' }], [{ t: 'text', s: '2' }]]] },
    { t: 'hr' },
    { t: 'p', c: [
      { t: 'link', kind: 'external', url: 'https://good.example/a?b=1', title: 'T', c: [{ t: 'text', s: 'ext' }] },
      { t: 'link', kind: 'repo', path: 'docs/guide.md', url: null, title: null, c: [{ t: 'text', s: 'repo md' }] },
      { t: 'link', kind: 'repo', path: 'src/main.rs', url: null, title: null, c: [{ t: 'text', s: 'repo file' }] },
      { t: 'link', kind: 'external', url: 'javascript:alert(1)', c: [{ t: 'text', s: 'evil' }] },
      { t: 'image', alt: 'logo', path: 'img/logo.png', src: 'img/logo.png', note: null },
      { t: 'image', alt: 'remote', path: null, src: 'ftp://evil/x.png', note: 'remote images are not loaded' },
      { t: 'image', alt: 'broken', path: 'img/broken.png', src: 'img/broken.png', note: null },
      { t: 'image', alt: 'doc', path: 'img/doc.txt', src: 'img/doc.txt', note: null },
      { t: 'image', alt: 'huge', path: 'img/huge.png', src: 'img/huge.png', note: null },
      { t: 'image', alt: 'gone', path: 'img/gone.png', src: 'img/gone.png', note: null },
      { t: 'image', alt: 'html', path: 'img/html.png', src: 'img/html.png', note: null }
    ] },
    { t: 'mystery', c: [{ t: 'text', s: 'dropped' }] }
  ];
  const huge = new Uint8Array(4 * 1024 * 1024 + 1);
  const b = boot(
    routesOf({
      '/api/repo/markdown': () => json(mdDoc(blocks)),
      '/api/repo/blob': (q) => {
        const p = q.get('path');
        if (p === 'img/logo.png') return pngResponse(PNG);
        if (p === 'img/broken.png') return { status: 500, body: { schema: 'fleet.error/1', error: { code: 'x', message: 'boom' } } };
        if (p === 'img/doc.txt') return { status: 415, body: { schema: 'fleet.error/1', error: { code: 'unsupported', message: 'not an image' } } };
        if (p === 'img/huge.png') return pngResponse(huge);
        if (p === 'img/gone.png') return { status: 404, body: { schema: 'fleet.error/1', error: { code: 'not_found', message: 'gone' } } };
        if (p === 'img/html.png') return pngResponse(PNG, 'text/html');
        return { status: 404, body: null };
      }
    })
  );
  const m = await b.mount('markdown', { task: REF, commit: 'head', path: 'README.md' });
  const doc = m.node.byClass('md-doc')[0];
  const audit = auditMd(doc, 'md');
  eq('markdown audit is clean', audit.problems, []);
  eq('request', b.calls(/markdown/).at(-1), '/api/repo/markdown?task=r1%2Fapi&commit=head&path=README.md');
  check('headings', doc.byTag('h1').length === 1 && text(doc.byTag('h1')[0]) === 'Title <b>' && doc.byTag('h3').length === 1);
  check('inline elements', doc.byTag('strong').length === 1 && doc.byTag('del').length === 1 && doc.byTag('em').length === 1 && doc.byTag('br').length === 1);
  check('inline code text', text(doc.byClass('md-code')[0]) === '<script>');
  check('bidi in text escaped', text(doc.byTag('p')[0]).includes('next[U+202E]'));
  check('code block', text(doc.byTag('pre')[0]).includes('<img src=x onerror=alert(1)>') && doc.byTag('pre')[0].getAttribute('title') === 'rust' && doc.byTag('pre')[0].getAttribute('tabindex') === '0');
  check('quote, hr', doc.byTag('blockquote').length === 1 && doc.byTag('hr').length === 1);
  const boxes = doc.byTag('input');
  check('task items are disabled checkboxes', boxes.length === 2 && boxes.every((x) => x.getAttribute('type') === 'checkbox' && x.getAttribute('disabled') === 'disabled') && boxes[0].getAttribute('checked') === 'checked' && boxes[1].getAttribute('checked') === null);
  eq('ol start', doc.byTag('ol')[0].getAttribute('start'), '5');
  check('table', doc.byTag('th').length === 2 && doc.byTag('td').length === 2 && doc.byTag('thead').length === 1 && doc.byTag('tbody').length === 1);
  check('table alignment classes', doc.byTag('th')[0].hasClass('md-al-left') && doc.byTag('th')[1].hasClass('md-al-center') && !doc.byTag('th')[1].hasClass('md-al-right'));
  check('unknown node type renders nothing', !text(doc).includes('dropped'));
  // links
  const links = doc.byTag('a');
  eq('links', links.length, 3);
  const ext = links[0];
  eq('external href', ext.getAttribute('href'), 'https://good.example/a?b=1');
  eq('external rel', ext.getAttribute('rel'), 'noopener noreferrer');
  check('no target attribute', !ext.attrs.has('target'));
  check('title carries the target', ext.getAttribute('title') === 'T (https://good.example/a?b=1)', ext.getAttribute('title'));
  check('unsafe link has no anchor', !doc.byTag('a').some((a) => /alert/.test(a.getAttribute('href') || '')) && text(doc.byClass('md-dead')[0]) === 'evil');
  const status = m.node.byClass('md-status')[0];
  ext.dispatch('mouseover');
  check('hover shows the target', text(status) === 'Link: https://good.example/a?b=1', text(status));
  ext.dispatch('mouseout');
  eq('mouseout clears it', text(status), '');
  ext.dispatch('focus');
  check('focus shows the target', /good\.example/.test(text(status)));
  ext.dispatch('blur');
  let ev = ext.dispatch('click');
  check('click is prevented', ev.defaultPrevented);
  eq('click does not open', b.win.opens.length, 0);
  const bar = m.node.byClass('md-confirm')[0];
  check('confirmation bar with the full url as text', bar && text(bar) === 'Open https://good.example/a?b=1 in a new tab?OpenCancel', bar && text(bar));
  check('confirm bar buttons', bar.byText('Open', 'button').length === 1 && bar.byText('Cancel', 'button').length === 1);
  bar.byText('Cancel', 'button')[0].dispatch('click');
  eq('Cancel does not open', b.win.opens.length, 0);
  eq('Cancel closes the bar', m.node.byClass('md-confirm').length, 0);
  ext.dispatch('click');
  m.node.byClass('md-confirm')[0].dispatch('keydown', { key: 'Escape' });
  eq('Escape closes the bar', [m.node.byClass('md-confirm').length, b.win.opens.length], [0, 0]);
  ext.dispatch('click');
  m.node.byClass('md-confirm')[0].byText('Open', 'button')[0].dispatch('click');
  eq('Open calls window.open with the exact arguments', b.win.opens, [['https://good.example/a?b=1', '_blank', 'noopener,noreferrer']]);
  eq('the bar closes after Open', m.node.byClass('md-confirm').length, 0);
  ext.dispatch('auxclick');
  check('middle click is also confirmed', m.node.byClass('md-confirm').length === 1 && b.win.opens.length === 1);
  // repo links
  const repoMd = links[1];
  check('repo link has no href, is focusable', !repoMd.attrs.has('href') && repoMd.getAttribute('tabindex') === '0');
  repoMd.dispatch('click');
  eq('repo md link opens the markdown pane', b.opened.at(-1), { kind: 'markdown', params: { task: REF, commit: 'head', path: 'docs/guide.md' }, opts: { reuse: 'markdown' } });
  links[2].dispatch('keydown', { key: 'Enter' });
  eq('repo file link opens the file pane', b.opened.at(-1), { kind: 'file', params: { task: REF, commit: 'head', path: 'src/main.rs' }, opts: { reuse: 'file' } });
  eq('window.open not used for repo links', b.win.opens.length, 1);
  // images
  const imgs = doc.byTag('img');
  eq('only the loadable image became an img', imgs.length, 1);
  check('image data url', imgs[0].getAttribute('src') === 'data:image/png;base64,' + Buffer.from(PNG).toString('base64') && imgs[0].getAttribute('alt') === 'logo');
  const alts = doc.byClass('md-alt').map(text);
  check('remote image: alt and note, never loaded', alts.some((a) => a === 'remote [remote images are not loaded]') && !b.calls(/blob/).some((u) => /evil/.test(u)), alts.join('|'));
  check('failed image falls back to alt', alts.some((a) => /^broken \[image not loaded: boom\]/.test(a)), alts.join('|'));
  check('non-image (415) refused', alts.some((a) => /^doc \[image not loaded: not an image\]/.test(a)));
  check('oversize refused', alts.some((a) => /^huge \[image not loaded: too large/.test(a)));
  check('404 falls back', alts.some((a) => /^gone \[image not loaded: not found\]/.test(a)));
  check('wrong content type refused', alts.some((a) => /^html \[image not loaded: not an image\]/.test(a)));
  eq('blob requests are sequential, in order', b.calls(/blob/).map((u) => new URL(u, 'http://x').searchParams.get('path')), ['img/logo.png', 'img/broken.png', 'img/doc.txt', 'img/huge.png', 'img/gone.png', 'img/html.png']);
  check('header: sentence, task, view source', text(m.node).includes('committed state of the repository, not the worktree') && m.node.byText('View source', 'button').length === 1);
  m.node.byText('View source', 'button')[0].dispatch('click');
  eq('View source opens the file', b.opened.at(-1).kind, 'file');
  eq('title', m.titles.at(-1), 'Preview README.md');
  check('no object URLs, no storage', b.storage.log.length === 0);
}

// markdown: sequential loading and the image cap
{
  const many = [];
  for (let i = 0; i < 30; i += 1) many.push({ t: 'p', c: [{ t: 'image', alt: 'i' + i, path: 'img/' + i + '.png', src: 'x', note: null }] });
  let inflight = 0;
  let maxInflight = 0;
  const b = boot(
    routesOf({
      '/api/repo/markdown': () => json(mdDoc(many)),
      '/api/repo/blob': async () => {
        inflight += 1;
        maxInflight = Math.max(maxInflight, inflight);
        await new Promise((r) => setImmediate(r));
        inflight -= 1;
        return pngResponse(PNG);
      }
    })
  );
  const m = await b.mount('markdown', { task: REF, commit: 'head', path: 'README.md' });
  await settle(200);
  eq('at most 20 images loaded', m.node.byTag('img').length, 20);
  eq('never two image loads at once', maxInflight, 1);
  check('images past the cap show alt and note', m.node.byClass('md-alt').filter((x) => /at most 20 images/.test(text(x))).length === 10);
  // reloading mid-way stops the old loader
  m.inst.setParams({ task: REF, commit: 'head', path: 'README.md' });
  m.inst.stop();
  await settle(100);
  check('stop halts loading', b.calls(/blob/).length <= 50);
}

// markdown: truncated / notices / bounds
{
  let nest = { t: 'p', c: [{ t: 'text', s: 'core' }] };
  for (let i = 0; i < 500; i += 1) nest = { t: 'quote', c: [nest] };
  const wide = [];
  for (let i = 0; i < 40000; i += 1) wide.push({ t: 'text', s: 'w' });
  const b = boot(routesOf({ '/api/repo/markdown': (q) => json(mdDoc(q.get('path') === 'deep.md' ? [nest] : [{ t: 'p', c: wide }], { truncated: true, notice: 'server truncated it' })) }));
  const deep = await b.mount('markdown', { task: REF, commit: 'head', path: 'deep.md' });
  const a = auditMd(deep.node.byClass('md-doc')[0], 'deep');
  check('deep tree is cut at depth 40', a.problems.length === 0 && a.maxDepth <= 60 && !text(deep.node.byClass('md-doc')[0]).includes('core'), a.maxDepth);
  check('depth notice, truncated notice and server notice', /too large or nested too deeply/.test(text(deep.node)) && /Truncated/.test(text(deep.node)) && /server truncated it/.test(text(deep.node)));
  const wideP = await b.mount('markdown', { task: REF, commit: 'head', path: 'wide.md' });
  const wa = auditMd(wideP.node.byClass('md-doc')[0], 'wide');
  check('node count bounded', wa.nodes <= 20002 && wa.nodes > 1000, wa.nodes);
  check('node cap notice', /too large or nested too deeply/.test(text(wideP.node)));
  const err = boot(routesOf({ '/api/repo/markdown': () => errDoc(413, 'limit', 'document too large') }));
  const me = await err.mount('markdown', { task: REF, commit: 'head', path: 'a.md' });
  check('markdown error shown', /document too large/.test(text(me.node)));
}

// ------------------------------------------------------------ markdown fuzz

{
  let seed = 0x2545f491;
  const rnd = () => {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const pick = (a) => a[Math.floor(rnd() * a.length)];
  const STRINGS = ['', 'x', '<img src=x onerror=alert(1)>', '<script>alert(1)</script>', '\u001b[31m', 'a\u202etxt.exe', 'javascript:alert(1)', 'data:text/html,x', '//evil', 'vbscript:x', '"><svg onload=1>', 'é\u0000', 'README.md', '../../etc/passwd', 'a\\b', 'x'.repeat(5000)];
  const BLOCK_T = ['h', 'p', 'code', 'quote', 'ul', 'ol', 'table', 'hr', 'img', 'script', 'iframe', '', 5, null, 'H', 'link'];
  const INLINE_T = ['text', 'em', 'strong', 'del', 'code', 'br', 'link', 'link', 'link', 'image', 'script', 'a', 'html', 'raw', '', null, 7, 'TEXT'];
  const junk = () => pick([null, undefined, 5, -1, 1.5, NaN, true, false, 'str', STRINGS[Math.floor(rnd() * STRINGS.length)], {}, [], [[]], { t: 'text', s: 5 }, { c: 'x' }]);
  const str = () => (rnd() < 0.15 ? junk() : pick(STRINGS));
  const num = () => pick([1, 2, 3, 5, 6, 7, 0, -1, 1e12, 1.5, NaN, null, 'x']);
  function inl(d) {
    if (rnd() < 0.05) return junk();
    const t = pick(INLINE_T);
    const n = { t };
    if (rnd() < 0.7) n.s = str();
    if (rnd() < 0.5) n.c = kids(inl, d + 1);
    if (t === 'link' || rnd() < 0.1) {
      n.kind = pick(['external', 'external', 'external', 'repo', 'repo', 'weird', null, 5]);
      n.url = pick(HOSTILE_URLS);
      n.path = rnd() < 0.5 ? str() : pick(['a.md', 'docs/b.rs', 'x\u0000y', 'a\\b', '']);
      n.title = str();
    }
    if (t === 'image' || rnd() < 0.1) {
      n.alt = str();
      n.path = rnd() < 0.6 ? pick(['img/a.png', 'img/b.png', 'img/bad.png', 'x/../../y', str()]) : null;
      n.src = str();
      n.note = str();
    }
    if (rnd() < 0.1) n.extra = { onclick: 'alert(1)', style: 'x', href: 'javascript:1', src: 'x', onerror: 'y' };
    if (rnd() < 0.05) n.onerror = 'alert(1)';
    return n;
  }
  function item(d) {
    return rnd() < 0.05 ? junk() : { checked: pick([null, true, false, 'yes', 0, undefined]), c: kids(blk, d + 1) };
  }
  function kids(f, d) {
    if (d > 90 || rnd() < 0.1) return rnd() < 0.5 ? junk() : [];
    const k = Math.floor(rnd() * 5);
    const a = [];
    for (let i = 0; i < k; i += 1) a.push(f(d));
    if (rnd() < 0.01) for (let i = 0; i < 30000; i += 1) a.push({ t: 'text', s: 'z' });
    return a;
  }
  function blk(d) {
    if (rnd() < 0.05) return junk();
    const t = pick(BLOCK_T);
    const n = { t };
    n.level = num();
    n.start = num();
    n.lang = rnd() < 0.5 ? str() : null;
    n.text = str();
    n.c = t === 'ul' || t === 'ol' ? kids(item, d + 1) : t === 'quote' ? kids(blk, d + 1) : kids(inl, d + 1);
    n.align = rnd() < 0.7 ? [pick(['left', 'center', 'right', null, 'x', 5]), pick(['left', 'center', 'right', null, 'x'])] : junk();
    n.head = rnd() < 0.8 ? [kids(inl, d + 1), kids(inl, d + 1)] : junk();
    n.rows = rnd() < 0.8 ? [[kids(inl, d + 1), kids(inl, d + 1)], junk(), [junk()]] : junk();
    if (rnd() < 0.05) n.html = '<b>x</b>';
    return n;
  }
  function deepChain(kind, depth) {
    let n = kind === 'quote' ? { t: 'p', c: [{ t: 'text', s: 'leaf' }] } : { t: 'text', s: 'leaf' };
    for (let i = 0; i < depth; i += 1) {
      if (kind === 'quote') n = { t: 'quote', c: [n] };
      else if (kind === 'list') n = { t: 'ul', c: [{ checked: null, c: [n] }] };
      else if (kind === 'em') n = { t: 'em', c: [n] };
      else n = { t: 'link', kind: 'external', url: 'https://x.example/', c: [n] };
    }
    return kind === 'quote' || kind === 'list' ? n : { t: 'p', c: [n] };
  }

  const b = boot(
    routesOf({
      '/api/repo/markdown': (q) => json(JSON.parse(q.get('x') ? '{}' : '{}')),
      '/api/repo/blob': (q) => {
        const p = q.get('path');
        if (p === 'img/bad.png') return pngResponse(PNG, 'image/svg+xml');
        return pngResponse(PNG);
      }
    })
  );
  const trees = [];
  for (let i = 0; i < 250; i += 1) {
    const blocks = [];
    const k = 1 + Math.floor(rnd() * 6);
    for (let j = 0; j < k; j += 1) blocks.push(blk(0));
    trees.push(blocks);
  }
  for (const kind of ['quote', 'list', 'em', 'link']) for (const depth of [30, 39, 40, 41, 80, 400, 5000]) trees.push([deepChain(kind, depth)]);
  trees.push('not an array', null, 7, { t: 'p' }, [[], [[]], [null], [5], ['x']]);
  const huge = [];
  for (let i = 0; i < 100000; i += 1) huge.push({ t: 'hr' });
  trees.push(huge);
  const bigRows = { t: 'table', align: [], head: [[]], rows: [] };
  for (let i = 0; i < 50000; i += 1) bigRows.rows.push([[{ t: 'text', s: 'c' }], [{ t: 'text', s: 'd' }]]);
  trees.push([bigRows]);
  const imgsMany = [];
  for (let i = 0; i < 100; i += 1) imgsMany.push({ t: 'p', c: [{ t: 'image', alt: 'a', path: 'img/a.png', src: 's', note: null }] });
  trees.push(imgsMany);

  let threw = 0;
  let worst = { nodes: 0, depth: 0 };
  let problems = [];
  let totalImgs = 0;
  let anchorsSeen = 0;
  let imgsSeen = 0;
  let elementsSeen = 0;
  let idx = 0;
  for (const blocks of trees) {
    const routes = routesOf({
      '/api/repo/markdown': () => json(mdDoc(blocks, { truncated: rnd() < 0.2, notice: rnd() < 0.2 ? str() : null })),
      '/api/repo/blob': (q) => (q.get('path') === 'img/bad.png' ? pngResponse(PNG, 'image/svg+xml') : pngResponse(PNG))
    });
    const f = boot(routes);
    let m;
    try {
      m = await f.mount('markdown', { task: REF, commit: 'head', path: 'README.md' });
      await settle(Array.isArray(blocks) && blocks.length > 50 ? 60 : 12);
    } catch (e) {
      threw += 1;
      problems.push('threw: ' + (e && e.stack));
      continue;
    }
    const doc = m.node.byClass('md-doc')[0];
    const a = auditMd(doc, 'fuzz#' + idx);
    problems = problems.concat(a.problems.slice(0, 3));
    worst = { nodes: Math.max(worst.nodes, a.nodes), depth: Math.max(worst.depth, a.maxDepth) };
    totalImgs = Math.max(totalImgs, a.imgs);
    anchorsSeen += doc.byTag('a').length;
    imgsSeen += a.imgs;
    elementsSeen += a.nodes;
    // click every link: must never throw, never navigate, window.open only after Open
    for (const link of doc.byTag('a').slice(0, 10)) {
      try {
        link.dispatch('mouseover');
        link.dispatch('click');
      } catch (e) {
        threw += 1;
        problems.push('click threw: ' + e.message);
      }
    }
    if (f.win.opens.length !== 0) problems.push('fuzz#' + idx + ' window.open without confirmation');
    for (const bar of m.node.byClass('md-confirm')) {
      const shown = text(bar.byClass('md-confirm-url')[0]);
      if (!HTTP_URL.test(shown) && !/\[U\+/.test(shown)) problems.push('confirm url not http(s): ' + shown.slice(0, 50));
    }
    m.inst.stop();
    idx += 1;
  }
  eq('fuzz: nothing threw', threw, 0);
  eq('fuzz: no allowlist violation', problems.slice(0, 10), []);
  check('fuzz: node bound respected', worst.nodes <= 20002, worst.nodes);
  check('fuzz: depth bound respected', worst.depth <= 70, worst.depth);
  check('fuzz: image cap respected', totalImgs <= 20, totalImgs);
  check('fuzz: ran many trees', idx >= 250, idx);
  check('fuzz: exercised anchors, images and many elements', anchorsSeen > 20 && imgsSeen > 5 && elementsSeen > 50000, anchorsSeen + '/' + imgsSeen + '/' + elementsSeen);
}

// ---------------------------------------------------------- hostile hrefs

{
  const hrefs = HOSTILE_URLS.map((u) => ({ t: 'link', kind: 'external', url: u, title: null, c: [{ t: 'text', s: 'l' }] }));
  const b = boot(routesOf({ '/api/repo/markdown': () => json(mdDoc([{ t: 'p', c: hrefs }])) }));
  const m = await b.mount('markdown', { task: REF, commit: 'head', path: 'README.md' });
  const doc = m.node.byClass('md-doc')[0];
  const as = doc.byTag('a').map((a) => a.getAttribute('href'));
  eq('only the two good urls became anchors', as, ['http://good.example/path?q=1#h', 'https://good.example/']);
  eq('every other link is a dead span', doc.byClass('md-dead').length, HOSTILE_URLS.length - 2);
  eq('and the audit is clean', auditMd(doc, 'hrefs').problems, []);
}

// -------------------------------------------------------------------- image

{
  const b = boot(
    routesOf({
      '/api/repo/blob': (q) => {
        const p = q.get('path');
        if (p === 'a.png') return pngResponse(PNG);
        if (p === 'jpg.jpg') return pngResponse(PNG, 'image/jpeg; charset=binary');
        if (p === 'txt') return { status: 415, body: { schema: 'fleet.error/1', error: { code: 'unsupported', message: 'nope' } } };
        if (p === 'big.png') return { status: 413, body: { schema: 'fleet.error/1', error: { code: 'limit', message: 'big' } } };
        if (p === 'svg.png') return pngResponse(PNG, 'image/svg+xml');
        if (p === 'big2.png') return pngResponse(new Uint8Array(4 * 1024 * 1024 + 1));
        if (p === 'empty.png') return pngResponse(new Uint8Array(0));
        if (p === 'lots.png') return pngResponse(new Uint8Array(3 * 1024 * 1024).map((_, i) => i & 255));
        if (p === 'odd<name>.png') return pngResponse(PNG, 'image/webp');
        return errDoc(404, 'not_found', 'missing');
      }
    }),
    { host: verif('pending', null) }
  );
  const open = (p) => b.mount('image', { task: REF, commit: 'head', path: p });
  let m = await open('a.png');
  const img = m.node.byTag('img')[0];
  check('image shown from a data URL', img && img.getAttribute('src') === 'data:image/png;base64,' + Buffer.from(PNG).toString('base64'));
  check('image alt is the escaped file name', img.getAttribute('alt') === 'a.png');
  check('type and size shown', /image\/png, 13 bytes/.test(text(m.node)), text(m.node));
  check('header: pending badge, sentence, task', m.node.byClass('vf-pending').length === 1 && text(m.node).includes('committed state of the repository, not the worktree') && text(m.node.byClass('pane-task')[0]) === REF);
  img.naturalWidth = 640;
  img.naturalHeight = 480;
  img.dispatch('load');
  check('dimensions after load', /640 \u00d7 480 px/.test(text(m.node)));
  const stage = m.node.byClass('img-stage')[0];
  const fit = m.node.byText('Fit to width', 'button')[0];
  fit.dispatch('click');
  check('fit-to-width toggle', stage.hasClass('fit') && fit.getAttribute('aria-pressed') === 'true');
  fit.dispatch('click');
  check('fit-to-width toggles off', !stage.hasClass('fit'));
  m = await open('jpg.jpg');
  check('content type parameters are tolerated', m.node.byTag('img').length === 1 && /^data:image\/jpeg;base64,/.test(m.node.byTag('img')[0].getAttribute('src')));
  m = await open('lots.png');
  const src = m.node.byTag('img')[0].getAttribute('src');
  const expect = Buffer.from(new Uint8Array(3 * 1024 * 1024).map((_, i) => i & 255)).toString('base64');
  check('large images are encoded correctly in chunks', src === 'data:image/png;base64,' + expect);
  for (const [p, re] of [['txt', /not an image/], ['big.png', /too large/], ['svg.png', /not an image/], ['big2.png', /too large/], ['empty.png', /no image data/], ['nothing.png', /not found/]]) {
    m = await open(p);
    check('image error ' + p, m.node.byTag('img').length === 0 && re.test(text(m.node)), text(m.node));
  }
  m = await open('odd<name>.png');
  check('hostile file name only as alt text', m.node.byTag('img')[0].getAttribute('alt') === 'odd<name>.png' && b.dom.created.every((t) => t !== 'name'));
  m = await b.mount('image', { task: REF, commit: SHA, path: 'a.png' });
  check('sha commit is sent', /commit=a1b2c3d4e5f60718293a4b5c6d7e8f9012345678/.test(b.calls(/blob/).at(-1)));
  check('custom commit label', /commit a1b2c3d4e5/.test(text(m.node.byClass('pane-commits')[0])));
  eq('title', m.titles.at(-1), 'a.png');
  // blob routine never uses a plain fetch of the url as img src
  check('no blob: or http(s) src was ever set', b.dom.attrWrites.filter((w) => w.name === 'src').every((w) => DATA_URL.test(w.value)));
}

// verification states
{
  for (const [state, dirty, label, cls] of [['none', null, 'unverified', 'vf-none'], ['unverified', false, 'unverified', 'vf-unverified'], ['failed', true, 'failed', 'vf-failed'], ['pending', null, 'pending', 'vf-pending'], ['verified', false, 'verified', 'vf-verified'], ['bogus', null, 'verification unknown', 'vf-unknown']]) {
    const b = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', [])) }), { host: verif(state, dirty) });
    const m = await b.mount('explorer', { task: REF, commit: 'head', path: '' });
    const badge = m.node.byClass('pane-badges')[0].byClass('badge')[0];
    check('verification ' + state, badge && badge.hasClass(cls) && text(badge) === label, badge && badge.className + ' ' + text(badge));
    eq('dirty badge for ' + state, m.node.byClass('pane-dirty').length, dirty === true ? 1 : 0);
  }
  const rej = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', [])) }), { host: { taskVerification: async () => { throw new Error('x'); } } });
  const mr = await rej.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('verification failure is shown as unknown', mr.node.byClass('vf-unknown').length === 1);
  const thr = boot(routesOf({ '/api/repo/tree': () => json(treeDoc('', [])) }), { host: { taskVerification: () => { throw new Error('x'); } } });
  const mt = await thr.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('synchronous verification failure is survived', mt.node.byClass('tree').length === 1);
  const net = boot(() => { throw new Error('network down'); });
  const mn = await net.mount('explorer', { task: REF, commit: 'head', path: '' });
  check('network failure is an error notice', mn.node.byClass('pane-notice').some((n) => n.hasClass('bad')));
  const un = boot(() => errDoc(401, 'unauthorized', 'login'));
  await un.mount('file', { task: REF, commit: 'head', path: 'a' });
  eq('401 calls host.unauthorized', un.host.unauthorizedCalls, 1);
}

// ---------------------------------------------------------- global audit

{
  const TAGS = new Set(['div', 'span', 'p', 'pre', 'code', 'button', 'a', 'img', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'ul', 'ol', 'li', 'table', 'thead', 'tbody', 'tr', 'th', 'td', 'hr', 'em', 'strong', 'del', 'br', 'input', 'blockquote', 'nav']);
  const ATTRS = new Set(['class', 'type', 'title', 'role', 'tabindex', 'disabled', 'href', 'rel', 'alt', 'src', 'start', 'checked']);
  let tags = 0;
  let attrs = 0;
  for (const dom of allDoms) {
    for (const t of dom.created) {
      tags += 1;
      if (!TAGS.has(t)) failures.push('created element outside the allowlist: ' + t);
    }
    for (const w of dom.attrWrites) {
      attrs += 1;
      if (!(ATTRS.has(w.name) || w.name.startsWith('aria-'))) failures.push('attribute outside the allowlist: ' + w.name);
      if (w.name === 'href' && !(HTTP_URL.test(w.value) && w.el.tag === 'a')) failures.push('bad href ' + w.value.slice(0, 50));
      if (w.name === 'src' && !(DATA_URL.test(w.value) && w.el.tag === 'img')) failures.push('bad src ' + w.value.slice(0, 50));
      if (w.name === 'type' && !((w.el.tag === 'button' && w.value === 'button') || (w.el.tag === 'input' && w.value === 'checkbox'))) failures.push('bad type ' + w.el.tag + ' ' + w.value);
      if (w.name === 'rel' && w.value !== 'noopener noreferrer') failures.push('bad rel ' + w.value);
    }
    for (const v of dom.violations) failures.push('DOM violation: ' + v);
  }
  check('audited many elements', tags > 20000, tags);
  check('audited many attribute writes', attrs > 20000, attrs);
  check('no a element without rel', allDoms.every((d) => d.attrWrites.filter((w) => w.name === 'href').every((w) => w.el.attrs.get('rel') === 'noopener noreferrer')));
  check('no target attribute anywhere', allDoms.every((d) => d.attrWrites.every((w) => w.name !== 'target')));
}

if (failures.length) {
  console.error(failures.length + ' FAILED of ' + checks + ' checks');
  for (const f of failures.slice(0, 60)) console.error(' - ' + f);
  process.exit(1);
}
console.log('web-smoke-panes: ' + checks + ' checks passed');
