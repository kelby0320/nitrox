// The review workflow's control flow, with every agent stubbed: `node pr-review-workflow.test.mjs`,
// or with a copy of the workflow's path to test that instead. Nothing here or in CI runs a workflow.
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
const file = process.argv[2] || fileURLToPath(new URL('./pr-review-workflow.js', import.meta.url))
const src = readFileSync(file, 'utf8').replace('export const meta', 'const meta')
const body = new (Object.getPrototypeOf(async function () {}).constructor)('args', 'agent', 'parallel', 'pipeline', 'phase', 'log', src)
async function run(args, behave) {
  const calls = [], logs = []
  const agent = async (prompt, opts) => { calls.push({ label: opts.label, opts, prompt }); return behave(opts.label, prompt, opts) }
  const parallel = async thunks => Promise.all(thunks.map(t => t().catch(() => null)))
  let result, error
  try { result = await body(args, agent, parallel, null, () => {}, m => logs.push(m)) } catch (e) { error = e.message }
  return { calls, logs, result, error }
}
const WT = '/r/.claude/worktrees/wf_run1-'
const scope = (o = {}) => ({ ok: true, stop_reason: '', worktree: WT + '1', head: 'hhhh1234', base: 'b', files: ['a.rs'], claude_md: [], tests: [], touches_kernel_or_abi: false, summary: 's', ...o })
const f = (file, line, o = {}) => ({ file, line, defect: 'd', scenario: 's', evidence: 'e', demonstrated: true, needs_boot: false, severity: 'worth-fixing', ...o })
const rev = findings => ({ held_up: [], findings })
const rp = r => r.calls.find(c => c.label === 'report')?.prompt || ''
const reportFindings = r => { const p = rp(r); const i = p.indexOf('severity:\n'); return i < 0 ? [] : JSON.parse(p.slice(i + 10, p.indexOf('\n\nYour final message'))) }
let failed = 0
const check = (name, ok, detail = '') => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ': ' + detail : ''}`); if (!ok) failed++ }

let r = await run({ pr: 5 }, () => ({ ...scope(), ok: false, stop_reason: 'CI red' }))
check('CI red stops at the scope', r.calls.length === 1 && r.result?.stopped === 'CI red')

r = await run({ pr: 5 }, (l, p) => {
  if (l === 'scope') return scope({ touches_kernel_or_abi: true })
  if (l === 'review:tests') return rev([f('a.rs', 1), f('b.rs', 2, { needs_boot: true, demonstrated: false }), f('c.rs', 3, { needs_boot: true })])
  if (l === 'review:logic') return rev([f('d.rs', 4, { demonstrated: false })])
  if (l.startsWith('review:')) return rev([])
  if (l === 'verify:boot') return { verdicts: [{ id: 'tests-2', verdict: 'demonstrated', severity: 'blocking', evidence: 'booted', duplicate_of: 'logic-1' }, { id: 'tests-3', verdict: 'refuted', severity: 'optional', evidence: 'no' }] }
  return 'BODY'
})
check('a kernel PR with boot findings: 8 agents', r.calls.length === 8, r.calls.map(c => c.label).join(','))
const rf = reportFindings(r)
check('#7 a booted finding is demonstrated and keeps duplicate_of', rf.find(x => x.file === 'b.rs')?.demonstrated === true && rf.find(x => x.file === 'b.rs')?.duplicate_of === 'logic-1')
check('a refuted boot finding is dropped', !rf.some(x => x.file === 'c.rs') && r.result?.counts.refuted === 1)
check('an undemonstrated finding is reported unverified', rf.find(x => x.file === 'd.rs')?.verdict === 'unverified')
check('the report returns its body', r.result?.posted === 'BODY')

r = await run({ pr: 1 }, l => l === 'scope' ? scope() : l === 'review:error-paths' ? null : l.startsWith('review:') ? rev([]) : 'BODY')
check('#1 a dead lens is named to the report', /Lenses that returned nothing: error-paths/.test(rp(r)) && r.result?.lenses_missing.join() === 'error-paths')
r = await run({ pr: 1 }, l => l === 'scope' ? scope() : l.startsWith('review:') ? null : 'NO REVIEW')
check('#1 every lens dead: nothing posted, an error, worktrees still removed', !!r.error && /Post nothing/.test(rp(r)) && rp(r).includes(WT), r.error)

r = await run({ pr: 1 }, l => l === 'scope' ? scope() : l === 'review:logic' ? rev([f('a.rs', 1, { severity: 'blocking' }), f('b.rs', 2, { needs_boot: true })]) : l.startsWith('review:') ? rev([]) : l === 'verify:boot' ? { verdicts: [{ id: 'logic-1', verdict: 'refuted', severity: 'optional', evidence: 'x' }] } : 'BODY')
check('#2 a verdict for a finding not given to the verifier is ignored', reportFindings(r).some(x => x.file === 'a.rs') && r.result?.counts.refuted === 0 && r.logs.some(m => /logic-1, which it was not given/.test(m)))

r = await run({ pr: 1 }, l => l === 'scope' ? scope() : l === 'review:tests' ? null : l.startsWith('review:') ? rev([]) : 'BODY')
check('#4/#9 worktrees removed by the run prefix from the scope, not by agents\' paths', rp(r).includes(`those whose path starts ${WT}.`))
r = await run({ pr: 1 }, l => l === 'scope' ? scope({ worktree: '/home/kelby/src/nitrox-review' }) : l.startsWith('review:') ? rev([]) : 'BODY')
check('#9 a scope worktree outside .claude/worktrees removes nothing', /Remove no worktree/.test(rp(r)) && !rp(r).includes('nitrox-review'))
r = await run({ pr: 1 }, l => l === 'scope' ? scope() : l.startsWith('review:') ? rev([]) : null)
check('#4 a dead report is an error', !!r.error && /report agent returned nothing/.test(r.error), r.error)

r = await run({ pr: 1, head: 'aaaa' }, l => l === 'scope' ? scope({ head: 'cccc' }) : l.startsWith('review:') ? rev([]) : 'BODY')
check('#5 a scope naming another head than the pinned one is an error', !!r.error && r.calls.length === 1, r.error)
r = await run({ pr: 1, head: 'hhhh' }, l => l === 'scope' ? scope() : l.startsWith('review:') ? rev([]) : 'BODY')
check('#5 a pinned short sha matches its full head', !r.error)

r = await run({ pr: 1 }, l => l === 'scope' ? scope({ files: ['kernel/src/syscall/table.rs'], touches_kernel_or_abi: false }) : l.startsWith('review:') ? rev([]) : 'BODY')
check('#6 kernel paths bring in the architecture lens without the boolean', r.calls.some(c => c.label === 'review:architecture'))
r = await run({ pr: 1 }, l => l === 'scope' ? scope({ files: ['userspace/nxsh/src/eval.rs'] }) : l.startsWith('review:') ? rev([]) : 'BODY')
check('#6 other paths do not', !r.calls.some(c => c.label === 'review:architecture'))

const src2 = readFileSync(file, 'utf8')
check('#8 MAX_AGENTS is used', (src2.match(/MAX_AGENTS/g) || []).length >= 2)

const want = { scope: 'sonnet', 'review:logic': undefined, 'review:error-paths': undefined, 'review:architecture': undefined, 'review:tests': 'sonnet', 'review:rules-and-docs': 'sonnet', 'verify:boot': 'sonnet', report: 'sonnet' }
r = await run({ pr: 5 }, l => l === 'scope' ? scope({ touches_kernel_or_abi: true }) : l.startsWith('review:') ? rev([f('a.rs', 1, { needs_boot: true })]) : l === 'verify:boot' ? { verdicts: [] } : 'BODY')
check('each stage on its model', r.calls.every(c => want[c.label] === c.opts.model))
r = await run({ pr: 'x' }, () => null)
check('bad args refused', !!r.error && r.calls.length === 0, r.error)
process.exitCode = failed ? 1 : 0
