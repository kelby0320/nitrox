export const meta = {
  name: 'bug-bash-workflow',
  description: 'Hunt bugs in one Nitrox subsystem as main has it: a scope, reviewers with one lens each, a boot for findings that need one, the findings returned for the triage file',
  whenToUse: 'Only through /bug-bash, which the maintainer starts',
  phases: [
    { title: 'Scope', detail: "main's commit, the entry points, the trust boundaries" },
    { title: 'Hunt', detail: 'one agent a lens, each in its own worktree' },
    { title: 'Verify', detail: 'only findings that need a QEMU boot to show' },
    { title: 'Report', detail: 'the section for docs/planning/bug-bash.md, returned, not written' },
  ],
}

// A scope, at most five reviewers, a boot verifier when a finding needs one, and the report.
const MAX_AGENTS = 8
const DIR = '.claude/skills/bug-bash'
const SHARED = '.claude/skills/pr-review/lenses'
const SONNET = 'sonnet'
const SEVERITY = ['high', 'medium', 'low']

const a = args || {}
const name = String(a.subsystem || '')
const id = String(a.id || '')
const what = String(a.what || '')
const paths = Array.isArray(a.paths) ? a.paths.map(String).filter(Boolean) : []
if (!/^[a-z][a-z0-9-]*$/.test(name) || !/^[a-z]+$/.test(id) || paths.length === 0) {
  throw new Error('bug-bash-workflow takes args {subsystem: <name>, id: <prefix>, what: <text>, paths: [<path>, ...]}')
}
const inKernel = paths.some(p => p.startsWith('kernel/'))

const LENSES = [
  { key: 'logic', file: `${SHARED}/logic.md`, title: 'logic and boundaries' },
  { key: 'error-paths', file: `${SHARED}/error-paths.md`, title: 'error paths and partial state' },
  { key: 'trust-boundary', file: `${DIR}/lenses/trust-boundary.md`, title: 'the trust boundary' },
  { key: 'tests', file: `${DIR}/lenses/tests.md`, title: 'tests', model: SONNET },
  { key: 'architecture', file: `${SHARED}/architecture.md`, title: 'architecture and the ABI', only: () => inKernel },
]
if (1 + LENSES.length + 1 + 1 > MAX_AGENTS) throw new Error(`more than ${MAX_AGENTS} agents planned`)

const strings = { type: 'array', items: { type: 'string' } }

const SCOPE = {
  type: 'object',
  properties: {
    ok: { type: 'boolean' },
    stop_reason: { type: 'string' },
    worktree: { type: 'string' },
    head: { type: 'string' },
    entry_points: strings,
    trust_boundaries: strings,
    deferrals: strings,
    tests: strings,
    claude_md: strings,
    summary: { type: 'string' },
  },
  required: [
    'ok', 'stop_reason', 'worktree', 'head', 'entry_points', 'trust_boundaries', 'deferrals', 'tests', 'claude_md',
    'summary',
  ],
}

const FOUND = {
  type: 'object',
  properties: {
    held_up: strings,
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          defect: { type: 'string' },
          scenario: { type: 'string' },
          evidence: { type: 'string' },
          demonstrated: { type: 'boolean' },
          needs_boot: { type: 'boolean' },
          severity: { type: 'string', enum: SEVERITY },
        },
        required: ['file', 'line', 'defect', 'scenario', 'evidence', 'demonstrated', 'needs_boot', 'severity'],
      },
    },
  },
  required: ['held_up', 'findings'],
}

const VERIFIED = {
  type: 'object',
  properties: {
    verdicts: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          verdict: { type: 'string', enum: ['demonstrated', 'refuted', 'unverified'] },
          severity: { type: 'string', enum: SEVERITY },
          evidence: { type: 'string' },
          duplicate_of: { type: 'string' },
        },
        required: ['id', 'verdict', 'severity', 'evidence'],
      },
    },
  },
  required: ['verdicts'],
}

const subject = `${name} (${what}), whose code starts in ${paths.join(', ')}`

phase('Scope')
const scope = await agent(
  `Scope a bug bash of one Nitrox subsystem: ${subject}. Read ${DIR}/rules.md and ${DIR}/scope.md and follow ` +
    `scope.md. If you stop, still fill every field: empty strings, empty lists, false.`,
  { label: 'scope', phase: 'Scope', schema: SCOPE, isolation: 'worktree', model: SONNET, effort: 'low' },
)
if (!scope) throw new Error('the scope agent returned nothing')
if (!scope.ok) {
  log(`Stopped: ${scope.stop_reason}`)
  return { subsystem: name, stopped: scope.stop_reason }
}

// Every agent's worktree is a sibling of the scope's, named for this run: `wf_<run>-<n>`.
const run = scope.worktree.match(/^(.*\/\.claude\/worktrees\/)(wf_[^/]+-)\d+\/?$/)
const worktreePrefix = run ? run[1] + run[2] : null
if (!worktreePrefix) log(`The scope's worktree ${scope.worktree} is not a run's; no worktree will be removed`)

const list = xs => (xs.length ? xs.map(x => `- ${x}`).join('\n') : 'none')
const context = [
  `Subsystem: ${subject}. main at ${scope.head}.`,
  `What it does, as the scope read it (a claim to check): ${scope.summary}`,
  `Entry points:\n${list(scope.entry_points)}`,
  `Trust boundaries:\n${list(scope.trust_boundaries)}`,
  `Deferrals already recorded, not findings:\n${list(scope.deferrals)}`,
  `Host tests:\n${list(scope.tests)}`,
  `CLAUDE.md files governing it: ${scope.claude_md.join(', ')}`,
].join('\n\n')

const setup = file =>
  `Before anything else, read ${DIR}/rules.md and ${file}. Then \`git fetch origin main\` and ` +
  `\`git checkout --detach ${scope.head}\` in this worktree.`

phase('Hunt')
const lenses = LENSES.filter(l => !l.only || l.only())
// A barrier: the report needs every lens's findings, and which lenses returned none.
const hunts = await parallel(
  lenses.map(l => () =>
    agent(
      `You hunt for bugs in one Nitrox subsystem through one lens: ${l.title}. ${setup(l.file)}\n\n${context}\n\n` +
        `Start at the entry points and trust boundaries, and follow the data and the calls wherever they lead. ` +
        `Go after what matters most first — a crash, corruption, a way past a capability — and stop when further ` +
        `digging turns up only low-severity notes. Demonstrate each finding or mark it undemonstrated, and give it ` +
        `a severity as rules.md defines them. Do not boot QEMU; a finding only a boot can show is needs_boot. ` +
        `In held_up, list briefly what you checked that held, such as a break a test caught. Restore every probe.`,
      { label: `hunt:${l.key}`, phase: 'Hunt', schema: FOUND, isolation: 'worktree', model: l.model },
    ),
  ),
)

const findings = []
const heldUp = []
const missing = []
hunts.forEach((r, i) => {
  const key = lenses[i].key
  if (!r) {
    missing.push(key)
    log(`The ${key} reviewer returned nothing`)
    return
  }
  heldUp.push(...r.held_up.map(h => `${key}: ${h}`))
  r.findings.forEach((f, j) => findings.push({ ...f, id: `${key}-${j + 1}`, lens: key }))
})
log(`${findings.length} findings from ${lenses.length - missing.length} of ${lenses.length} reviewers`)

phase('Verify')
const boot = findings.filter(f => f.needs_boot)
// After the hunt, so the one agent booting QEMU has the machine to itself.
const booted = boot.length
  ? await agent(
      `You verify findings from a bug bash of one Nitrox subsystem that only a QEMU boot can show. ` +
        `${setup(`${DIR}/verify.md`)}\n\n${context}\n\n` +
        `You alone in this bug bash boot QEMU, one gate at a time; nothing else runs beside you. ` +
        `Restore every probe. Return one verdict for each finding, by id.\n\n` +
        `Findings:\n${JSON.stringify(boot, null, 1)}`,
      { label: 'verify:boot', phase: 'Verify', schema: VERIFIED, isolation: 'worktree', model: SONNET },
    )
  : null
const bootIds = new Set(boot.map(f => f.id))
const verdicts = new Map()
for (const v of booted ? booted.verdicts : []) {
  if (bootIds.has(v.id)) verdicts.set(v.id, v)
  else log(`The boot verifier answered ${v.id}, which it was not given; ignored`)
}
const judged = findings.map(f => {
  if (!f.needs_boot) return { ...f, verdict: f.demonstrated ? 'demonstrated' : 'unverified' }
  const v = verdicts.get(f.id)
  if (!v) return { ...f, verdict: 'unverified', verification: 'the boot verifier returned no verdict for it' }
  return {
    ...f,
    demonstrated: v.verdict === 'demonstrated',
    verdict: v.verdict,
    severity: v.severity,
    verification: v.evidence,
    duplicate_of: v.duplicate_of,
  }
})
const reported = judged.filter(f => f.verdict !== 'refuted')
const counts = { found: findings.length, refuted: judged.length - reported.length, reported: reported.length }
log(`${counts.found} found, ${counts.refuted} refuted by a boot, ${counts.reported} to report`)

phase('Report')
const cleanup = worktreePrefix
  ? `Remove this run's worktrees and their branches as report.md says: those whose path starts ${worktreePrefix}.`
  : 'Remove no worktree: this run did not name its own.'
const nobody = missing.length === lenses.length
const section = await agent(
  nobody
    ? `Every reviewer of a bug bash of ${name} returned nothing, so there is nothing to report. Read ` +
        `${DIR}/report.md for how to remove worktrees. ${cleanup} Then your final message is the single line NO HUNT.`
    : `You write up a bug bash of one Nitrox subsystem. Read ${DIR}/rules.md and ${DIR}/report.md, and follow ` +
        `report.md. Check nothing out, change no file and post nothing.\n\n${context}\n\n` +
        `The subsystem's id, for numbering: ${id}.\n` +
        `Lenses that hunted: ${lenses.filter(l => !missing.includes(l.key)).map(l => l.key).join(', ')}.\n` +
        `Lenses that returned nothing: ${missing.join(', ') || 'none'}.\n` +
        `Counts: ${counts.found} found, ${counts.refuted} refuted by a boot, ${counts.reported} to report.\n` +
        `${cleanup}\n\n` +
        `What the reviewers checked that held:\n${heldUp.join('\n') || 'nothing listed'}\n\n` +
        `Findings, each with its reviewer's evidence and severity:\n${JSON.stringify(reported, null, 1)}\n\n` +
        `Your final message is the section, with nothing before or after it.`,
  { label: 'report', phase: 'Report', model: SONNET, effort: 'medium' },
)
if (nobody) throw new Error(`every reviewer of the ${name} bug bash returned nothing; there is no section`)
if (!section) {
  log('The report agent returned nothing: the findings are returned unwritten')
  return {
    subsystem: name,
    head: scope.head,
    counts,
    lenses_missing: missing,
    section: null,
    findings: reported,
    held_up: heldUp,
    error: `no report: write the section from these findings as ${DIR}/report.md says, and remove ` +
      `${worktreePrefix || '.claude/worktrees/'}* worktrees left behind`,
  }
}

return { subsystem: name, head: scope.head, counts, lenses_missing: missing, section }
