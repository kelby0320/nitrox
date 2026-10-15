export const meta = {
  name: 'pr-review-workflow',
  description: 'Review a Nitrox PR: a scope, reviewers with one lens each, a boot for findings that need one, one report posted to the PR',
  whenToUse: 'Only through /pr-review, which the maintainer starts; never by the author of the PR on its own',
  phases: [
    { title: 'Scope', detail: 'the head, the base, the files, CI on the head' },
    { title: 'Review', detail: 'one agent a lens, each in its own worktree' },
    { title: 'Verify', detail: 'only findings that need a QEMU boot to show' },
    { title: 'Report', detail: 'one comment on the PR' },
  ],
}

// A scope, at most five reviewers, a boot verifier when a finding needs one, and the report.
const MAX_AGENTS = 8
const DIR = '.claude/skills/pr-review'
// The session's model (Opus) where a lens finds subtle defects; Sonnet for mechanical or
// reproduction-heavy stages.
const SONNET = 'sonnet'
const KERNEL_OR_ABI = ['kernel/', 'userspace/libkern/', 'userspace/librsproto/', 'docs/spec/']

const pr = Number(args && args.pr)
if (!Number.isInteger(pr) || pr <= 0) {
  throw new Error('pr-review-workflow takes args {pr: <number>, head?: <commit>}')
}
const pinned = args.head ? String(args.head) : null

const touchesKernelOrAbi = s =>
  s.touches_kernel_or_abi || s.files.some(f => KERNEL_OR_ABI.some(p => f.startsWith(p)))

const LENSES = [
  { key: 'logic', title: 'logic and boundaries' },
  { key: 'error-paths', title: 'error paths and partial state' },
  { key: 'tests', title: 'tests', model: SONNET },
  { key: 'rules-and-docs', title: 'rules and docs', model: SONNET },
  { key: 'architecture', title: 'architecture and the ABI', only: touchesKernelOrAbi },
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
    base: { type: 'string' },
    files: strings,
    claude_md: strings,
    tests: strings,
    touches_kernel_or_abi: { type: 'boolean' },
    summary: { type: 'string' },
  },
  required: [
    'ok', 'stop_reason', 'worktree', 'head', 'base', 'files', 'claude_md', 'tests', 'touches_kernel_or_abi', 'summary',
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
          severity: { type: 'string', enum: ['blocking', 'worth-fixing', 'optional'] },
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
          severity: { type: 'string', enum: ['blocking', 'worth-fixing', 'optional'] },
          evidence: { type: 'string' },
          duplicate_of: { type: 'string' },
        },
        required: ['id', 'verdict', 'severity', 'evidence'],
      },
    },
  },
  required: ['verdicts'],
}

phase('Scope')
const scope = await agent(
  `Scope Nitrox PR #${pr} for review${pinned ? `, at head ${pinned} rather than the PR's latest` : ''}. ` +
    `Read ${DIR}/scope.md and follow it. If you stop, still fill every field: empty strings, empty lists, false.`,
  { label: 'scope', phase: 'Scope', schema: SCOPE, isolation: 'worktree', model: SONNET, effort: 'low' },
)
if (!scope) throw new Error('the scope agent returned nothing')
if (!scope.ok) {
  log(`Stopped: ${scope.stop_reason}`)
  return { pr, stopped: scope.stop_reason }
}
if (pinned && !scope.head.startsWith(pinned) && !pinned.startsWith(scope.head)) {
  throw new Error(`asked to review ${pinned}, but the scope named ${scope.head}`)
}

// Every agent's worktree is a sibling of the scope's, named for this run: `wf_<run>-<n>`.
const run = scope.worktree.match(/^(.*\/\.claude\/worktrees\/)(wf_[^/]+-)\d+\/?$/)
const worktreePrefix = run ? run[1] + run[2] : null
if (!worktreePrefix) log(`The scope's worktree ${scope.worktree} is not a run's; no worktree will be removed`)

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
// A barrier: the report needs every lens's findings, and which lenses returned none.
const reviews = await parallel(
  lenses.map(l => () =>
    agent(
      `You review Nitrox PR #${pr} through one lens: ${l.title}. ` +
        `${setup(`${DIR}/lenses/${l.key}.md`)}\n\n${context}\n\n` +
        `Report everything suspicious in your lens, with its evidence: do not hold back a real doubt, and do not ` +
        `pad. Go after what matters most first — a wrong ` +
        `result or a crash before a missing test of a trivial path — and stop when further digging turns up ` +
        `only optional notes. ` +
        `Demonstrate each finding or mark it undemonstrated, and give it a severity as ${DIR}/verify.md defines ` +
        `them: no one checks your findings before the author, who reproduces each. ` +
        `Do not boot QEMU; a finding only a boot can show is needs_boot. ` +
        `In held_up, list briefly what you checked that held, such as a mutation a test caught. ` +
        `Restore every probe.`,
      { label: `review:${l.key}`, phase: 'Review', schema: FOUND, isolation: 'worktree', model: l.model },
    ),
  ),
)

const findings = []
const heldUp = []
const missing = []
reviews.forEach((r, i) => {
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
// After the reviewers, so the one agent booting QEMU has the machine to itself.
const booted = boot.length
  ? await agent(
      `You verify findings from a review of Nitrox PR #${pr} that only a QEMU boot can show. ` +
        `${setup(`${DIR}/verify.md`)}\n\n${context}\n\n` +
        `You alone in this review boot QEMU, one gate at a time; nothing else runs beside you. ` +
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
const posted = await agent(
  nobody
    ? `Every reviewer of Nitrox PR #${pr} returned nothing, so there is no review. Post nothing. Read ` +
        `${DIR}/report.md for how to remove worktrees. ${cleanup} Then your final message is the single line NO REVIEW.`
    : `You write and post the review of Nitrox PR #${pr}. Read ${DIR}/reviewer.md and ${DIR}/report.md, and ` +
        `follow report.md. Check nothing out and change no file in this checkout: the comment's body is the only ` +
        `file you write, in a temporary directory.\n\n${context}\n\n` +
        `Lenses reviewed: ${lenses.filter(l => !missing.includes(l.key)).map(l => l.key).join(', ')}.\n` +
        `Lenses that returned nothing: ${missing.join(', ') || 'none'}.\n` +
        `Counts: ${counts.found} found, ${counts.refuted} refuted by a boot, ${counts.reported} to report.\n` +
        `${cleanup}\n\n` +
        `What the reviewers checked that held:\n${heldUp.join('\n') || 'nothing listed'}\n\n` +
        `Findings, each with its reviewer's evidence and severity:\n${JSON.stringify(reported, null, 1)}\n\n` +
        `Your final message is the comment's body exactly as posted, with nothing before or after it.`,
  { label: 'report', phase: 'Report', model: SONNET, effort: 'medium' },
)
if (nobody) throw new Error(`every reviewer of PR #${pr} returned nothing; no review was posted`)
if (!posted) {
  throw new Error(
    `the report agent returned nothing: check PR #${pr} for a comment, and ` +
      `${worktreePrefix || '.claude/worktrees/'}* for worktrees left behind`,
  )
}

return { pr, head: scope.head, base: scope.base, counts, lenses_missing: missing, posted }
