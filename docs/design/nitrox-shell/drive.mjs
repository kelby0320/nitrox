// Drive the Nitrox shell design in headless Chrome, and read it back as numbers.
//
//   node docs/design/nitrox-shell/drive.mjs page   <out.png>
//   node docs/design/nitrox-shell/drive.mjs window <title> <out-base>   # out-base.png + .json
//   node docs/design/nitrox-shell/drive.mjs steps  <steps.mjs> [args…]
//
// Needs `google-chrome` and Node 22+ (for the built-in WebSocket) — nothing installed from npm.
// Chrome runs on a throwaway profile, so a browser you have open is not touched.
//
// `window` finds the application window whose text starts with <title> ("Files", "nxterm",
// "theme.toml"), writes a PNG of it as it is stacked at load — raise one first with `steps` if
// another covers it — and a JSON list of every element inside it: its rectangle relative to the
// window, colours, font, padding, radius and borders, as the page computed them. That is what
// makes a comparison a list of numbers rather than a squint.
//
// `steps` imports a module whose default export receives { eval, click, move, key, shot, sleep }
// and drives the page: `click` sends real mouse events, `eval` runs JavaScript in the page and
// returns JSON. See README.md for why this exists.
import { spawn } from 'node:child_process';
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const PAGE = pathToFileURL(join(dirname(fileURLToPath(import.meta.url)), 'nitrox-shell.html')).href;
// The page's root is a fixed 1440x900; a smaller viewport crops rather than reflows.
const W = 1440;
const H = 900;
const PORT = 9333 + Math.floor(Math.random() * 500);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const [command, ...args] = process.argv.slice(2);
if (!['page', 'window', 'steps'].includes(command)) {
  console.error('usage: drive.mjs page <out.png> | window <title> <out-base> | steps <steps.mjs> [args…]');
  process.exit(2);
}

const profile = mkdtempSync(join(tmpdir(), 'nitrox-design-'));
const chrome = spawn('google-chrome', [
  '--headless=new', '--no-sandbox', '--disable-gpu', '--hide-scrollbars',
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`, `--window-size=${W},${H}`,
  'about:blank',
], { stdio: 'ignore' });

async function pageTarget() {
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const page = list.find((t) => t.type === 'page');
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
    await sleep(100);
  }
  throw new Error('Chrome never offered a page to drive');
}

const ws = new WebSocket(await pageTarget());
await new Promise((r) => ws.addEventListener('open', r, { once: true }));
let nextId = 1;
const pending = new Map();
ws.addEventListener('message', (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    const { ok, err } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? err(new Error(JSON.stringify(msg.error))) : ok(msg.result);
  }
});
const send = (method, params = {}) =>
  new Promise((ok, err) => {
    const id = nextId++;
    pending.set(id, { ok, err });
    ws.send(JSON.stringify({ id, method, params }));
  });

await send('Page.enable');
await send('Runtime.enable');
await send('Emulation.setDeviceMetricsOverride', { width: W, height: H, deviceScaleFactor: 1, mobile: false });
await send('Page.navigate', { url: PAGE });
// The page renders from its own script after load; this is long enough for every state it draws.
await sleep(2500);

const api = {
  sleep,
  async eval(expr) {
    const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
    return r.result.value;
  },
  async move(x, y) {
    await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
    await sleep(120);
  },
  async click(x, y) {
    await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
    await sleep(300);
  },
  async key(key) {
    await send('Input.dispatchKeyEvent', { type: 'keyDown', key });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key });
    await sleep(150);
  },
  async shot(file, clip) {
    const params = { format: 'png' };
    if (clip) params.clip = { x: clip[0], y: clip[1], width: clip[2], height: clip[3], scale: 1 };
    const r = await send('Page.captureScreenshot', params);
    writeFileSync(file, Buffer.from(r.data, 'base64'));
  },
};

// Every element inside the window whose text starts with `title`, measured as the page drew it.
const DESCRIBE = `(title) => {
  const root = [...document.querySelectorAll('div')].find(el => {
    const cs = getComputedStyle(el); const b = el.getBoundingClientRect();
    return cs.position === 'absolute' && cs.zIndex !== 'auto' && b.width > 250 && b.height > 150
      && (el.innerText || '').startsWith(title);
  });
  if (!root) return null;
  const R = root.getBoundingClientRect();
  const els = [];
  const walk = (el, depth) => {
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden') return;
    const b = el.getBoundingClientRect();
    const text = [...el.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent.trim()).join(' ').trim();
    const side = s => cs['border' + s + 'Width'] + ' ' + cs['border' + s + 'Style'] + ' ' + cs['border' + s + 'Color'];
    els.push({ depth, tag: el.tagName.toLowerCase(), x: Math.round(b.x - R.x), y: Math.round(b.y - R.y),
      w: Math.round(b.width), h: Math.round(b.height), text: text.slice(0, 80), background: cs.backgroundColor,
      color: cs.color, font: cs.fontWeight + ' ' + cs.fontSize + ' ' + cs.fontFamily.split(',')[0],
      lineHeight: cs.lineHeight, padding: cs.padding, radius: cs.borderRadius, gap: cs.gap,
      border: ['Top', 'Right', 'Bottom', 'Left'].map(side) });
    for (const c of el.children) walk(c, depth + 1);
  };
  walk(root, 0);
  return { rect: { x: R.x, y: R.y, w: R.width, h: R.height }, els };
}`;

try {
  if (command === 'page') {
    await api.shot(resolve(args[0]));
  } else if (command === 'window') {
    const found = await api.eval(`(${DESCRIBE})(${JSON.stringify(args[0])})`);
    if (!found) throw new Error(`no window whose text starts with ${JSON.stringify(args[0])}`);
    const r = found.rect;
    await api.shot(resolve(args[1] + '.png'), [r.x - 1, r.y - 1, r.w + 2, r.h + 2]);
    writeFileSync(resolve(args[1] + '.json'), JSON.stringify(found, null, 1));
    console.log(`${args[0]}: ${r.w}x${r.h} at ${r.x},${r.y}, ${found.els.length} elements`);
  } else {
    const steps = await import(pathToFileURL(resolve(args[0])).href);
    await steps.default(api, args.slice(1));
  }
} catch (e) {
  console.error(e.message ?? e);
  process.exitCode = 1;
} finally {
  ws.close();
  chrome.kill('SIGKILL');
  await sleep(200);
  rmSync(profile, { recursive: true, force: true });
}
