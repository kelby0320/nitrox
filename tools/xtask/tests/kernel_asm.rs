use std::path::{Path, PathBuf};

/// The `asm!` invocations in `text`, each from its `asm!(` to the parenthesis closing it, with the
/// line it starts on. String literals are skipped over, so a parenthesis inside one is not counted.
fn asm_blocks(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("asm!(").map(|i| from + i) {
        let open = at + "asm!".len();
        let (mut depth, mut in_str, mut escaped, mut end) = (0, false, false, text.len());
        for (i, c) in text[open..].char_indices() {
            match (in_str, c) {
                (true, _) if escaped => escaped = false,
                (true, '\\') => escaped = true,
                (true, '"') => in_str = false,
                (true, _) => {}
                (false, '"') => in_str = true,
                (false, '(') => depth += 1,
                (false, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push((text[..at].matches('\n').count() + 1, &text[at..end]));
        from = end;
    }
    out
}

/// Whether one of `block`'s string literals holds one of `words` as a whole word.
fn template_has(block: &str, words: &[&str]) -> bool {
    block
        .split('"')
        .skip(1)
        .step_by(2)
        .any(|lit| lit.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| words.contains(&w)))
}

fn arch_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernel/src/arch/x86_64");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    files
}

/// **An instruction that masks, unmasks or waits for interrupts is a compiler barrier**: with
/// `nomem` the compiler may move a memory access across it, out of an interrupt-masked section,
/// or keep a value read before a `hlt` that an interrupt handler changed.
#[test]
fn no_cli_sti_or_hlt_is_marked_nomem() {
    let mut found = 0;
    for path in arch_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line, block) in asm_blocks(&text) {
            if template_has(block, &["cli", "sti", "hlt"]) {
                found += 1;
                assert!(!block.contains("nomem"), "{}:{line}: {block}", path.display());
            }
        }
    }
    assert!(found >= 5, "found {found}: the scan no longer sees regs.rs's cli and sti and cpu.rs's halts");
}

#[test]
fn the_asm_scan_finds_each_block_whole_and_reads_only_its_templates() {
    let text = "a\nasm!(\"cli\", options(nomem));\nlet x = asm!(\"mov {}, rsp\", out(reg) y, options(nostack)) + 1;";
    let blocks = asm_blocks(text);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0], (2, "asm!(\"cli\", options(nomem))"));
    assert_eq!(blocks[1].0, 3);
    assert!(blocks[1].1.ends_with("options(nostack))"));
    assert!(template_has(blocks[0].1, &["cli"]));
    assert!(!template_has("asm!(\"clid\", cli = in(reg) x)", &["cli"]), "a longer word, or an operand name");
    assert!(template_has("asm!(\"sti; hlt\")", &["hlt"]));
    assert_eq!(asm_blocks("asm!(\"(\", x)").len(), 1, "a parenthesis in a string");
}

fn arch_file(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernel/src/arch/x86_64").join(name))
        .unwrap()
}

/// **An IPI goes out after the stores it announces are visible**: the x2APIC ICR is written in
/// one place, `write_icr`, after a fence that is `mfence; lfence` and a compiler barrier.
#[test]
fn the_icr_is_written_once_after_the_x2apic_fence() {
    let apic = arch_file("apic.rs");
    let writes: Vec<_> = apic.match_indices("wrmsr(MSR_X2APIC_ICR").map(|(i, _)| i).collect();
    assert_eq!(writes.len(), 1, "the ICR written outside write_icr");
    let start = apic.find("unsafe fn write_icr(").expect("write_icr");
    assert!(start < writes[0] && apic[start..writes[0]].contains("regs::x2apic_write_fence();"));
    let others = arch_files().into_iter().filter(|p| !p.ends_with("apic.rs"));
    assert!(others.map(|p| std::fs::read_to_string(p).unwrap()).all(|t| !t.contains("0x830")), "the ICR by number");
    let regs = arch_file("regs.rs");
    let fence = &regs[regs.find("pub fn x2apic_write_fence()").expect("the fence")..];
    let (_, block) = asm_blocks(fence)[0];
    assert!(template_has(block, &["mfence"]) && template_has(block, &["lfence"]) && !block.contains("nomem"));
}
