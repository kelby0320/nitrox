// The safety survey's control flow, with every agent stubbed, and its subsystems' coverage:
// `node safety-survey-workflow.test.mjs`, or with a copy of the workflow's path to test that instead.
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { load, run as runBody, checker } from './harness.mjs'
const file = process.argv[2] || fileURLToPath(new URL('./safety-survey-workflow.js', import.meta.url))
const { src, body } = load(file)
const run = (args, behave) => runBody(body, args, behave)
const { check, done } = checker()
const repo = p => fileURLToPath(new URL(`../../${p}`, import.meta.url))

const WT = '/r/.claude/worktrees/wf_run1-'
const KERNEL = { subsystem: 'kernel-objects', id: 'kobj', what: 'w', paths: ['kernel/src/syscall/', 'kernel/src/object/'] }
const USER = { subsystem: 'filesystems', id: 'fs', what: 'w', paths: ['userspace/fs-server-fat/'] }
const scope = (o = {}) => ({ ok: true, stop_reason: '', worktree: WT + '1', head: 'hhhh1234', inventory: '| INV |', entry_points: ['e'], boundaries: ['b'], hot_spots: ['h'], kernel_access: ['k'], deferrals: [], tests: [], claude_md: [], summary: 's', ...o })
const d = (file, line, o = {}) => ({ file, line, defect: 'd', scenario: 's', evidence: 'e', demonstrated: true, needs_boot: false, severity: 'medium', ...o })
const op = (pattern, o = {}) => ({ kind: 'encapsulate', pattern, sites: 3, counted_by: 'grep', examples: ['a.rs:1'], abstraction: 'x', home: 'libos', closes: 'c', depends_on: 'nothing', value: 'high', effort: 'medium', ...o })
const found = (defects, opportunities = []) => ({ held_up: ['held'], defects, opportunities })
const rp = r => r.calls.find(c => c.label === 'report')?.prompt || ''
const between = (p, from, to) => { const i = p.indexOf(from); return i < 0 ? [] : JSON.parse(p.slice(i + from.length, p.indexOf(to, i))) }
const reportDefects = r => between(rp(r), 'severity:\n', '\n\nOpportunities')
const reportOpps = r => between(rp(r), 'value:\n', '\n\nYour final message')

for (const bad of [undefined, {}, { ...USER, paths: [] }, { ...USER, id: 'F S' }, { ...USER, subsystem: '../x' }]) {
  const r = await run(bad, () => null)
  check(`bad args refused: ${JSON.stringify(bad)}`, !!r.error && r.calls.length === 0, r.error)
}

let r = await run(USER, () => scope({ ok: false, stop_reason: 'no such path' }))
check('a scope that stops ends the run, and says why', r.calls.length === 1 && r.result?.stopped === 'no such path' && r.logs.includes('Stopped: no such path'))

r = await run(KERNEL, l => {
  if (l === 'scope') return scope()
  if (l === 'survey:abi') return found([d('a.rs', 1, { severity: 'high' }), d('b.rs', 2, { needs_boot: true, demonstrated: false }), d('c.rs', 3, { needs_boot: true })], [op('raw syscalls')])
  if (l === 'survey:ownership') return found([d('d.rs', 4, { demonstrated: false })])
  if (l === 'survey:opportunities') return found([], [op('owned handles'), op('mappings', { value: 'low' })])
  if (l.startsWith('survey:')) return found([])
  if (l === 'verify:boot') return { verdicts: [{ id: 'abi-2', verdict: 'demonstrated', severity: 'high', evidence: 'booted', duplicate_of: 'ownership-1' }, { id: 'abi-3', verdict: 'refuted', severity: 'low', evidence: 'no' }, { id: 'ownership-1', verdict: 'refuted', severity: 'low', evidence: 'x' }] }
  return 'SECTION'
})
check('a kernel subsystem with boot defects: 8 agents, the concurrency lens among them', r.calls.length === 8 && r.calls.some(c => c.label === 'survey:concurrency'), r.calls.map(c => c.label).join(','))
const rd = reportDefects(r)
check('a booted defect is demonstrated and keeps its duplicate', rd.find(x => x.file === 'b.rs')?.demonstrated === true && rd.find(x => x.file === 'b.rs')?.duplicate_of === 'ownership-1')
check('a booted defect takes the verifier\'s severity', rd.find(x => x.file === 'b.rs')?.severity === 'high')
check('a refuted boot defect is dropped', !rd.some(x => x.file === 'c.rs') && r.result?.counts.refuted === 1)
check('a verdict for a defect not given to the verifier is ignored', rd.find(x => x.file === 'd.rs')?.verdict === 'unverified' && r.logs.some(m => /ownership-1, which it was not given/.test(m)))
const ro = reportOpps(r)
check('every lens\'s opportunities reach the report, numbered apart from defects', ro.length === 3 && ro.some(o => o.id === 'abi-o1') && ro.some(o => o.id === 'opportunities-o2') && r.result?.counts.opportunities === 3)
check('no opportunity goes to the boot verifier', !(r.calls.find(c => c.label === 'verify:boot')?.prompt || '').includes('owned handles'))
check('the inventory reaches every reviewer and the report', r.calls.filter(c => c.label.startsWith('survey:') || c.label === 'report').every(c => c.prompt.includes('| INV |')))
check('what held up reaches the report', rp(r).includes('abi: held'))
check('the section is returned', r.result?.section === 'SECTION' && r.result?.head === 'hhhh1234')
check('the report numbers by the subsystem id and removes the run\'s worktrees', /id, for numbering: kobj\./.test(rp(r)) && rp(r).includes(`those whose path starts ${WT}.`))
check('every reviewer checks out the scope\'s commit', r.calls.filter(c => c.label.startsWith('survey:')).every(c => c.prompt.includes('git checkout --detach hhhh1234')))
check('the boot verifier reads the survey\'s verify.md, which grades by the survey\'s rules', (r.calls.find(c => c.label === 'verify:boot')?.prompt || '').includes('.claude/skills/safety-survey/verify.md') && readFileSync(repo('.claude/skills/safety-survey/verify.md'), 'utf8').includes('.claude/skills/safety-survey/rules.md'))

r = await run(USER, l => l === 'scope' ? scope() : l.startsWith('survey:') ? found([], [op('p')]) : 'SECTION')
check('a userspace subsystem has no concurrency lens and no boot', !r.calls.some(c => c.label === 'survey:concurrency' || c.label === 'verify:boot') && r.calls.length === 6)

r = await run(USER, l => l === 'scope' ? scope() : l === 'survey:soundness' ? null : l.startsWith('survey:') ? found([]) : 'SECTION')
check('a dead lens is named to the report and returned', /Lenses that returned nothing: soundness/.test(rp(r)) && r.result?.lenses_missing.join() === 'soundness')
check('a dead lens is not among those that surveyed', /Lenses that surveyed: abi, ownership, opportunities\./.test(rp(r)))

const boot = d('b.rs', 2, { needs_boot: true, severity: 'low' })
r = await run(USER, l => l === 'scope' ? scope() : l === 'survey:abi' ? found([boot]) : l.startsWith('survey:') ? found([]) : l === 'verify:boot' ? { verdicts: [] } : 'SECTION')
check('a boot defect the verifier did not answer stays, unverified', reportDefects(r).find(x => x.file === 'b.rs')?.verdict === 'unverified' && r.result?.counts.refuted === 0)
r = await run(USER, l => l === 'scope' ? scope() : l === 'survey:abi' ? found([boot]) : l.startsWith('survey:') ? found([]) : l === 'verify:boot' ? null : 'SECTION')
check('every boot defect stays, unverified, when the verifier dies', reportDefects(r).find(x => x.file === 'b.rs')?.verdict === 'unverified' && r.result?.counts.refuted === 0)
r = await run(USER, l => l === 'scope' ? scope() : l.startsWith('survey:') ? null : 'NO SURVEY')
check('every lens dead: an error, worktrees still removed', !!r.error && /NO SURVEY/.test(rp(r)) && rp(r).includes(WT), r.error)
r = await run(USER, l => l === 'scope' ? scope({ worktree: '/home/kelby/src/nitrox' }) : l.startsWith('survey:') ? found([]) : 'SECTION')
check('a scope worktree outside .claude/worktrees removes nothing', /Remove no worktree/.test(rp(r)) && !rp(r).includes('/home/kelby/src/nitrox'))

r = await run(USER, l => {
  if (l === 'scope') return scope()
  if (l === 'survey:abi') return found([d('a.rs', 1), d('b.rs', 2, { needs_boot: true })], [op('p')])
  if (l === 'survey:soundness') return null
  if (l.startsWith('survey:')) return found([])
  if (l === 'verify:boot') return { verdicts: [{ id: 'abi-2', verdict: 'refuted', severity: 'low', evidence: 'no' }] }
  return null
})
const dead = r.result || {}
check('a dead report returns the items and the inventory unwritten', !r.error && dead.section === null && dead.opportunities?.length === 1 && dead.inventory === '| INV |' && /no report/.test(dead.error), r.error)
check('a dead report returns only the defects a boot did not refute', dead.defects?.length === 1 && dead.defects[0].file === 'a.rs')
check('a dead report returns what the section is written from: what held, the dead lenses, the counts, the head', dead.held_up?.includes('abi: held') && dead.lenses_missing?.join() === 'soundness' && dead.counts?.found === 2 && dead.counts?.refuted === 1 && dead.counts?.reported === 1 && dead.counts?.opportunities === 1 && dead.head === 'hhhh1234' && dead.subsystem === 'filesystems')

const want = { scope: 'sonnet', 'survey:abi': undefined, 'survey:ownership': undefined, 'survey:soundness': undefined, 'survey:concurrency': undefined, 'survey:opportunities': undefined, 'verify:boot': 'sonnet', report: 'sonnet' }
r = await run(KERNEL, l => l === 'scope' ? scope() : l.startsWith('survey:') ? found([d('a.rs', 1, { needs_boot: true })]) : l === 'verify:boot' ? { verdicts: [] } : 'SECTION')
check('each stage on its model', r.calls.every(c => want[c.label] === c.opts.model), r.calls.map(c => `${c.label}=${c.opts.model}`).join(','))
check('every agent but the report in its own worktree', r.calls.every(c => (c.label === 'report') === (c.opts.isolation !== 'worktree')))
check('MAX_AGENTS is used', (src.match(/MAX_AGENTS/g) || []).length >= 2)
const named = new Set(src.match(/\$\{DIR\}\/[\w./-]+\.md/g) || [])
check('the rules, scope, verify and report files are named', ['rules', 'scope', 'verify', 'report'].every(n => named.has(`\${DIR}/${n}.md`)))
for (const n of named) {
  const path = n.replace('${DIR}', '.claude/skills/safety-survey')
  check(`${path} exists`, existsSync(repo(path)))
}

// The subsystems: well formed, and together covering every userspace crate and kernel/src entry.
const toml = readFileSync(repo('.claude/skills/safety-survey/subsystems.toml'), 'utf8')
const entries = toml.split('[[subsystem]]').slice(1).map(e => ({
  name: e.match(/^name = "([^"]+)"/m)?.[1],
  id: e.match(/^id = "([^"]+)"/m)?.[1],
  paths: [...(e.match(/^paths = \[([^\]]*)\]/m)?.[1] || '').matchAll(/"([^"]+)"/g)].map(m => m[1]),
}))
check('every subsystem has a name, an id the workflow takes, and paths', entries.length > 0 && entries.every(e => /^[a-z][a-z0-9-]*$/.test(e.name || '') && /^[a-z]+$/.test(e.id || '') && e.paths.length > 0))
check('names and ids are unique', new Set(entries.map(e => e.name)).size === entries.length && new Set(entries.map(e => e.id)).size === entries.length)
const all = entries.flatMap(e => e.paths)
const missingPaths = all.filter(p => !existsSync(repo(p)))
check('every path exists', missingPaths.length === 0, missingPaths.join(', '))
const crates = readdirSync(repo('userspace')).filter(n => existsSync(repo(`userspace/${n}/Cargo.toml`))).map(n => `userspace/${n}/`)
const kernel = readdirSync(repo('kernel/src')).map(n => statSync(repo(`kernel/src/${n}`)).isDirectory() ? `kernel/src/${n}/` : `kernel/src/${n}`)
const uncovered = [...crates, ...kernel].filter(x => !all.some(p => p === x || (p.endsWith('/') && x.startsWith(p))))
check('every userspace crate and kernel/src entry is in a subsystem', uncovered.length === 0, uncovered.join(', '))
done()
