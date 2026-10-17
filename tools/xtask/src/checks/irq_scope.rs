//! `check-irq-scope`: every interrupt entry stub opens the lock-ordering scope.

use crate::*;

/// How `irq_dispatcher!` binds the guard [`enter_interrupt`] returns.
///
/// *When the guard drops* decides whether the handler body runs inside the scope or after it,
/// and the three forms below are indistinguishable to a check that only asks whether the text
/// `enter_interrupt` appears.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScopeBinding {
    /// `let name = …enter_interrupt();` — lives to the end of the block. Correct.
    Held,
    /// `let _ = …enter_interrupt();` — dropped immediately. `#[must_use]` does not fire,
    /// because `let _ =` is the sanctioned way to silence it.
    Discarded,
    /// `…enter_interrupt();` — a temporary, dropped at the end of the statement.
    Temporary,
}

pub(crate) fn scope_binding(line: &str) -> ScopeBinding {
    let t = line.trim();
    let Some(rest) = t.strip_prefix("let ") else {
        return ScopeBinding::Temporary;
    };
    let Some((binding, _)) = rest.split_once('=') else {
        return ScopeBinding::Temporary;
    };
    // `let mut g: Guard = …` → `g`.
    let binding = binding.trim().trim_start_matches("mut ").trim();
    let binding = binding.split(':').next().unwrap_or("").trim();
    if binding == "_" { ScopeBinding::Discarded } else { ScopeBinding::Held }
}

/// How many naked entry stubs `check-irq-scope` expects to find.
///
/// A count, because a stub can leave the scan's view without leaving the kernel — by being
/// renamed, or moved out of `kernel/src/arch`. The emptiness guard below only fires at *zero*.
/// Today: 6 in `idt.rs` (exception, page-fault, timer, TLB shootdown, reschedule IPI, device
/// IRQ) + `syscall_entry`.
const EXPECTED_ENTRY_STUBS: usize = 7;

/// `<name> = sym TARGET` operands that are deliberately **not** interrupt dispatchers.
///
/// Keyed on the *target*, not the operand name: allowlisting by operand name would let a
/// rename buy the exemption this exists to deny.
const SYM_OPERAND_ALLOWLIST: &[&str] = &[
    // The thread trampoline: reached by a context switch, not by an interrupt, so the
    // rank order continues rather than restarting.
    "crate::sched::thread_enter",
];

/// `cargo xtask check-irq-scope` — every interrupt entry opens a lock-ordering scope.
///
/// The kernel's lock-rank tracker (`kernel/src/libkern/lockrank.rs`) has each interrupt handler
/// start from an empty view of the held-rank stack, because the acquisition order genuinely
/// restarts at an interrupt boundary — a tick that lands on a thread holding an allocator lock
/// and takes `SCHED` is correct, and reads as an inversion otherwise.
///
/// A dispatcher that forgets the scope does not fail loudly: it reports phantom inversions from
/// its own vector, days later. So the scope is not left to the author of the next dispatcher:
///
/// 1. every `dispatch = sym NAME` in a naked entry stub must name a function defined by the
///    `irq_dispatcher!` macro, which opens the scope itself; and
/// 2. that macro must in fact still call `enter_interrupt` — otherwise rule 1 checks that
///    everyone went through a door that no longer leads anywhere.
///
/// Arch-generic: it walks `kernel/src/arch`, so an aarch64 entry path is covered the day it exists.
///
/// ## What it cannot see (the ESCAPES boundary)
///
/// This is a textual scan, so its coverage is a convention rather than a proof. Three escapes
/// are closed by construction — a guard bound to `_` or left as a temporary ([`ScopeBinding`]),
/// a stub whose operand is renamed ([`SYM_OPERAND_ALLOWLIST`]), and a stub that leaves the
/// scan's view entirely ([`EXPECTED_ENTRY_STUBS`]). Two are **not** closed, and need an entry
/// stub in a deliberately unusual shape:
///
/// - A macro that puts the handler body *above* the `let` binding classifies as held and
///   passes. The classifier reads the binding form, not the ordering.
/// - The count notices a stub going missing, not a new one arriving in a form the scan does
///   not parse — a positional `sym` operand, say.
pub(crate) fn cmd_check_irq_scope() -> R<()> {
    let arch_dir = repo_root().join("kernel").join("src").join("arch");
    // Dispatchers named by a naked stub, as (file:line, file, name).
    let mut called: Vec<(String, String, String)> = Vec::new();
    // Dispatchers the macro generated, as (file, name).
    // **Per file, for the same reason as `user_entry` below.** Matched by name alone, a second
    // architecture reusing an obvious name — `timer_dispatch`, `exception_dispatch` — would have
    // its *unscoped* dispatcher exempted because x86_64 happens to generate one with that name.
    let mut generated: Vec<(String, String)> = Vec::new();
    // Dispatchers that are not interrupts and take the ring-3 entry discipline instead
    // (see `assert_user_entry_safe`): the order begins there rather than restarting.
    let mut user_entry: Vec<(String, String)> = Vec::new();
    // `<name> = sym …` operands that are not the `dispatch` this check keys on; without this a
    // stub leaves the check's view by being renamed.
    let mut stray_sym: Vec<(String, String)> = Vec::new();
    // Files that define the macro, and whether the definition still opens a scope.
    let mut macro_defs: Vec<(String, Option<ScopeBinding>)> = Vec::new();

    visit_rs_files(&arch_dir, &mut |path| {
        let text = fs::read_to_string(path)?;
        let lines: Vec<&str> = text.lines().collect();
        // `irq_dispatcher! {` … `fn NAME(` — the docs sit between the two.
        let mut pending_macro_body = false;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if let Some(rest) = line.split("dispatch = sym").nth(1) {
                let name: String = rest.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                if !name.is_empty() {
                    called.push((format!("{}:{}", path.display(), i + 1), path.display().to_string(), name));
                }
            }

            // Any *other* `<ident> = sym` operand. Allowlisted by target, not by operand name, so
            // renaming an operand cannot buy an exemption. Comment lines are skipped, as in
            // `assert_user_entry_safe` below: a doc comment quoting an operand is prose.
            // Whitespace is normalised first so `dispatch=sym` cannot slip past the convention.
            let normalised = line.replace("=sym", "= sym");
            if !trimmed.starts_with("//")
                && !trimmed.starts_with("*")
                && let Some(before) = normalised.split("= sym").next()
                && normalised.contains("= sym")
                && !normalised.contains("dispatch = sym")
            {
                let operand: String = before
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let target: String = normalised
                    .split("= sym")
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
                    .collect();
                if !operand.is_empty() && !target.is_empty() && !SYM_OPERAND_ALLOWLIST.contains(&target.as_str()) {
                    stray_sym.push((format!("{}:{}", path.display(), i + 1), format!("{operand} = sym {target}")));
                }
            }

            if trimmed.starts_with("irq_dispatcher!") {
                pending_macro_body = true;
            } else if pending_macro_body && trimmed.starts_with("fn ") {
                let name: String =
                    trimmed[3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                generated.push((path.display().to_string(), name));
                pending_macro_body = false;
            }

            // A plain `fn NAME(` whose body asserts the ring-3 entry invariant. Scanned to
            // the next column-0 `}`, which ends a top-level function.
            if let Some(after) = trimmed.strip_prefix("fn ").or_else(|| {
                trimmed.strip_prefix("unsafe extern \"C\" fn ").or_else(|| trimmed.strip_prefix("extern \"C\" fn "))
            }) {
                let name: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                let body_end = lines[i..].iter().position(|l| *l == "}").map(|o| i + o).unwrap_or(lines.len());
                // **Code only, and this file only.** Matched textually anywhere in the
                // function it would be satisfied by a doc comment that merely mentions the
                // assertion; pooled across files it would exempt a dispatcher because some
                // *other* file has a same-named function that asserts.
                if lines[i..body_end].iter().any(|l| {
                    let t = l.trim();
                    !t.starts_with("//") && !t.starts_with("*") && l.contains("assert_user_entry_safe")
                }) {
                    user_entry.push((path.display().to_string(), name));
                }
            }

            if trimmed.starts_with("macro_rules! irq_dispatcher") {
                let end = (i + 12).min(lines.len());
                let scope_line = lines[i..end].iter().find(|l| l.contains("enter_interrupt"));
                macro_defs.push((format!("{}:{}", path.display(), i + 1), scope_line.map(|l| scope_binding(l))));
            }
        }
        Ok(())
    })?;

    let mut violations: Vec<String> = Vec::new();

    // A check that silently finds nothing to check reads as a pass.
    if called.is_empty() {
        violations.push(
            "found no `dispatch = sym …` entry stubs under kernel/src/arch — either the \
             entry path was restructured (update this check) or the scan is broken"
                .to_string(),
        );
    }
    if macro_defs.is_empty() {
        violations.push(
            "found no `macro_rules! irq_dispatcher` definition — the macro every dispatcher \
             is required to go through does not exist"
                .to_string(),
        );
    }
    for (site, binding) in &macro_defs {
        match binding {
            None => violations.push(format!(
                "{site}: `irq_dispatcher!` no longer calls `enter_interrupt` — dispatchers \
                 go through it precisely so they get a lock-ordering scope"
            )),
            Some(ScopeBinding::Discarded) => violations.push(format!(
                "{site}: `irq_dispatcher!` binds the interrupt scope to `_`, which drops it \
                 *before* the handler body runs — the body is then checked against the \
                 interrupted context, which is the flat-tracker behaviour that got the \
                 first tracker withdrawn (decision log 2026-07-29). Bind it to a name"
            )),
            Some(ScopeBinding::Temporary) => violations.push(format!(
                "{site}: `irq_dispatcher!` calls `enter_interrupt()` without binding the \
                 guard, so it is dropped at the end of that statement and the handler body \
                 runs unscoped. Bind it to a name"
            )),
            Some(ScopeBinding::Held) => {}
        }
    }
    if called.len() != EXPECTED_ENTRY_STUBS {
        violations.push(format!(
            "found {} entry stub(s), expected {EXPECTED_ENTRY_STUBS}. A stub that leaves \
             this check's view does so silently — the count is the only thing that notices. \
             If the entry path genuinely gained or lost one, update EXPECTED_ENTRY_STUBS \
             deliberately",
            called.len()
        ));
    }
    for (site, operand) in &stray_sym {
        violations.push(format!(
            "{site}: `{operand}` — an entry stub names its dispatcher `dispatch = sym`, and \
             this check keys on that literal. A differently-named operand is invisible to \
             it, so either rename the operand to `dispatch` or add the target to \
             SYM_OPERAND_ALLOWLIST with a reason"
        ));
    }
    for (site, file, name) in &called {
        if !generated.iter().any(|(f, g)| f == file && g == name)
            && !user_entry.iter().any(|(f, u)| f == file && u == name)
        {
            violations.push(format!(
                "{site}: entry stub dispatches to `{name}`, which is neither defined by \
                 `irq_dispatcher!` nor asserts `assert_user_entry_safe()` — it would run \
                 without a lock-ordering scope"
            ));
        }
    }

    if violations.is_empty() {
        println!(
            "check-irq-scope: {} entry stub(s) → {} scoped dispatcher(s) + {} ring-3 \
             entry point(s) ✓",
            called.len(),
            generated.len(),
            user_entry.len()
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "every interrupt entry must open a lock-ordering scope — define the dispatcher \
             with the `irq_dispatcher!` macro (see kernel/src/libkern/lockrank.rs \
             § Interrupt scopes):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}
