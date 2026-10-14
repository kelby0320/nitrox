export const meta = {
  name: 'pr-review-workflow',
  description: 'Review a Nitrox PR: a scope, reviewers with one lens each, verifiers that try to refute, one report posted to the PR',
  whenToUse: 'Only through /pr-review, which the maintainer starts; never by the author of the PR on its own',
  phases: [
    { title: 'Scope', detail: 'the head, the base, the files, CI on the head' },
    { title: 'Review', detail: 'one agent a lens, each in its own worktree' },
    { title: 'Verify', detail: 'each finding handed to an agent told to refute it' },
    { title: 'Report', detail: 'one comment on the PR' },
  ],
}

const MAX_AGENTS = 10
const DIR = '.claude/skills/pr-review'

const pr = Number(args && args.pr)
if (!Number.isInteger(pr) || pr <= 0) {
  throw new Error('pr-review-workflow takes args {pr: <number>, head?: <commit>}')
}
const pinned = args.head ? String(args.head) : null

const LENSES = [
  { key: 'logic', title: 'logic and boundaries' },
  { key: 'error-paths', title: 'error paths and partial state' },
  { key: 'tests', title: 'tests' },
  { key: 'rules-and-docs', title: 'rules and docs' },
  { key: 'architecture', title: 'architecture and the ABI', only: s => s.touches_kernel_or_abi },
]

const strings = { type: 'array', items: { type: 'string' } }

const SCOPE = {
  type: 'object',
  properties: {
    ok: { type: 'boolean' },
    stop_reason: { type: 'string' },
    head: { type: 'string' },
    base: { type: 'string' },
    files: strings,
    claude_md: strings,
    tests: strings,
    touches_kernel_or_abi: { type: 'boolean' },
    summary: { type: 'string' },
  },
  required: ['ok', 'stop_reason', 'head', 'base', 'files', 'claude_md', 'tests', 'touches_kernel_or_abi', 'summary'],
}

const FOUND = {
  type: 'object',
  properties: {
    worktree: { type: 'string' },
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
        },
        required: ['file', 'line', 'defect', 'scenario', 'evidence', 'demonstrated', 'needs_boot'],
      },
    },
  },
  required: ['worktree', 'held_up', 'findings'],
}

const VERIFIED = {
  type: 'object',
  properties: {
    worktree: { type: 'string' },
    verdicts: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          verdict: { type: 'string', enum: ['demonstrated', 'refuted', 'unverified'] },
          severity: { type: 'string', enum: ['blocking', 'worth-fixing', 'optional'] },
          evidence: { type: 'string' },
          duplicate_of: { type: 'string' },
        },
        required: ['id', 'verdict', 'severity', 'evidence'],
      },
    },
  },
  required: ['worktree', 'verdicts'],
}

phase('Scope')
const scope = await agent(
  `Scope Nitrox PR #${pr} for review${pinned ? `, at head ${pinned} rather than the PR's latest` : ''}. ` +
    `Read ${DIR}/scope.md and follow it. If you stop, still fill every field: empty strings, empty lists, false.`,
  { label: 'scope', phase: 'Scope', schema: SCOPE, isolation: 'worktree' },
)
if (!scope) throw new Error('the scope agent returned nothing')
if (!scope.ok) {
  log(`Stopped: ${scope.stop_reason}`)
  return { pr, stopped: scope.stop_reason }
}

const context = [
  `PR #${pr}: head ${scope.head}, base ${scope.base}.`,
  `What it does, as the scope read it (a claim to check): ${scope.summary}`,
  `Files changed: ${scope.files.join(', ')}`,
  `CLAUDE.md files governing them: ${scope.claude_md.join(', ')}`,
  `Tests added or changed: ${scope.tests.join('; ') || 'none'}`,
].join('\n')

const setup = file =>
  `Before anything else, read ${DIR}/reviewer.md and ${file}. Then bring the head into this worktree — ` +
  `\`git fetch origin pull/${pr}/head\`, and \`git fetch origin ${scope.head}\` if it is still missing — ` +
  `and \`git checkout --detach ${scope.head}\`. The diff under review is \`git diff ${scope.base} ${scope.head}\`.`

phase('Review')
const lenses = LENSES.filter(l => !l.only || l.only(scope))
// A barrier: every lens's findings are needed together to share them out among the verifiers.
const reviews = await parallel(
  lenses.map(l => () =>
    agent(
      `You review Nitrox PR #${pr} through one lens: ${l.title}. ${setup(`${DIR}/lenses/${l.key}.md`)}\n\n${context}\n\n` +
        `Report everything suspicious in your lens, with its evidence: verifiers will try to refute each finding, ` +
        `so do not hold back a real doubt, and do not pad. Do not boot QEMU; a finding only a boot can show is needs_boot. ` +
        `In held_up, list briefly what you checked that held, such as a mutation a test caught. ` +
        `Restore every probe. Return this worktree's absolute path.`,
      { label: `review:${l.key}`, phase: 'Review', schema: FOUND, isolation: 'worktree' },
    ),
  ),
)

const worktrees = []
const findings = []
const heldUp = []
reviews.forEach((r, i) => {
  const key = lenses[i].key
  if (!r) {
    log(`The ${key} reviewer returned nothing`)
    return
  }
  if (r.worktree) worktrees.push(r.worktree)
  heldUp.push(...r.held_up.map(h => `${key}: ${h}`))
  r.findings.forEach((f, j) => findings.push({ ...f, id: `${key}-${j + 1}`, lens: key }))
})
log(`${findings.length} findings from ${lenses.length} reviewers`)

phase('Verify')
const byFile = (a, b) => (a.file < b.file ? -1 : a.file > b.file ? 1 : a.line - b.line)
const boot = findings.filter(f => f.needs_boot).sort(byFile)
const plain = findings.filter(f => !f.needs_boot).sort(byFile)
// What is left of the ten once the scope, the reviewers, the report and a boot verifier are counted.
const room = MAX_AGENTS - 1 - lenses.length - 1 - (boot.length ? 1 : 0)
const n = Math.min(room, plain.length)
// Neighbouring findings in one batch, so one verifier sees the duplicates among them.
const batches = Array.from({ length: n }, (_, i) =>
  plain.slice(Math.floor((i * plain.length) / n), Math.floor(((i + 1) * plain.length) / n)),
)

const verify = (batch, mayBoot) =>
  `You verify findings from a review of Nitrox PR #${pr}. ${setup(`${DIR}/verify.md`)}\n\n${context}\n\n` +
  (mayBoot
    ? 'You alone in this review may boot QEMU, one gate at a time; nothing else runs beside you. '
    : 'Do not boot QEMU. ') +
  `Restore every probe. Return this worktree's absolute path and one verdict for each finding, by id.\n\n` +
  `Findings:\n${JSON.stringify(batch, null, 1)}`

const verified = await parallel(
  batches.map((b, i) => () =>
    agent(verify(b, false), { label: `verify:${i + 1}`, phase: 'Verify', schema: VERIFIED, isolation: 'worktree' }),
  ),
)
// After the others, so the one agent booting QEMU has the machine to itself.
if (boot.length) {
  verified.push(
    await agent(verify(boot, true), { label: 'verify:boot', phase: 'Verify', schema: VERIFIED, isolation: 'worktree' }),
  )
}

const verdicts = new Map()
for (const r of verified) {
  if (!r) continue
  if (r.worktree) worktrees.push(r.worktree)
  for (const v of r.verdicts) verdicts.set(v.id, v)
}
const judged = findings.map(f => {
  const v = verdicts.get(f.id) || {
    verdict: 'unverified',
    severity: 'worth-fixing',
    evidence: 'no verifier returned a verdict for it',
  }
  return { ...f, verdict: v.verdict, severity: v.severity, verification: v.evidence, duplicate_of: v.duplicate_of }
})
const survived = judged.filter(f => f.verdict !== 'refuted')
const counts = { found: findings.length, refuted: judged.length - survived.length, reported: survived.length }
log(`${counts.found} found, ${counts.refuted} refuted, ${counts.reported} to report`)

phase('Report')
const posted = await agent(
  `You write and post the review of Nitrox PR #${pr}. Read ${DIR}/reviewer.md and ${DIR}/report.md, and follow ` +
    `report.md. Check nothing out and change no file in this checkout: the comment's body is the only file you ` +
    `write, in a temporary directory.\n\n${context}\n\n` +
    `Counts: ${counts.found} found, ${counts.refuted} refuted, ${counts.reported} to report.\n` +
    `Worktrees to remove: ${worktrees.join(' ') || 'none'}\n\n` +
    `What the reviewers checked that held:\n${heldUp.join('\n') || 'nothing listed'}\n\n` +
    `Findings that survived verification:\n${JSON.stringify(survived, null, 1)}`,
  { label: 'report', phase: 'Report' },
)

return { pr, head: scope.head, base: scope.base, counts, posted }
