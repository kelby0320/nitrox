// The bug bash's control flow, with every agent stubbed: `node bug-bash-workflow.test.mjs`, or with a
// copy of the workflow's path to test that instead.
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { load, run as runBody, checker } from './harness.mjs'
const file = process.argv[2] || fileURLToPath(new URL('./bug-bash-workflow.js', import.meta.url))
const { src, body } = load(file)
const run = (args, behave) => runBody(body, args, behave)
const { check, done } = checker()

const WT = '/r/.claude/worktrees/wf_run1-'
const KERNEL = { subsystem: 'kernel-objects', id: 'kobj', what: 'w', paths: ['kernel/src/syscall/', 'userspace/libkern/'] }
const USER = { subsystem: 'filesystems', id: 'fs', what: 'w', paths: ['userspace/fs-server-fat/'] }
const scope = (o = {}) => ({ ok: true, stop_reason: '', worktree: WT + '1', head: 'hhhh1234', entry_points: ['e'], trust_boundaries: ['t'], deferrals: [], tests: [], claude_md: [], summary: 's', ...o })
const f = (file, line, o = {}) => ({ file, line, defect: 'd', scenario: 's', evidence: 'e', demonstrated: true, needs_boot: false, severity: 'medium', ...o })
const hunt = findings => ({ held_up: ['held'], findings })
const rp = r => r.calls.find(c => c.label === 'report')?.prompt || ''
const reportFindings = r => { const p = rp(r); const i = p.indexOf('severity:\n'); return i < 0 ? [] : JSON.parse(p.slice(i + 10, p.indexOf('\n\nYour final message'))) }

for (const bad of [undefined, {}, { ...USER, paths: [] }, { ...USER, id: 'F S' }, { ...USER, subsystem: '../x' }]) {
  const r = await run(bad, () => null)
  check(`bad args refused: ${JSON.stringify(bad)}`, !!r.error && r.calls.length === 0, r.error)
}

let r = await run(USER, () => scope({ ok: false, stop_reason: 'no such path' }))
check('a scope that stops ends the run', r.calls.length === 1 && r.result?.stopped === 'no such path')

r = await run(KERNEL, (l) => {
  if (l === 'scope') return scope()
  if (l === 'hunt:trust-boundary') return hunt([f('a.rs', 1, { severity: 'high' }), f('b.rs', 2, { needs_boot: true, demonstrated: false }), f('c.rs', 3, { needs_boot: true })])
  if (l === 'hunt:logic') return hunt([f('d.rs', 4, { demonstrated: false })])
  if (l.startsWith('hunt:')) return hunt([])
  if (l === 'verify:boot') return { verdicts: [{ id: 'trust-boundary-2', verdict: 'demonstrated', severity: 'high', evidence: 'booted', duplicate_of: 'logic-1' }, { id: 'trust-boundary-3', verdict: 'refuted', severity: 'low', evidence: 'no' }, { id: 'logic-1', verdict: 'refuted', severity: 'low', evidence: 'x' }] }
  return 'SECTION'
})
check('a kernel subsystem with boot findings: 8 agents, the architecture lens among them', r.calls.length === 8 && r.calls.some(c => c.label === 'hunt:architecture'), r.calls.map(c => c.label).join(','))
const rf = reportFindings(r)
check('a booted finding is demonstrated and keeps its duplicate', rf.find(x => x.file === 'b.rs')?.demonstrated === true && rf.find(x => x.file === 'b.rs')?.duplicate_of === 'logic-1')
check('a refuted boot finding is dropped', !rf.some(x => x.file === 'c.rs') && r.result?.counts.refuted === 1)
check('a verdict for a finding not given to the verifier is ignored', rf.find(x => x.file === 'd.rs')?.verdict === 'unverified' && r.logs.some(m => /logic-1, which it was not given/.test(m)))
check('the section is returned', r.result?.section === 'SECTION' && r.result?.head === 'hhhh1234')
check('the report numbers by the subsystem id and removes the run\'s worktrees', /id, for numbering: kobj\./.test(rp(r)) && rp(r).includes(`those whose path starts ${WT}.`))
check('every hunter checks out the scope\'s commit', r.calls.filter(c => c.label.startsWith('hunt:')).every(c => c.prompt.includes('git checkout --detach hhhh1234')))

r = await run(USER, l => l === 'scope' ? scope() : l.startsWith('hunt:') ? hunt([]) : 'SECTION')
check('a userspace subsystem has no architecture lens and no boot', !r.calls.some(c => c.label === 'hunt:architecture' || c.label === 'verify:boot') && r.calls.length === 6)

r = await run(USER, l => l === 'scope' ? scope() : l === 'hunt:error-paths' ? null : l.startsWith('hunt:') ? hunt([]) : 'SECTION')
check('a dead lens is named to the report and returned', /Lenses that returned nothing: error-paths/.test(rp(r)) && r.result?.lenses_missing.join() === 'error-paths')
r = await run(USER, l => l === 'scope' ? scope() : l.startsWith('hunt:') ? null : 'NO HUNT')
check('every lens dead: an error, worktrees still removed', !!r.error && /NO HUNT/.test(rp(r)) && rp(r).includes(WT), r.error)
r = await run(USER, l => l === 'scope' ? scope({ worktree: '/home/kelby/src/nitrox' }) : l.startsWith('hunt:') ? hunt([]) : 'SECTION')
check('a scope worktree outside .claude/worktrees removes nothing', /Remove no worktree/.test(rp(r)) && !rp(r).includes('/home/kelby/src/nitrox'))

r = await run(USER, l => l === 'scope' ? scope() : l === 'hunt:logic' ? hunt([f('a.rs', 1)]) : l.startsWith('hunt:') ? hunt([]) : null)
check('a dead report returns the findings unwritten', !r.error && r.result?.section === null && r.result?.findings.length === 1 && /no report/.test(r.result?.error), r.error)

const want = { scope: 'sonnet', 'hunt:logic': undefined, 'hunt:error-paths': undefined, 'hunt:trust-boundary': undefined, 'hunt:architecture': undefined, 'hunt:tests': 'sonnet', 'verify:boot': 'sonnet', report: 'sonnet' }
r = await run(KERNEL, l => l === 'scope' ? scope() : l.startsWith('hunt:') ? hunt([f('a.rs', 1, { needs_boot: true })]) : l === 'verify:boot' ? { verdicts: [] } : 'SECTION')
check('each stage on its model', r.calls.every(c => want[c.label] === c.opts.model), r.calls.map(c => `${c.label}=${c.opts.model}`).join(','))
check('every agent but the report in its own worktree', r.calls.every(c => (c.label === 'report') === (c.opts.isolation !== 'worktree')))
check('MAX_AGENTS is used', (src.match(/MAX_AGENTS/g) || []).length >= 2)
const named = new Set(src.match(/\$\{(?:DIR|SHARED)\}\/[\w./-]+\.md/g) || [])
check('the rules, scope, verify and report files are named', ['rules', 'scope', 'verify', 'report'].every(n => named.has(`\${DIR}/${n}.md`)))
for (const named_ of named) {
  const path = named_.replace('${DIR}', '.claude/skills/bug-bash').replace('${SHARED}', '.claude/skills/pr-review/lenses')
  let exists = true
  try { readFileSync(fileURLToPath(new URL(`../../${path}`, import.meta.url))) } catch { exists = false }
  check(`${path} exists`, exists)
}
done()
