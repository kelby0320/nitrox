# Nitrox: Shell Design Notes (v1.1)

## Status

Working capture of shell design decisions. Semantics (execution model, error handling, type
system, function semantics), grammar (lexical/expression, statement/control-flow, pattern
matching, literals/ranges/modules), coreutils scope, and REPL/interactive behavior are all now
covered for a first pass. Remaining gaps are listed in §12 — none block implementation of the core
language and shell. Update in place as the design progresses; don't treat this as final.
Companion document: `nitrox-ui-composition-model-v1.md` (windows/widgets as resource servers,
window-to-window composition) — present in `docs/history/`, upstream of this one where they touch
(e.g. `form`'s mechanics), but this doc stands on its own for shell/language concerns.

### Changes in v1.1 (2026-07-22, consistency pass)

Editorial corrections found while checking the design against the implemented system; **no
semantic decisions were changed**. See `docs/planning/shell-coreutils-plan.md` for the build
plan and the full gap analysis this pass produced.

- **§1** — corrected a stale crate reference (`librt` → `libos`; `librt` was cut, decision log
  2026-07-13; in-process concurrency is libos tasks over `sys_wait`).
- **§9d** — reworded so the `Value` collection representation (`Table`/`List`/`Record`) reads as
  the *planned* enum shape, not an existing one. **Implementation reality (2026-07-22):** the
  in-memory `libstream` `Value` today carries only scalars + `Str`/`Bytes`/`Handle`; the `List`
  (0x07) and `Record` (0x08) wire tags are *reserved but `Unsupported`* in the codec. The whole
  data model in §5c/§6/§9d/§9f — tables, subset-match, CoW rebind, Arc-clone capture — depends on
  adding those variants and their codecs. This is the first substrate item in the build plan, not
  a given. The governing rule "**`Value` is exactly what TSM1 can represent**" (§6) becomes
  literally true once they land; until then it describes the target.
- Renumbered the trailing sections so numbering is contiguous (old §12 → §11 "REPL", old §13 →
  §12 "Open questions"); the missing §11 in v1 was a working-notes gap, not a dropped section.

## 1. Pipeline execution model

- **Concurrency:** Unix-style. Each stage in `a | b | c` is a concurrently-running process,
  streaming records to the next via IPC, not an iterator chain the shell drives serially.
- **Backpressure:** handled by the underlying IPC channel's bounded queue depth. A slow
  consumer's channel fills, the producer's write blocks/awaits. No shell-level flow control needed
  on top of this.
- **Early-consumer cancellation** (the `yes | head -1` case): falls out of existing primitives, no
  new mechanism. `head` reads what it needs and closes its read end; the producer's next write
  surfaces `PeerClosed`, which library code (`libstream`/`libos`) treats as "stop producing, exit
  cleanly." No signals involved, consistent with the system's existing no-signals stance.

### Error propagation — three distinct categories, three different owners

"An error in the pipeline" was ambiguous until split into what's actually being described:

| Category | Mechanism | Owner |
|---|---|---|
| Row-level data error (one record failed, stream continues) | TSM1 `error_tag` record, inline in the body | Each stage decides whether to handle or forward |
| Diagnostic output (warnings, progress, "skipping X") | `stderr` channel — separate from the pipe entirely | Bypasses the pipe by default, surfaces to display/log |
| Fatal stage failure (a stage crashes outright) | Process lifecycle notification (`ChildExited` / `PeerClosed`) | The shell, which spawned every stage and already watches these |

**Default for row-level errors:** pass-through. A stage with no specific handling for an
`error_tag` record forwards it untouched rather than silently dropping it. Prevents errors from
vanishing just because they were piped through commands that didn't think about them — they
surface wherever something finally looks (`display`, terminal). Deliberately biased toward
visibility over bash's common failure mode of errors disappearing into `| grep foo`.

**Default pipeline policy:** fail loud, don't fail silent. A non-zero or crashed stage anywhere
makes the overall pipeline report as failed by default. This does **not** mean downstream stages
get torn down — a later stage still finishes processing whatever data it already received.

**Eager-abort opt-in — `strict { ... }` block, not a global flag.** Deliberately local and
visible rather than a bash-`pipefail`-style ambient mode switch, consistent with the no-implicit-
capture/no-global-`$?`/no-ambient-env stance used throughout. Inside the block, first stage
failure causes the shell to terminate remaining stages immediately. Needs no new mechanism: the
shell already holds process handles for every stage it spawned, so "abort the rest" is an ordinary
capability-mediated terminate call on handles it already owns — not signal delivery, consistent
with the system's no-signals stance. Composes independently with `try`/`catch` (`try { strict { a
| b | c } } catch { ... }`) since they govern different things — `strict` changes behavior *during*
execution, `try`/`catch` governs control flow *after* the pipeline resolves.

**Composite exit status — first-class typed value, not a bash-style array hack:**

```rust
struct StageStatus {
    command: String,
    exit_status: i32,
    crashed: bool,     // true = no clean terminator was ever sent (process died unexpectedly)
    cancelled: bool,    // true = the shell proactively terminated this stage (strict-mode abort)
}

struct PipelineStatus {
    stages: Vec<StageStatus>,  // one per stage, in pipeline order
}
```

Just a `Record`/`Table` like anything else — no new mechanism. A derived boolean ("all stages
exited 0, none crashed") covers the casual `$?`-equivalent case without losing per-stage detail
for scripts that want it.

## 2. Control flow and error handling

- **`try`/`catch`** — sugar over branching on a Result-shaped value, *not* real exception
  unwinding. No stack-unwinding across unrelated call frames. Consistent with the system's
  rejection of hidden non-local control flow (no signals) elsewhere.
- **`?` operator** — Rust-style propagation. Exits the *current function*, returning the error to
  its caller. Chosen deliberately over bash's silent-continue-by-default, and over a generic
  exception model, given Rust is the explicit language inspiration and the implementation
  language.
- **No global `$?`.** Rejected as ambient state — invisible information flowing outside explicit
  data flow, the same category of thing (ambient authority, implicit env inheritance) rejected
  elsewhere in the system. Dissolved rather than worked around: pipelines are **expressions that
  evaluate to a value** (nushell/PowerShell-style), so `let result = my_pipeline` puts the status
  directly in hand — there's no separate "go check afterward" step for `$?` to exist for.
  `PipelineStatus` (§1) is the real answer to "what's the status of a multi-stage pipeline,"
  not a single scalar pretending to represent several stages.
- **REPL-only convenience binding** (name TBD, e.g. `$last`): holds the most recent top-level
  pipeline's result, scoped only to the interactive session. Deliberately unavailable inside
  function bodies or scripts — same pattern as Python's `_` / IPython's `Out[]`. Gets back bash's
  ad hoc interactive convenience without reintroducing ambient state into real functions.

## 3. Language shape

- **Split grammar**, not uniform command-invocation-shaped. Real keyword-level syntax for control
  flow (`if`, `for`, `let`, `def`, `try`/`catch`) — not commands that happen to take a block
  argument. Matches what nushell/PowerShell actually do underneath their pipeline-object framing;
  chosen because the shell's language is meant to work as a real scripting language, not just a
  command launcher.
- **Expression-position escape hatch for pipelines:** parens, `(pipeline)`, to use a pipeline
  result where an expression is expected — e.g. `if (files | count) > 0 { ... }`. Follows
  nushell/PowerShell convention directly.
- **Keyword-collision override: `^`**, matching the nushell precedent (`^ls` forces literal
  external resolution, bypassing keyword/builtin/generic-operator lookup entirely). One flag worth
  keeping in mind rather than treating as fully closed: `^` is a common bitwise-XOR symbol in
  C-family languages, so there's a minor collision risk if that operator is wanted later — judged
  low-risk for a pipeline/data-oriented language, but noted rather than silently decided.
- **Four-way command categorization** (resolved by the split-grammar decision plus the
  generic-operator distinction in §5c):

  | Category | Examples | Why |
  |---|---|---|
  | Language keywords | `if`, `for`, `let`, `def`, `try`/`catch` | Pure syntax, not commands at all |
  | Shell-state builtins | `cd`, `exit` | Must mutate the shell's own process state; an external process structurally can't reach back and do this |
  | Generic value operators | `filter`, `sort`, `select`, `save`, `open`, `each`, `map`, `display`, `expect`, `assert`, `format`, `last`, `skip`, `dedupe` | Generic dispatch over `Value`'s structural shape — can't cross IPC without knowing the schema ahead of time; see §5c and §10b |
  | External programs | `list`, `copy`, `move`, `remove`, `mkdir`, `touch`, `rename`, `date`, `sleep`, `whoami` | Ordinary userspace programs speaking TSM1 on stdio for pipeline composability. **Not** resource servers — resource servers (`fs-server-ext4`, block/device drivers) are a distinct, narrower category of long-running service speaking `librsproto`; an external program may be a *client* of one, but implementing `librsproto` is not required to participate in a shell pipeline. Full scope and naming rationale in §10 |

## 4. I/O: `save` / `open` replace redirection

`save <path>` (output) and `open <path>` (input) replace `>` / `<` **entirely** — no leftover
low-level byte-redirect case remains, because TSM1 already auto-wraps unstructured text output
into a single-column `Table<String>`; there's no such thing as a raw untyped byte stream in this
pipeline model to begin with. Format inferred from extension (`.csv`, `.json`, `.txt`, `.tsm`).

**`open` accepts multiple paths, concatenating them into one stream** — `open a.txt b.txt`. This
absorbs `cat`'s job entirely (§10a); "open these paths as one stream" is a more natural reading of
what `cat` was ever doing than a second, near-identical verb would be.

## 5. Functions

### 5a. Scoping and nesting

- **Named (`def`) functions do not capture their enclosing scope.** A function's parameter list
  is a complete, honest account of its inputs — nothing arrives except what's declared. Same
  reasoning as env vars being explicit namespace-scoped resources instead of ambient inheritance,
  and no global `$?`: anything that can silently read from wherever it happened to be defined
  can't be understood from its signature alone.
- **Anonymous closures are the deliberate exception** — they exist specifically to capture nearby
  locals (`ls | filter { |row| row.size > threshold }`), and forcing that through explicit
  parameters would make the common case painful for no real gain. Capture is by value, at
  creation (settled previously) — snapshots the closed-over variables when the closure is defined,
  avoiding the classic "closure captures the loop variable, everyone sees the final value" bug
  class without needing a borrow checker. This mirrors Rust directly: a nested `fn` doesn't
  capture; a `|x| ...` closure does.
- **`def` can nest inside `def`**, for local organization (a helper visible only inside the
  enclosing function). Same no-implicit-capture rule applies to the nested `def` — nesting is a
  visibility scope, not a capability grant. Again mirrors Rust's nested-`fn`-item behavior.
- **`def` bindings are hoisted** within their declaring scope, so two functions (including nested
  helpers) can call each other regardless of textual order — needed for mutual recursion. `let`
  stays sequential (must exist before use), since it's tied to actual evaluation, not a
  declaration.

### 5b. Calling convention — deliberately split from external-program invocation

Script functions and external programs are called differently on purpose, not unified under one
convention. Two different things (running your own code vs. invoking a program) get two different
shapes, which also happens to resolve how closures fit the type system (§5c):

```
# Script functions — parens, named args
def greet(name: String, loud: Bool = false, times: Int = 1) -> String {
    let punctuation = if loud { "!" } else { "." }
    return name ++ punctuation
}

greet("Alice")
greet("Alice", loud: true, times: 3)

# closures — trailing-block style, in-process only
let threshold = 1000
ls | filter { |row| row.size > threshold }

# External programs — bareword, --flag, unchanged
ls --long /some/path
sort size --reverse
```

- **Parameters:** positional for required inputs, named (`loud:`) for the rest — closer to
  keyword arguments than to external `--flag` syntax.
- **Defaults are evaluated fresh per call**, in the function's own parameter scope — not once at
  definition time. Deliberately avoids the classic mutable-default-argument trap (Python's
  `def f(x=[])`, where every caller who omits `x` shares the same object). This also lets a later
  default reference an earlier positional parameter: `def f(a: Int, b: Int = a + 1)`.
- **Variadic (rest) parameters** for the "list of files" case: `def cat_files(...paths:
  List<String>)`. Must be the last positional parameter.
- **Arity / unknown-named-arg mismatch is an error**, not silently padded with null — same
  fail-loud default used throughout (§1).
- **Return: implicit last-expression value; `return` only for early exit.** Matches Rust, and
  matches "pipelines are expressions" (§2) — a function body is itself an expression producing a
  value; `return` (and `?`) exist for exiting *before* the end, not as the normal way to hand back
  a result.
- **Pipeline-fill placeholder — settled: explicit `_` required whenever an argument list is
  present; implicit fill only for the bare, argument-free call.** Pure implicit-first-param-fill
  was rejected on a concrete correctness gap: nothing enforces that a function's first parameter
  is always the "data" slot (`def scale(factor: Int, data: Table)` would silently receive the
  piped table into `factor`). Generic value operators get to have implicit fill safely because
  they're a small, closed, system-standardized set where "first thing is the operand" is a hard
  contract; arbitrary `def` functions have no such guarantee, so the language shouldn't guess.
  ```
  ls | summarize                          # bare call, unambiguous — fills the sole slot
  ls | summarize(_, label: "src files")   # explicit list present — _ marks the pipe target
  ```
  Omitting `_` when an argument list is present is a parse-time error, not a silent guess. Applies
  only to `def`-function calls (parens/named convention) — external programs keep implicit stdin
  (a separate, already-settled mechanism) and generic operators keep their existing bareword form.
- `def`'d functions are themselves callable-by-reference (`filter(is_big)`), same as a literal
  closure — falls out for free once functions are real runtime values (§5c), no separate mechanism
  needed.

### 5c. Functions and closures are language-only values — not `Value`, not TSM1

Tying this to the same rule ascription already uses: **`Value` is exactly what TSM1 can
represent — no more, no less.** A closure can't cross a process boundary (nothing to encode it
as), so it was never a candidate for the `Value` enum; adding it would have been the actual
inconsistency — a member of the type system that provably can't survive IPC, sitting next to ones
that can. Named functions and closures are real, first-class, assignable runtime values, they just
live in a category TSM1 was never trying to cover — same relationship Python functions have to
JSON.

**Consequence for the builtin/external boundary:** anything that accepts a closure argument
(`filter`, `each`, `map`, a hypothetical `sort_by { |a,b| ... }`) cannot be an external program —
there's nothing to send over IPC. This is the actual dividing line behind the "generic value
operators" category in §3, and it isn't closure-specific: `sort size` needs the same generic,
schema-agnostic access to an arbitrary `Table` that `sort_by { ... }` does, whether or not a
closure is involved. The real rule: **builtins in this category do generic dispatch over `Value`'s
structural shape; external programs define their own fixed schema.** `save`/`open` belong here too
for the same reason — serializing an arbitrary table means walking whatever columns showed up,
the same generic-schema requirement as `sort`. Not a closed, hardcoded list either — a user-defined
`def` that wants to do its own generic dispatch over an arbitrary `Table` gets the same capability
for free; "generic operator over `Value`" is an open category, not a privileged blessed set.

Practical payoff: most of a typical pipeline (`ls | sort size | take 5 | select name | display`)
costs exactly one process boundary (`ls` producing over IPC) — everything after runs natively on
the in-memory `Value` tree inside the shell process, no re-serialization, no additional spawns.
Meaningfully de-risks how much the builtin/external boundary depends on process-spawn cost, since
the densest, most frequent chains no longer touch it at all.

## 6. Type system

- **`Value` is exactly what TSM1 can represent — no more, no less** (see §5c). This is the
  governing rule for what belongs in the `Value` enum at all: functions and closures are real
  runtime values but are deliberately excluded, since they can't cross a process boundary.
- **Dynamic core**, optional runtime-checked annotations — not static typing. Deliberate choice
  given the language is fully interpreted (no JIT, no compiler — closer to CPython/Lua than to
  Rust in this respect). Static typing's usual payoffs (compiler-driven representation choices,
  JIT specialization) don't apply without a compiler or JIT to exploit them; a sound static system
  would also need a coherent story for the boundary where a script calls an external program whose
  output schema isn't knowable until it actually runs — and that boundary is the majority of what
  a shell script does, not a rare edge case worth walling off.
- **Duck typing over structural types was already the committed core semantic** (`sort`/`filter`/
  `select` already work on whatever fields are present) — the type-annotation question was really
  "what optional layer sits on top of that dynamic core," not "static or dynamic" from scratch.
- **Mismatch is an error, not a warning.** Consistent with the fail-loud pipeline default (§1) — a
  warning can be silently accumulated and ignored, which is the exact failure mode already
  rejected elsewhere.
- **Type annotations are a textual notation for a TSM1 `Schema`, not a separate concept.** The
  wire format already carries `field_count` + typed `Field`s in every stream header before any row
  arrives. `Table<{name: String, size: Int}>` in the language *is* how you write a `Schema` by
  hand; checking an annotation against an incoming value is checking it against the schema already
  in that stream's header.
  - Practical consequence: for `Table` values, the shape check happens **once, at header-read
    time** — not per-row. Cost is independent of row count.
- **Subset match by default.** `Table<{size: Int}>` matches any table containing at least a `size:
  Int` field; extra columns are ignored. Consistent with the duck-typing commitment already
  load-bearing elsewhere — an annotation means "requires at least this shape," not "exactly this
  shape." An `exact` modifier could exist later for the rare case that wants it; not the default.
- **Ascription is the one real mechanism; several things are just spellings of it:**
  - `let x: T = expr` — binding-site ascription.
  - Function parameter / return type annotations — same check, at a call boundary.
  - `expect T` — ascription usable mid-pipeline, in expression position: `ls | filter size > 1000
    | expect Table<{name: String, size: Int}> | display`.
  - All three fail loud on mismatch, same underlying pass-or-throw pattern as `try`/`catch`/`?`.
- **`assert (predicate)` — a sibling to `expect`, not a fold into it.** `expect` checks *shape*
  (schema comparison); `assert` checks a *content* predicate (`ls | assert (count > 0)`). Same
  pipeline slot, same pass-or-throw contract, deliberately kept as separate keywords rather than
  one verb overloaded on argument type — keeps error messages specific ("shape mismatch" vs.
  "assertion failed") rather than merging two different jobs into one grammar rule.
- **Escape hatch for external programs:** annotate loosely (`let result: Table = some_tool
  --flag`) or not at all — schema is genuinely unknowable ahead of time, checked only once the
  actual TSM1 header arrives at runtime.
- **Error messages are schema diffs**, not vague type mismatches — possible specifically because
  the check is a real schema comparison:
  ```
  TypeError: summarize() parameter 'data' expects Table<{name: String, size: Int}>
    — got Table<{name: String, email: String}>, missing field 'size'
  ```

## 7. Illustrative sketch (early snapshot — see §8/§9 for the settled grammar this now reflects)

```
const THRESHOLD_DEFAULT: Int = 1000
let threshold = THRESHOLD_DEFAULT
let files = open ./src | filter { |row| row.size > threshold }

if (files | count) > 0 {
    display files
} else {
    print "nothing matched"
}

def summarize(data: Table<{name: String, size: Int}>, label: String = "result")
    -> Record<{label: String, total: Int}> {
    return { label: label, total: data | count }
}

ls --long
  | expect Table<{name: String, size: Int}>
  | assert (count > 0)
  | sort size --reverse
  | summarize(_, label: "src files")

strict {
    open ./input.csv | validate_schema | save ./output.csv
}

mut counts = { errors: 0, warnings: 0 }
for line in open ./log.txt {
    if line.level == "error" {
        counts.errors = counts.errors + 1
    }
}
```

## 8. Grammar: lexical and expression layer

First layer of grammar work — literals, operators, precedence, how pipelines and calls actually
parse. Statement/control-flow syntax, pattern matching, and remaining literal forms follow in §9.

### 8a. Precedence (highest to lowest, Rust/C-shaped)

```
1. Postfix         call(), .field, [index]
2. Unary           -x, !x
3. Multiplicative  * / %
4. Additive        + -   ++ (string concat)
5. Comparison      < <= > >=
6. Equality        == !=   ~= (regex match, see §10b)
7. Logical AND     &&
8. Logical OR      ||
9. Range           .. ..=
10. Pipe           |     (lowest, left-associative)
```

**`:` is not a general infix expression operator.** Restricted to fixed grammatical positions —
`let x: T`, function params (`name: String`), named args (`loud: true`), record literal fields.
Mid-expression ascription is spelled `expect T`, never bare `expr: T`. Avoids a real parsing
ambiguity between "named argument" and "ascription" at call sites.

### 8b. Bareword predicate arguments desugar to implicit closures

`filter size > 1000` and `filter { |row| row.size > threshold }` aren't two mechanisms — the
bareword form is sugar for an implicit single-parameter closure, the same "one real primitive,
multiple spellings" pattern used for `try`/`catch`+`?` and ascription+`expect`. Reserved implicit
parameter name: **`it`**. Bare identifiers inside the bareword form resolve as field-shorthand on
`it`:

```
filter size > 1000                    # sugar for:
filter { |it| it.size > 1000 }
```

Generic operators don't need a separate bareword-argument grammar rule at all under this — they
always take a closure; the parser recognizes "bare expression, no `|params|`" as the sugared case.

### 8c. Core expression grammar (sketch)

```
expr        := pipeline
pipeline    := range_expr ( "|" pipeline_stage )*
range_expr  := or_expr ( (".." "="?) or_expr )?
pipeline_stage := call_expr | closure_lit

call_expr   := IDENT arg_list?
             | IDENT "(" (named_arg | expr | "_") ("," ...)? ")"   # def-function form
arg_list    := (expr | "--" IDENT (expr)? )*                      # bareword/flag form

closure_lit := "{" "|" (param ("," param)*)? "|" statement* "}"   # pipes mandatory, always

or_expr     := and_expr ("||" and_expr)*
and_expr    := eq_expr ("&&" eq_expr)*
eq_expr     := cmp_expr (("==" | "!=") cmp_expr)*
cmp_expr    := add_expr (("<" | "<=" | ">" | ">=") add_expr)*
add_expr    := mul_expr (("+" | "-" | "++") mul_expr)*
mul_expr    := unary (("*" | "/" | "%") unary)*
unary       := ("-" | "!")? postfix
postfix     := primary ("." IDENT | "[" expr "]")*
primary     := literal | IDENT | "(" pipeline ")" | "_"
             | record_lit | list_lit | closure_lit
```

Ranges (`..`/`..=`) are language-only values, not `Value` variants and not TSM1-representable —
same rule as closures (§5c): a range isn't something meant to stream over IPC, it'd be materialized
into a `List<Int>` first if it needed to leave the process. Always ascending, `i64`-typed, no step
syntax yet — deliberately minimal, extensible later if a real need shows up.

`closure_lit` requiring `|params|` even when empty (`||`) fixed a real grammar bug rather than
being a style choice: with the pipes optional, `{ name }` was ambiguous between record-literal
shorthand and a zero-arg closure body. Making the pipes mandatory means `{` followed immediately by
`|` is unambiguously a closure, and anything else in that position is unambiguously a record — and
it's also why `closure_lit` needed adding to `primary` at all, since closures are first-class
assignable values per §5c (`let f = { |x| x + 1 }` wasn't legal grammar before this fix). The
bareword-predicate sugar from §8b is unaffected — it desugars into `{ |it| ... }` under the hood,
so nobody has to type the pipes for that path by hand.

### 8d. String literals — no interpolation; `format()` instead

Rejected full string interpolation (`"hello ${name}"`) on two grounds: it forces the string
literal to become a recursive parse (an embedded expression that can itself contain a string) —
real lexer complexity for a hand-built interpreter with no compiler stage to lean on — and it
reopens the same hazard flagged for `expect`/`assert`: how much is legitimately allowed inside the
braces.

Also rejected Rust-style **named** capture (`format("hello {name}")` resolving `name` from the
calling scope). Rust gets away with this because `format!` is a **macro** — a compile-time
rewrite using the literal token already in the source, never touching a runtime environment table
by string key. Without a macro system, the same surface syntax as an ordinary function call would
require `format` to look up a *string* (`"name"`) against the caller's live scope at call time —
categorically different from every other name resolution in this language, all of which the
parser can see and check. It's a real refactoring trap: rename the variable, and the string
silently breaks with no error pointing at the right place, or worse, silently resolves against an
unrelated variable of the same name if one happens to be in scope.

**Settled: `format` is fully positional/indexed, nothing else.**

```
let world = "world"
format("hello {}", world)              # sequential
format("{1}, you are {0}", 30, name)   # explicit index — reorder or reuse an arg
format("{0} met {0} again", name)      # repeat one arg without repeating it in the call
```

- `{{` / `}}` escape to a literal brace, matching Rust's convention.
- Arity mismatch (too few args, or an index with no matching arg) is an error — same fail-loud
  default used throughout.
- `format` sits in the generic-value-operator category alongside `display` (§3) — it stringifies
  an arbitrary `Value` variant generically, same shape-genericity `sort`/`filter` have over table
  fields, just over `Value` variants instead.
- Deliberately distinct from `display`, not merged: `format` *produces* a `String` for further use
  (a message, a `save` path); `display` *consumes* a value and renders it to a display surface,
  ending the chain. Likely share an internal value-to-string primitive under the hood, but that's
  an implementation detail, not a language-level fold.
- `++` remains available for the trivial single-value case (`"hello " ++ world`) where a template
  is overkill — both exist, aimed at different points on the complexity curve.

### 8e. Comments and remaining literal forms

**Comments:** `#` to end of line — already the de facto convention in every code example in this
document, formalized here. No block-comment form; matches bash/nushell/Ruby, and a
pipeline-oriented language has little need for one.

**Numeric literals:**
```
int_lit   := DIGIT (DIGIT | "_")*
           | "0x" HEXDIGIT (HEXDIGIT | "_")*
           | "0b" BINDIGIT (BINDIGIT | "_")*
float_lit := DIGIT (DIGIT|"_")* "." DIGIT (DIGIT|"_")* (("e"|"E") ("+"|"-")? DIGIT+)?
```
Underscore separators (`1_000_000`) are a lexer-only nicety, stripped at lex time. Hex and binary
included deliberately — systems-adjacent shell, permissions/flags/addresses come up regularly. No
bare leading-zero octal (`010` silently meaning 8 is a real, well-known C/older-JS footgun) — an
explicit `0o` prefix would be added later if octal is ever wanted. No integer-width suffixes
(`i32`/`u8`): `Value::Int` is a single `i64`, nothing for a suffix to select between.

**List and record literals:**
```
list_lit     := "[" (expr ("," expr)* ","?)? "]"

record_lit   := "{" (record_field ("," record_field)* ","?)? "}"
record_field := IDENT (":" expr)?          # `{ name }` shorthand for `{ name: name }`
```
Record shorthand (`{ name, size }` for `{ name: name, size: size }`) is a deliberate parallel to
the destructuring shorthand used on the pattern side (§9f) — construction and destructuring share
one shorthand convention rather than two unrelated ideas.

## 9. Grammar: statement and control-flow layer

### 9a. Blocks are expressions

A block's value is the value of its last statement, if that statement is itself
expression-shaped. `let`, `return`, `for`, `while`, `def` don't produce values, so a block ending
in one of those evaluates to `Null` in expression position. This is what makes the
implicit-last-expression function-return rule (§5b) actually work when a function's last statement
is an `if`/`else` — `if`/`else` follows the same rule one level up: its value is whichever
branch's block ran.

No trailing-semicolon convention needed, unlike Rust — Rust needs it specifically because its
statements are semicolon-delimited, and dropping the `;` on a block's last line is what marks "this
is the value." This grammar is newline-delimited, not semicolon-delimited, so that mechanism has
no equivalent problem to solve: the last statement simply *is* the block's value if it's
expression-shaped, full stop.

`if` without `else`, used in expression position, evaluates to `Null` when the condition is false.
No separate static "is this in expression position" check — consistent with the dynamic-core,
check-at-point-of-use approach the type system already takes (see §9e for how ascription defuses
the resulting hazard).

### 9b. Condition/iterable grammar sits below `pipeline`

`if`'s condition and `for`'s iterable are grammatically `or_expr`, not full `pipeline` — one level
below `|` in the precedence table (§8a). This removes a real ambiguity structurally rather than by
convention: if a bare `pipeline` were allowed, `if ls | count > 0 { ... }` couldn't be parsed
unambiguously — is `{ ... }` the `if`-body, or a trailing closure argument being handed to
`count`? Restricting condition/iterable grammar to `or_expr` means a bare pipe simply cannot appear
at a condition's top level, so a pipeline-derived condition *must* be parenthesized — codifying the
convention already used by hand throughout this document: `if (files | count) > 0 { ... }`.

### 9c. Statement grammar (sketch)

```
statement   := let_stmt | mut_stmt | const_stmt | assign_stmt
             | if_stmt | for_stmt | while_stmt
             | def_stmt | try_stmt | strict_stmt
             | return_stmt | expr

block       := "{" statement* "}"          # value = last statement, if expression-shaped

let_stmt    := "let" IDENT (":" type_expr)? "=" expr             # immutable, file-private
mut_stmt    := "mut" IDENT (":" type_expr)? "=" expr             # mutable, file-private
const_stmt  := "pub"? "const" IDENT (":" type_expr)? "=" expr    # immutable, exportable

assign_stmt := lvalue "=" expr             # legal only if lvalue's root binding is `mut`
lvalue      := IDENT ("." IDENT | "[" expr "]")*

if_stmt     := "if" or_expr block ("else" (if_stmt | block))?
for_stmt    := "for" IDENT "in" or_expr block
while_stmt  := "while" or_expr block

def_stmt    := "pub"? "def" IDENT "(" param_list? ")" ("->" type_expr)? block
param_list  := param ("," param)* ("," "..." IDENT ":" type_expr)?
param       := IDENT (":" type_expr)? ("=" expr)?

try_stmt    := "try" block "catch" ("(" IDENT ")")? block
strict_stmt := "strict" block
return_stmt := "return" expr?

type_expr   := base_type "?"?
base_type   := IDENT ("<" type_expr ("," type_expr)* ">")?     # Table<...>, List<T>
             | "{" field ("," field)* "}"                      # record shape
field       := IDENT "?"? ":" type_expr
```

### 9d. Mutability: `let` / `mut` / `const`, and field/index mutation

**`let` is immutable, `mut` is mutable — nushell's actual concrete keywords, not Rust's `let mut`
compound.** Both stated inspirations (nushell, and Rust's own `let`/`let mut` default) land on
immutable-by-default independently, so this isn't a stylistic pick, it's the converged answer.
Deliberately not the Rust *spelling* though — same reasoning that kept `def` over `fn` (§9g):
`mut` as its own keyword doesn't oversell a Rust-adjacent reading experience the way a compound
`let mut` token would, since this remains a dynamically-typed, runtime-checked language.

**`const` is mechanically identical to `let`** — single evaluation, sequential (not hoisted), at
the point it's written — **and earns its keep entirely as a role marker, not a mutability
marker.** With `let` already immutable, a Rust/JS-style "compile-time-evaluable" restriction on
`const` would require inventing a constant-folding evaluator that doesn't otherwise exist anywhere
in this design (§6 already committed to no compiler, no JIT, no compile-time phase at all) — real
new machinery for a benefit that doesn't apply here. Instead, `const`/`def` are the two things a
file is allowed to export (`pub const PI: Float = 3.14159`, matching `pub def`); `let`/`mut` are
strictly local-binding forms, always file-private regardless of mutability. **`pub mut` is
illegal** — exporting live mutable shared state across a module boundary is exactly the
ambient-mutable-state shape this design has refused everywhere else it's come up (ambient env
vars, ambient `$?`, implicit closure capture).

**Field/index "mutation" is sugar for copy-on-write rebind, not real in-place mutation** — `Value`
stays persistent/immutable, on the planned representation for its collection variants
(`Table(Arc<Table>)`, `List(Arc<[Value]>)`, `Record(Arc<...>)`). These variants are an addition
the interpreter brings to the current scalar-only `libstream` `Value`, together with the wire
codecs for the reserved `List`/`Record` tags — see the v1.1 status note and the build plan. The
persistence property below is a property of that intended representation:

```
mut row = { name: "Alice", size: 10 }
row.size = 20              # sugar for: row = { ...row, size: 20 }

mut items = [1, 2, 3]
items[0] = 99               # sugar for: items = list_set(items, 0, 99)
items[0].name = "Bob"       # dot and index paths compose freely
```

Real interior mutability (`Arc<Mutex<...>>`) was deliberately rejected — it would reopen exactly
the aliasing hazard already ruled out for closures (§5a): `let b = a; b.size = 20` silently
changing `a` too, the mutable-default-argument trap wearing a different outfit. Under copy-on-write
sugar, `row` and anything else sharing its underlying `Arc<Table>` are unaffected by an assignment
through a *different* `mut` binding, since nothing is actually mutated — only `mut`-declared
bindings, never `let`, ever get repointed. `lvalue` mutation only legal when the *root* identifier
was declared `mut`; index out-of-range is a fail-loud error, consistent with everywhere else.

Nice validation this surfaces rather than a new decision: it's *why* capture-by-value closures
(§5a) are cheap and not just correct — "by value" is an `Arc` clone (O(1) refcount bump), not a
deep copy, and it's safe specifically because the underlying data can't change out from under a
closure except through an explicit rebind the closure never observes. The persistent
representation and by-value capture were quietly supporting each other before either was named
that way.

**Call-site rule for parameters:** any parameter may be passed positionally (in declared order) or
by name — not a separately declared "named-only" category, just how any parameter may be called.
Once a named argument appears in a call, everything after it must also be named (Python's rule) —
avoids ambiguity from interleaving positional and named arguments arbitrarily.

**Line structure:** newline terminates a statement; `;` allowed for multiple statements on one
line. A line beginning with `|` continues the previous line's pipeline — codifying the convention
already used by hand in every multi-line example in this document:

```
ls --long
  | sort size
  | take 5
```

### 9e. Null handling

Closes the hazard opened by if-without-else (§9a): a false condition with no `else` produces
`Null`, and without a defined nullable story that value could silently propagate.

**Unified rather than left as two unrelated-looking mechanisms.** "This field might not be
present" (`size?: Int` in a record shape) and "this value might be `Value::Null`" are the same
underlying concept — a present-but-absent column reads back as `Value::Null` either way.
`type_expr` gets a nullable suffix (already reflected in §9c's grammar): `Int?`, `Table<{...}>?`
are real standalone types, and `size?: Int` is sugar for `size: Int?`. Both spellings are kept
deliberately — `field?: T` reads better describing a schema, `T?` reads better when nullability is
about the value itself.

**Ascription does the actual defusing.** A non-nullable annotation rejects `Null` outright,
fail-loud, same as any other shape mismatch (§6):

```
let x: Int = if cond { 5 }         # cond false → Null → TypeError, caught immediately
let x: Int? = if cond { 5 }        # cond false → Null → fine, x is genuinely optional
```

If-without-else stays exactly as legal as decided in §9a — the mistake this catches is a single,
localized, fail-loud one (forgot to mark a binding nullable), not silent multi-step propagation.

**Two operators for working with a `T?`**, both established, boring precedent (C#/Kotlin/
Swift/JS), not new invention:

- **`?.`** — safe navigation. `row?.field` short-circuits to `Null` if `row` is `Null`; chains:
  `a?.b?.c`.
- **`??`** — null-coalescing. `x ?? default` evaluates to `x` unless `x` is `Null`, else `default`.

```
let name = row?.name ?? "unknown"
```

**Unifying note:** `?` now consistently means "handle the empty/absent case" everywhere it
appears — bare `?` for Result-propagation (§2), `T?`/`field?:` for nullable shape, `?.`/`??` for
consuming a nullable value. One glyph, one consistent idea, several positions.

### 9f. Pattern matching

`match` is an expression, following the same rule as `if`/`for` (§9a): the value is whichever
arm's body ran. Same ambiguity as §9b applies to the scrutinee too — `match ls | count { ... }`
would be as ambiguous as the `if`/`for` case — so the scrutinee is `or_expr`, not `pipeline`; a
piped scrutinee needs parens: `match (files | count) { ... }`.

```
match_expr  := "match" or_expr "{" match_arm+ "}"
match_arm   := pattern ("if" or_expr)? "=>" (expr | block)

pattern     := IDENT                                       # catch-all binding (lowercase)
             | UPPER_IDENT ("(" pattern ("," pattern)* ")")?  # variant test (capitalized)
             | "{" pat_field ("," pat_field)* "}"           # record pattern, subset-match
             | "_"                                          # wildcard
             | expr ".." "="? expr                          # range pattern
             | IDENT "@" pattern                            # capture-while-testing
             | pattern ("|" pattern)+                        # or-pattern
pat_field   := IDENT (":" pattern)?                          # `{ name }` shorthand
```

**Variant patterns reuse the exact type vocabulary from ascription (§6)** — `Int`, `String`,
`Table`, `Record`, etc. already mean something precise; patterns test against that same
vocabulary in a new grammatical position, not a separately-invented name for the same thing.

**Capitalized vs. lowercase resolves binding vs. type-test for free**, formalizing a convention
this document has used by hand throughout: type names are capitalized, variables/params are
lowercase. So `Capitalized(...)` tests a variant; a bare lowercase identifier is a catch-all
binding (matches anything, binds the whole value to that name).

**Record patterns get subset-match for free from §6, not a new default to decide** — a bare `{...}`
pattern tests "is this a `Record` with at least these fields," extra fields ignored, exactly like
ascription's default:
```
match row {
    { name, size } if size > 1000 => format("{} is large", name)
    { name }                      => format("{} (no size)", name)
    _                              => "unrecognized"
}
```

**Pattern-level `|` never collides with pipe-level `|`**, since `pattern` and `expr` are separate
grammar productions — `match_arm` calls into `pattern`, never into `expr`'s pipeline rule. Same
disambiguation-by-grammatical-context move as restricting `:` to fixed positions (§8a).

**Capture-while-testing**, e.g. `x @ Int(n) if n > 0 => ...` — binds the whole matched value to
`x` while also destructuring it.

**Range patterns** — plug directly into the range grammar added in §8c:
```
match n {
    0..10   => "small"
    10..100 => "medium"
    _       => "large"
}
```

**No static exhaustiveness checking** — same fail-loud default used throughout, applied at match
time rather than compile time (there's no compiler pass to enforce it even if wanted — §6 already
made this call for the type system generally). A value hitting no arm and no wildcard produces a
runtime `MatchError`, styled like the existing `TypeError` schema-diff messages, rather than
silently falling through.

**Composes with `try`/`catch` for free** — zero new grammar needed, the payoff of treating `match`
as an ordinary expression:
```
try {
    open ./data.csv | validate_schema
} catch (err) {
    match err {
        { kind: "NotFound" }           => print "file missing"
        { kind: "TypeError", message } => print(format("bad shape: {}", message))
        _                               => print "unknown error"
    }
}
```

### 9g. Function keyword: `def`, not `fn`

Confirmed `def` over `fn` deliberately, not by default. The Rust inspiration in this language has
been about *mechanics* (`?`-propagation, capture-by-value closures, `Result`-shaped errors), not
surface branding. `fn` visually promises a Rust-adjacent reading experience this language can't
actually deliver — it's dynamically typed, fully interpreted, runtime-checked. `def` matches the
languages this one actually resembles when read top to bottom (Python, Ruby, nushell) and avoids a
reader assuming static/compiled semantics the syntax doesn't have.

### 9h. Modules and imports

Rejected ambient path-search resolution (a `PATH`-like or `node_modules`-like search list that
`use "utils"` silently walks to find a file) — the same shape of problem as every other
boundary-crossing decision this session, and every one of those has come down on the side of
explicit: ambient env vars, ambient `$?`, implicit closure capture, ambient keyword-collision
resolution. A module import crossing a file boundary gets the same treatment.

```
use "./lib/utils.nx" { helper, other_fn }
use "./lib/utils.nx" as utils              # utils.helper(...)

pub def helper(x: Int) -> Int { x + 1 }    # exported
def internal_only() { ... }                # file-private by default
```

Explicit relative/absolute path only, no search algorithm. A bare `use "path"` with no selector
isn't legal — either name exactly what's being imported, or bind the whole module to a namespace.
No wildcard "dump everything into scope" form, for the same reason record patterns don't silently
ignore fields you didn't ask about (§9f).

**`pub` for exports, not everything-public-by-default** — the one place reusing a real Rust
keyword is justified on principle rather than convenience (contrast the `def`-not-`fn` call in
§9g): the underlying idea — a name crossing a file boundary should be visible at its declaration,
not implicit — is the same recurring rule as env vars crossing a process boundary or data crossing
IPC, not a branding echo.

**Deliberately left open, not decided here:**
- Circular imports — needs a rule, but it's closer to an interpreter/loader implementation concern
  than a grammar one.
- The `.nx` file extension used above is a placeholder, not a real decision.
- This is deliberately the small, file-level answer only. A real package system (versioned,
  shared, content-addressed) would naturally build on the NixOS/Guix-store influence already
  referenced elsewhere in the OS design — but that's a system-level concern bigger than shell
  grammar, not something to decide as a side effect of this pass.

## 10. Coreutils scope

### 10a. Sorting the classic Unix set

The dividing rule that resolves every case cleanly, rather than needing a judgment call per tool:
**does it define its own schema from a live external source, or actually mutate something outside
the shell's own data flow?** If not, it's not a separate program — it's a gap in the existing
generic-operator set, and belongs there instead.

**Dissolves into an existing or near-trivial generic operator — no separate program:**

| Classic tool | Dissolves into |
|---|---|
| `cat` | `open`'s multi-path form (§4) |
| `find` | `list --recursive` gaining a filter, i.e. `list --recursive \| filter ...` |
| `grep` | `filter` + a new `~=` regex-match operator (§10b) |
| `tail` / `head -n -N` | new `last`/`skip` generic operators (§10b) |
| `uniq` | new `dedupe` generic operator (§10b) — same generic-row-equality shape as `sort`/`filter` |

**Deliberately not scoped now, flagged rather than answered:**
- **Process management (`ps`, `kill`)** — `kill` specifically doesn't map onto the system's
  no-signals design. Terminating an arbitrary process needs a capability-gated handle to it, which
  raises a real open question (how does a shell command legitimately *acquire* a handle to a
  process it didn't spawn?) deserving its own design pass, not a rushed answer here.
- **Networking tools** — excluded entirely; the network stack itself is already marked deferred in
  the OS design doc, so tooling for it is premature.
- **User-definable aliases with baked-in default arguments** (`alias ll = 'list --long'`) — a
  genuinely different feature from the name-only aliasing in §10e, would need its own design (does
  it interact with `^`, §8a? is it `def`-level or its own construct?). Flagged, not solved here.

### 10b. New generic-operator additions

- **`last N` / `skip N`** — tail/head-from-end and offset-skip, siblings to the existing `take`.
- **`dedupe`** — row-equality-based deduplication, same generic-dispatch shape as `sort`/`filter`
  (§5c) — no schema of its own, just generic access to whatever rows show up.
- **`~=`** — regex match operator, added to the precedence table (§8a) at the equality tier
  alongside `==`/`!=`. `ls | filter name ~= /\.rs$/` covers `grep`'s job entirely — the gap was a
  missing *operator*, not a missing *program*.

### 10c. Final external program scope and naming

```
list, copy, move, remove, mkdir, touch, rename
date, sleep, whoami
```

**Naming departs from GNU/Unix names deliberately, following PowerShell's actual convention where
it applies rather than either camp uninspected:**
- **`list`**, not `ls` — initially looked like the odd one out for having no explicit object
  (`copy`/`move`/`remove` all read fine standing alone because the path argument supplies the
  object), but on reflection this isn't unusual in a shell where "the current thing being operated
  on" is already an established idiom — `sort`/`filter`/`count` (§3) already read fine standing
  alone for the same reason. Kept as `list`.
- **`copy`, `move`** — unchanged, already read correctly as bare transitive verbs.
- **`remove`, not `delete`** — matches PowerShell's actual `Remove-Item` (not `Delete-Item`),
  precedent rather than a coin flip.
- **`mkdir`** — kept verbatim. Not an arbitrary Unix abbreviation the way `ls`/`cp`/`mv`/`rm` are;
  "make dir" reads directly off the letters, nothing to fix.
- **`touch`** — kept verbatim, deliberately not renamed to `create`/`mkfile`. Its real contract is
  "ensure this file exists and its modification time is now," covering both empty-file creation
  *and* updating mtime on an existing file — load-bearing for build systems that lean on exactly
  this to force a rebuild. `create` would have to either drop that behavior or misdescribe it.
- **`rename`** — new addition (PowerShell's `Rename-Item` is a real, separate cmdlet from
  `Move-Item`, not folded together). Given a genuinely distinct contract, not just a friendlier
  label for `move`: **accepts a bare target name only, no path separator; operates strictly within
  the source's existing directory; a path in the target is a fail-loud error.**

### 10d. Requirements (behavioral scope only — implementation deferred to build time)

| Program | Operates on | Requirements |
|---|---|---|
| `list` | directory (default: cwd) | Emits `Table<{name, size, kind, modified}>`, one row per entry. `--recursive` walks subdirectories (absorbs `find`, §10a). |
| `copy` | source path(s) → dest | Duplicates file or directory; recursive by nature for directories (no separate recursive flag needed the way `remove` requires one — copying a directory has no destructive-by-default hazard). Fail loud if destination exists, unless `--force`. |
| `move` | source path(s) → dest | Relocates within or across directories. Destination-is-a-directory moves *into* it; destination-is-a-name renames as a side effect — distinct from `rename`'s stricter single-directory-only contract. |
| `remove` | path(s) | Deletes. **Requires `--recursive` for non-empty directories** — refusing by default is a deliberate safety rail, not an oversight, consistent with fail-loud elsewhere. |
| `mkdir` | path | Creates a directory. `--parents` creates intermediate directories. |
| `touch` | path | Creates an empty file if absent; updates modified-time if present (§10c). |
| `rename` | path, bare new name | Renames within the source's existing directory only; a path separator in the target argument is a fail-loud error (§10c). |
| `date` | — (optional format spec) | Emits current date/time as a structured value. |
| `sleep` | duration | Suspends the calling pipeline stage for the given duration; not schema-producing. |
| `whoami` | — | Emits current user identity as a value. |

### 10e. Aliasing — namespace binding, not a new shell feature

`ls`/`cp`/`mv`/`rm` as familiar short names for `list`/`copy`/`move`/`remove` don't need a shell
aliasing mechanism at all — an alias here is just "one program, multiple names," which is exactly
what Plan 9-style namespace binding already provides as a first-class OS primitive (see companion
doc, §2). Ship the friendly names as the real programs; the Unix-familiar short names are namespace
binds pointing at the same underlying program. The "alias" is namespace setup data, not new
grammar or new shell machinery. (Baked-in-argument user aliases are a different, deferred feature —
§10a.)

### 10f. Flag conventions

Adopts actual GNU conventions as baseline, not just the general shape: long-form `--flag`, short
`-f`, `--` to end option parsing, `--help`/`--version` on every program. One deliberate deviation,
flagged so it isn't mistaken for an oversight: GNU's bare `-` argument meaning "read from stdin"
(`cat -`) has no equivalent here — piping is structural in this design (a stage's input *is* its
stdin stream, not a flag-selected mode), so that specific idiom doesn't carry over.

## 11. REPL / interactive shell behavior

### 11a. Prompt

Shows the current namespace position (the `cd`-tracked equivalent of cwd) plus a status glyph
reflecting the last pipeline's `PipelineStatus` (§1) — distinguishing "last thing failed
outright" from "last thing partially failed" from "clean" in the prompt itself, a direct payoff
of having a real composite status object rather than a bash-style scalar.

### 11b. Multi-line continuation

**Genuinely REPL-specific, not a grammar problem** — a whole script file parses multi-line
pipelines (leading-`|` style included) completely unambiguously, since the parser sees the entire
file at once. The ambiguity only exists in the REPL's line-by-line, decide-after-each-Enter model.

**Automatic continuation** — exact, not heuristic, covers states the parser can prove are
incomplete:
- **Unclosed brace/paren/bracket.**
- **A trailing `|` at end of line** — real, established precedent (bash's own `PS2` continuation
  triggers on a trailing `|`/`&&`/`\` for the same reason: the parser is demonstrably mid-way
  through `pipeline_stage`, not a guess).

**Shift-Enter — manual override for everything else**, including the genuinely ambiguous case: a
line ending in a complete, valid, executable statement, where the *next* line would begin with
`|` (leading-pipe style). That specific case has a real one-bit ambiguity no grammar refinement
can resolve — only the person typing knows whether they're done. Owning the terminal stack
end-to-end (local console driver and compositor terminal window, not constrained by a third-party
VT100/terminfo-limited emulator) makes a genuinely distinct Shift-Enter sequence a real, reliable
design choice here rather than a hopeful one. Leading-pipe style stays fully valid to write — used
throughout this document — it just needs the explicit keystroke interactively; a script file never
faces this regardless of which style it uses.

### 11c. History and completion

Persistent history across sessions, up/down navigation, incremental reverse-search
(`Ctrl-R`-equivalent). Tab completion baseline: command names across all four categories (§3),
file paths, and flag names where a program's flags are introspectable.

**Flagged as a future nice-to-have, not scoped now:** schema-aware field completion (`filter
siz<TAB>` → `size`, using an upstream pipeline's known `Table` shape). Plausible since shape is
often statically knowable from ascriptions, but needs real design — does it require speculatively
executing upstream stages, or only work when ascription made the shape explicit — not solved as a
side effect here.

### 11d. `$last` — resolves an ambiguity left open in §2

§2 said pipelines evaluate to "a value" and separately that `PipelineStatus` is "the real answer"
to status, without nailing down whether those are the same thing. They aren't, and shouldn't be
conflated: `let x = pipeline` binds the actual **data** value — what feeds into the next
function — while `PipelineStatus` is orthogonal execution metadata. The REPL convenience binding
(§2's `$last`, name now settled) is a small `Record` holding both:
```
$last: { value: Value, status: PipelineStatus }
```
`$last.value` for "what I just computed," `$last.status` for "did that succeed." Doesn't touch
`let`/`mut`/`const` semantics for real code at all — those still bind only the data value, exactly
as already settled (§9d). Purely REPL bookkeeping, scoped only to the interactive session per §2's
original rule.

### 11e. Top-level auto-display — deliberately different defaults in REPL vs. script

**REPL:** a top-level pipeline that isn't assigned (`let`/`mut`/`const`) and doesn't already end in
a terminal generic operator implicitly gets `| display` appended — standard REPL ergonomics
(Python, nushell, PowerShell all do some version of this).

**Script:** the opposite default — an unassigned bare expression statement evaluates and its
result is silently discarded, not auto-displayed. A script with many bare invocations for side
effects (`remove`, `move`) would otherwise flood output unpredictably. Consistent with the
fail-loud-but-explicit posture used throughout: a script that wants output says so (`display`)
rather than getting it as an ambient default.

### 11f. Exit behavior

`exit` is already a shell-state builtin (§3) — nothing new needed for the mechanism, just its REPL
usage: `Ctrl-D` at an empty prompt is treated identically to typing `exit` (universal convention).
With running background jobs (§11g), warn once rather than block — matches actual bash/zsh
behavior, and avoids silently discarding state without being obstructive about it.

### 11g. Job control — included, minus the one piece that doesn't fit this system

`&` suffix to background a pipeline (`long_task | save ./out.tsm &`), `jobs` to list running
background pipelines, `fg <id>` to bring one to the foreground. All three map onto primitives
already established: the shell already holds process handles for everything it spawns (§1);
backgrounding is just "don't await this pipeline's `PipelineStatus` before returning to the
prompt"; `fg` is "start awaiting it now."

**Deliberately excluded: `Ctrl-Z`-style suspend/resume (`bg`, job stopping).** Not a gap — a
structural mismatch. Suspending a running process relies on signal delivery (`SIGTSTP`) in every
shell that has it, and this system has no signals by design. A job here is either running or
finished; there's no "paused" state to reach for, and inventing one just to complete the bash
feature set would cut against the no-signals commitment for no real benefit — bash's own suspend
often pauses execution at a moment the user didn't actually intend or expect anyway.

## 12. Open questions carried forward

- Schema-aware tab completion (§11c) — flagged as a future nice-to-have, not designed.
- Process management / how a shell command legitimately acquires a capability handle to a process
  it didn't spawn (§10a) — needs its own design pass.
- User-definable aliases with baked-in default arguments (§10a) — deferred, distinct from the
  name-only aliasing already settled in §10e.
- Circular import resolution (§9h) — implementation-level, not yet decided.
- Package system beyond single-file `use` (§9h) — explicitly deferred, larger than shell grammar.
- From the earlier session: does "windows/widgets as namespace-resident resources" (see companion
  doc) overload namespace semantics past what they're meant for? Still flagged, not resolved.
