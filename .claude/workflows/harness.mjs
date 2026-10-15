// Runs a workflow script with every agent stubbed, for the workflows' tests. Nothing here or in CI
// runs a workflow.
import { readFileSync } from 'node:fs'

const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor

// The runtime refuses what would break a resume.
const Clockless = new Proxy(Date, {
  construct(target, a) {
    if (!a.length) throw new Error('new Date() is unavailable in a workflow')
    return new target(...a)
  },
  get(target, key) {
    if (key === 'now') return () => { throw new Error('Date.now() is unavailable in a workflow') }
    return Reflect.get(target, key)
  },
})
const Randomless = Object.create(Math, {
  random: { value: () => { throw new Error('Math.random() is unavailable in a workflow') } },
})

export function load(file) {
  const src = readFileSync(file, 'utf8').replace('export const meta', 'const meta')
  const body = new AsyncFunction('args', 'agent', 'parallel', 'pipeline', 'phase', 'log', 'Date', 'Math', src)
  return { src, body }
}

// `behave(label, prompt, opts)` answers each agent call; a thunk that throws resolves to null, as in
// the runtime's `parallel`.
export async function run(body, args, behave) {
  const calls = [], logs = []
  const agent = async (prompt, opts) => { calls.push({ label: opts.label, opts, prompt }); return behave(opts.label, prompt, opts) }
  const parallel = async thunks => Promise.all(thunks.map(t => t().catch(() => null)))
  let result, error
  try { result = await body(args, agent, parallel, null, () => {}, m => logs.push(m), Clockless, Randomless) } catch (e) { error = e.message }
  return { calls, logs, result, error }
}

export function checker() {
  let failed = 0
  return {
    check(name, ok, detail = '') {
      console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ': ' + detail : ''}`)
      if (!ok) failed++
    },
    done() { process.exitCode = failed ? 1 : 0 },
  }
}
