export const meta = {
  name: 'safety-survey-workflow',
  description: 'Survey one Nitrox subsystem as main has it for defects of sizes, ownership and lifetimes, and unsafe that could be removed, narrowed or encapsulated',
  whenToUse: 'Only through /safety-survey, which the maintainer starts',
  phases: [
    { title: 'Scope', detail: "main's commit, the inventory, the entry points and boundaries" },
    { title: 'Survey', detail: 'one agent a lens, each in its own worktree' },
    { title: 'Verify', detail: 'only defects that need a QEMU boot to show' },
    { title: 'Report', detail: 'the section for docs/planning/safety-survey.md, returned, not written' },
  ],
}

// A scope, at most five reviewers, a boot verifier when a defect needs one, and the report.
const MAX_AGENTS = 8
const DIR = '.claude/skills/safety-survey'
const BASH = '.claude/skills/bug-bash'
const SONNET = 'sonnet'
const SEVERITY = ['high', 'medium', 'low']

const a = args || {}
const name = String(a.subsystem || '')
const id = String(a.id || '')
const what = String(a.what || '')
const paths = Array.isArray(a.paths) ? a.paths.map(String).filter(Boolean) : []
if (!/^[a-z][a-z0-9-]*$/.test(name) || !/^[a-z]+$/.test(id) || paths.length === 0) {
  throw new Error('safety-survey-workflow takes args {subsystem: <name>, id: <prefix>, what: <text>, paths: [<path>, ...]}')
}
const inKernel = paths.some(p => p.startsWith('kernel/'))

const LENSES = [
  { key: 'abi', file: `${DIR}/lenses/abi-and-buffers.md`, title: 'ABI and buffers' },
  { key: 'ownership', file: `${DIR}/lenses/ownership.md`, title: 'ownership and lifetimes' },
  { key: 'soundness', file: `${DIR}/lenses/soundness.md`, title: 'unsafe soundness' },
  { key: 'concurrency', file: `${DIR}/lenses/concurrency.md`, title: 'concurrency under unsafe', only: () => inKernel },
  { key: 'opportunities', file: `${DIR}/lenses/opportunities.md`, title: 'opportunities' },
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
    inventory: { type: 'string' },
    entry_points: strings,
    boundaries: strings,
    hot_spots: strings,
    kernel_access: strings,
    deferrals: strings,
    tests: strings,
    claude_md: strings,
    summary: { type: 'string' },
  },
  required: [
    'ok', 'stop_reason', 'worktree', 'head', 'inventory', 'entry_points', 'boundaries', 'hot_spots', 'kernel_access',
    'deferrals', 'tests', 'claude_md', 'summary',
  ],
}

const DEFECT = {
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
}

const OPPORTUNITY = {
  type: 'object',
  properties: {
    kind: { type: 'string', enum: ['remove', 'narrow', 'encapsulate'] },
    pattern: { type: 'string' },
    sites: { type: 'integer' },
    counted_by: { type: 'string' },
    examples: strings,
    abstraction: { type: 'string' },
    home: { type: 'string' },
    closes: { type: 'string' },
    depends_on: { type: 'string' },
    value: { type: 'string', enum: SEVERITY },
    effort: { type: 'string', enum: ['small', 'medium', 'large'] },
  },
  required: [
    'kind', 'pattern', 'sites', 'counted_by', 'examples', 'abstraction', 'home', 'closes', 'depends_on', 'value', 'effort',
  ],
}

const FOUND = {
  type: 'object',
  properties: { held_up: strings, defects: { type: 'array', items: DEFECT }, opportunities: { type: 'array', items: OPPORTUNITY } },
  required: ['held_up', 'defects', 'opportunities'],
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
  `Scope a safety survey of one Nitrox subsystem: ${subject}. Read ${DIR}/rules.md and ${DIR}/scope.md and follow ` +
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
  `The inventory, \`cargo xtask unsafe-inventory\` over its paths:\n${scope.inventory || 'none returned'}`,
  `Entry points:\n${list(scope.entry_points)}`,
  `Boundaries:\n${list(scope.boundaries)}`,
  `Hot spots:\n${list(scope.hot_spots)}`,
  `How it reaches the kernel:\n${list(scope.kernel_access)}`,
  `Deferrals already recorded, not items:\n${list(scope.deferrals)}`,
  `Host tests:\n${list(scope.tests)}`,
  `CLAUDE.md files governing it: ${scope.claude_md.join(', ')}`,
].join('\n\n')

const setup = file =>
  `Before anything else, read ${DIR}/rules.md and ${file}. Then \`git fetch origin main\` and ` +
  `\`git checkout --detach ${scope.head}\` in this worktree.`

phase('Survey')
const lenses = LENSES.filter(l => !l.only || l.only())
// A barrier: the report needs every lens's items, and which lenses returned none.
const surveys = await parallel(
  lenses.map(l => () =>
    agent(
      `You survey one Nitrox subsystem through one lens: ${l.title}. ${setup(l.file)}\n\n${context}\n\n` +
        `Start at the boundaries, the entry points and the inventory, and follow the pointers and the data wherever ` +
        `they lead. This survey is wide by design: be thorough. Demonstrate each defect or mark it undemonstrated, and ` +
        `count each opportunity's sites, as rules.md says. Do not boot QEMU; a defect only a boot can show is ` +
        `needs_boot. In held_up, list briefly what you checked that held, such as a SAFETY argument found sound. ` +
        `Restore every probe.`,
      { label: `survey:${l.key}`, phase: 'Survey', schema: FOUND, isolation: 'worktree', model: l.model },
    ),
  ),
)

const defects = []
const opportunities = []
const heldUp = []
const missing = []
surveys.forEach((r, i) => {
  const key = lenses[i].key
  if (!r) {
    missing.push(key)
    log(`The ${key} reviewer returned nothing`)
    return
  }
  heldUp.push(...r.held_up.map(h => `${key}: ${h}`))
  r.defects.forEach((d, j) => defects.push({ ...d, id: `${key}-${j + 1}`, lens: key }))
  r.opportunities.forEach((o, j) => opportunities.push({ ...o, id: `${key}-o${j + 1}`, lens: key }))
})
log(`${defects.length} defects and ${opportunities.length} opportunities from ${lenses.length - missing.length} of ${lenses.length} reviewers`)

phase('Verify')
const boot = defects.filter(d => d.needs_boot)
// After the survey, so the one agent booting QEMU has the machine to itself.
const booted = boot.length
  ? await agent(
      `You verify defects from a safety survey of one Nitrox subsystem that only a QEMU boot can show. ` +
        `${setup(`${BASH}/verify.md`)}\n\n${context}\n\n` +
        `You alone in this survey boot QEMU, one gate at a time; nothing else runs beside you. ` +
        `Restore every probe. Return one verdict for each defect, by id.\n\n` +
        `Defects:\n${JSON.stringify(boot, null, 1)}`,
      { label: 'verify:boot', phase: 'Verify', schema: VERIFIED, isolation: 'worktree', model: SONNET },
    )
  : null
const bootIds = new Set(boot.map(d => d.id))
const verdicts = new Map()
for (const v of booted ? booted.verdicts : []) {
  if (bootIds.has(v.id)) verdicts.set(v.id, v)
  else log(`The boot verifier answered ${v.id}, which it was not given; ignored`)
}
const judged = defects.map(d => {
  if (!d.needs_boot) return { ...d, verdict: d.demonstrated ? 'demonstrated' : 'unverified' }
  const v = verdicts.get(d.id)
  if (!v) return { ...d, verdict: 'unverified', verification: 'the boot verifier returned no verdict for it' }
  return {
    ...d,
    demonstrated: v.verdict === 'demonstrated',
    verdict: v.verdict,
    severity: v.severity,
    verification: v.evidence,
    duplicate_of: v.duplicate_of,
  }
})
const reported = judged.filter(d => d.verdict !== 'refuted')
const counts = {
  found: defects.length,
  refuted: judged.length - reported.length,
  reported: reported.length,
  opportunities: opportunities.length,
}
log(`${counts.found} defects found, ${counts.refuted} refuted by a boot, ${counts.reported} to report; ${counts.opportunities} opportunities`)

phase('Report')
const cleanup = worktreePrefix
  ? `Remove this run's worktrees and their branches as report.md says: those whose path starts ${worktreePrefix}.`
  : 'Remove no worktree: this run did not name its own.'
const nobody = missing.length === lenses.length
const section = await agent(
  nobody
    ? `Every reviewer of a safety survey of ${name} returned nothing, so there is nothing to report. Read ` +
        `${DIR}/report.md for how to remove worktrees. ${cleanup} Then your final message is the single line NO SURVEY.`
    : `You write up a safety survey of one Nitrox subsystem. Read ${DIR}/rules.md and ${DIR}/report.md, and follow ` +
        `report.md. Check nothing out, change no file and post nothing.\n\n${context}\n\n` +
        `The subsystem's id, for numbering: ${id}.\n` +
        `Lenses that surveyed: ${lenses.filter(l => !missing.includes(l.key)).map(l => l.key).join(', ')}.\n` +
        `Lenses that returned nothing: ${missing.join(', ') || 'none'}.\n` +
        `Counts: ${counts.found} defects found, ${counts.refuted} refuted by a boot, ${counts.reported} to report; ` +
        `${counts.opportunities} opportunities.\n` +
        `${cleanup}\n\n` +
        `What the reviewers checked that held:\n${heldUp.join('\n') || 'nothing listed'}\n\n` +
        `Defects, each with its reviewer's evidence and severity:\n${JSON.stringify(reported, null, 1)}\n\n` +
        `Opportunities, each with its sites and value:\n${JSON.stringify(opportunities, null, 1)}\n\n` +
        `Your final message is the section, with nothing before or after it.`,
  { label: 'report', phase: 'Report', model: SONNET, effort: 'medium' },
)
if (nobody) throw new Error(`every reviewer of the ${name} safety survey returned nothing; there is no section`)
if (!section) {
  log('The report agent returned nothing: the items are returned unwritten')
  return {
    subsystem: name,
    head: scope.head,
    counts,
    lenses_missing: missing,
    section: null,
    inventory: scope.inventory,
    defects: reported,
    opportunities,
    held_up: heldUp,
    error: `no report: write the section from these items as ${DIR}/report.md says, and remove ` +
      `${worktreePrefix || '.claude/worktrees/'}* worktrees left behind`,
  }
}

return { subsystem: name, head: scope.head, counts, lenses_missing: missing, section }
