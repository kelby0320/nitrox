use crate::*;

/// A function symbol from an ELF's symbol table.
pub(crate) struct ElfSym {
    pub(crate) addr: u64,
    pub(crate) size: u64,
    pub(crate) name: String,
}

/// Read the `STT_FUNC` symbols out of an ELF64 file, sorted by address.
///
/// Hand-rolled rather than shelling out to `nm`/`addr2line`: those are another tool
/// requirement on every machine that runs the tests, for sixty lines of well-specified
/// header walking.
pub(crate) fn elf_function_symbols(path: &Path) -> R<Vec<ElfSym>> {
    let b = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let u16at = |o: usize| -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) };
    let u32at = |o: usize| -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) };
    let u64at = |o: usize| -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(v)
    };
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 {
        return Err(format!("{} is not an ELF64", path.display()).into());
    }
    let shoff = u64at(0x28) as usize;
    let shentsize = u16at(0x3A) as usize;
    let shnum = u16at(0x3C) as usize;
    let mut syms = Vec::new();
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if sh + shentsize > b.len() {
            break;
        }
        const SHT_SYMTAB: u32 = 2;
        if u32at(sh + 4) != SHT_SYMTAB {
            continue;
        }
        let off = u64at(sh + 0x18) as usize;
        let size = u64at(sh + 0x20) as usize;
        let link = u32at(sh + 0x28) as usize; // the associated string table
        let strsh = shoff + link * shentsize;
        let stroff = u64at(strsh + 0x18) as usize;
        const SYM_SIZE: usize = 24;
        for j in 0..size / SYM_SIZE {
            let e = off + j * SYM_SIZE;
            if e + SYM_SIZE > b.len() {
                break;
            }
            const STT_FUNC: u8 = 2;
            if b[e + 4] & 0xF != STT_FUNC {
                continue;
            }
            let addr = u64at(e + 8);
            let sz = u64at(e + 16);
            if addr == 0 {
                continue;
            }
            let nameoff = stroff + u32at(e) as usize;
            let end = b[nameoff..].iter().position(|&c| c == 0).unwrap_or(0) + nameoff;
            let name = String::from_utf8_lossy(&b[nameoff..end]).into_owned();
            syms.push(ElfSym { addr, size: sz, name });
        }
    }
    syms.sort_by_key(|s| s.addr);
    Ok(syms)
}

/// Undo rustc's legacy `_ZN…E` name mangling, enough to read a backtrace.
///
/// `_ZN13nitrox_kernel5sched9idle_body17h08d8…E` → `nitrox_kernel::sched::idle_body`. Not a
/// general Itanium demangler — just the length-prefixed component form rustc emits, with the
/// trailing hash and any LLVM suffix dropped. Unrecognised names come back unchanged, which
/// is strictly better than the alternative of not printing them.
pub(crate) fn demangle(name: &str) -> String {
    let Some(rest) = name.strip_prefix("_ZN") else { return name.to_string() };
    let rest = rest.split(".llvm.").next().unwrap_or(rest);
    let mut parts: Vec<String> = Vec::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'E' {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return name.to_string(); // not the shape we expect
        }
        let Ok(len) = rest[start..i].parse::<usize>() else { return name.to_string() };
        if i + len > bytes.len() {
            return name.to_string();
        }
        let part = &rest[i..i + len];
        i += len;
        // The final `17h<16 hex>` component is the disambiguating hash, not a path element.
        if !(part.len() == 17 && part.starts_with('h') && part[1..].bytes().all(|c| c.is_ascii_hexdigit())) {
            parts.push(part.to_string());
        }
    }
    if parts.is_empty() { name.to_string() } else { parts.join("::") }
}

/// Name the function containing `addr`, if any.
///
/// Several symbols can share one address — aliased generic instantiations, say — and this
/// returns an arbitrary one of the group. Read a resolved name as "this address is in this
/// function's code", not as a unique identification.
pub(crate) fn resolve_symbol(syms: &[ElfSym], addr: u64) -> Option<String> {
    let i = syms.partition_point(|s| s.addr <= addr).checked_sub(1)?;
    let s = &syms[i];
    // A zero-size symbol still names its entry point; otherwise require containment so a
    // wildly wrong address is reported as unknown rather than attributed to the last
    // function before it.
    if s.size == 0 || addr < s.addr + s.size {
        Some(format!("{}+{:#x}", demangle(&s.name), addr - s.addr))
    } else {
        None
    }
}

/// Walk a stack, printing every word that resolves to a kernel function.
///
/// A real unwinder needs frame pointers or DWARF; this is the cheap approximation that
/// works on an optimised kernel: read raw stack words and keep the ones that land inside a
/// known function. It over-reports (stale return addresses linger below the stack pointer)
/// and that is fine — the question it answers is *"is this CPU inside this function
/// twice?"*, which a few spurious frames cannot fake.
fn stack_trace(qmp: &mut Qmp, syms: &[ElfSym], cpu: usize, rsp: u64, depth: usize) {
    if syms.is_empty() || rsp == 0 {
        return;
    }
    if qmp.hmp(&format!("cpu {cpu}")).is_err() {
        return;
    }
    let Ok(text) = qmp.hmp(&format!("x/{depth}gx {rsp:#x}")) else { return };
    let mut shown = 0usize;
    for line in text.lines() {
        // `addr: 0xword 0xword`
        let Some((_, words)) = line.split_once(':') else { continue };
        for w in words.split_whitespace() {
            let Some(hex) = w.strip_prefix("0x") else { continue };
            let Ok(v) = u64::from_str_radix(hex, 16) else { continue };
            if let Some(sym) = resolve_symbol(syms, v) {
                println!("      ↳ {sym}");
                shown += 1;
                if shown >= 12 {
                    return;
                }
            }
        }
    }
}

/// Dump what the guest was doing when it stopped answering.
///
/// The discriminator this exists for: **are all vCPUs halted, or is one spinning?** Every
/// CPU parked in the idle loop means nothing is runnable — a lost wake, where somebody is
/// blocked on a completion that was signalled to no one. A CPU spinning in a lock is the
/// opposite problem. A bare 90-second timeout cannot tell those apart, and the whole cost
/// of chasing the `test-qemu` flake so far has been that ambiguity.
pub(crate) fn dump_guest_state(qmp: &mut Qmp) {
    println!("\n─── guest state at timeout ───");
    // **Stop the guest first.** Without this the vCPUs keep executing between queries, so
    // the RIP summary, the stack walk and the full register block are three snapshots of
    // three different moments — and they visibly disagreed when this was first written.
    // The run is over either way; a stopped guest is the only coherent one.
    if let Err(e) = qmp.execute(r#"{"execute":"stop"}"#) {
        println!("  (could not pause the guest, readings may be inconsistent: {e})");
    }
    let syms = match elf_function_symbols(&kernel_elf()) {
        Ok(s) => s,
        Err(e) => {
            println!("  (kernel symbols unavailable: {e})");
            Vec::new()
        }
    };
    match qmp.hmp("info registers -a") {
        Ok(text) => {
            let mut cpu = 0usize;
            let mut last_rbx = 0u64;
            let mut last_rsp = 0u64;
            let mut cpu_state: Vec<(usize, u64, u64, u64)> = Vec::new();
            let grab = |line: &str, key: &str| -> Option<u64> {
                let pos = line.find(key)?;
                let hex: String = line[pos + key.len()..]
                    .chars()
                    .skip_while(|c| c.is_whitespace() || *c == '=')
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect();
                u64::from_str_radix(&hex, 16).ok()
            };
            for line in text.lines() {
                if let Some(v) = grab(line, "RBX=") {
                    last_rbx = v;
                }
                if let Some(v) = grab(line, "RSP=") {
                    last_rsp = v;
                }
                if let Some(rest) = line.trim().strip_prefix("CPU#") {
                    cpu = rest.trim().parse().unwrap_or(cpu);
                }
                // The monitor prints `RIP=<hex>` on the register dump's second line.
                if let Some(pos) = line.find("RIP=") {
                    let hex: String = line[pos + 4..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                    if let Ok(rip) = u64::from_str_radix(&hex, 16) {
                        let where_ = resolve_symbol(&syms, rip).unwrap_or_else(|| {
                            if rip >= 0xFFFF_8000_0000_0000 {
                                "<kernel, no symbol>".to_string()
                            } else {
                                "<userspace>".to_string()
                            }
                        });
                        println!("  CPU#{cpu}  RIP={rip:#018x}  {where_}");
                        cpu_state.push((cpu, rip, last_rbx, last_rsp));
                    }
                }
            }
            // The two questions the summary above cannot answer: are the spinning CPUs
            // waiting on the *same* lock (RBX carries the `&SlabCache` receiver), and is
            // either of them already inside that function further down its own stack —
            // which is what re-entrancy looks like.
            for &(cpu, _rip, rbx, rsp) in &cpu_state {
                println!("\n  CPU#{cpu}  RBX={rbx:#018x}  RSP={rsp:#018x}  stack:");
                stack_trace(qmp, &syms, cpu, rsp, 48);
            }
        }
        Err(e) => println!("  (info registers failed: {e})"),
    }
    match qmp.hmp("info cpus") {
        Ok(text) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                println!("  {}", line.trim());
            }
        }
        Err(e) => println!("  (info cpus failed: {e})"),
    }
    // The full register block too, verbose but only on a failure. The summary above says
    // *where* each CPU is; this says *what it is working on* — and for a spin inside a
    // lock acquire, the lock's address is in a register, which is the only way to tell one
    // CPU waiting on another from two CPUs waiting on nothing.
    if let Ok(text) = qmp.hmp("info registers -a") {
        println!("\n  --- full registers ---");
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            println!("  {}", line.trim_end());
        }
    }
    println!("─────────────────────────────\n");
}

/// Which of `sent` — the lines typed since the last match — would satisfy `pat` by echo alone.
///
/// **A free function so it can be table-tested.** Inline in [`Session::expect`] the only thing
/// proving it worked would be a manual mutation of the gate, which is the situation this guard
/// exists to argue against; the same reasoning is written out at
/// [`scope_binding`]. The live risk is not the predicate's logic but its inputs: `sent` is
/// appended to in two places, and a later `send_line`-style wrapper that forgets to do so
/// disarms the guard for those steps while every gate stays green.
pub(crate) fn echo_source<'a>(sent: &'a [String], pat: &str) -> Option<&'a str> {
    if pat.is_empty() {
        return None;
    }
    sent.iter().find(|s| s.contains(pat)).map(|s| s.as_str())
}
