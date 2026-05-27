/* Nitrox kernel linker script — Limine boot protocol, x86_64 higher-half.
 *
 * Layout matches the Limine C template's x86_64.lds (see Limine PROTOCOL.md).
 * The kernel sits in the top 2 GiB of the canonical address space, which is
 * what -C code-model=kernel expects. Each output segment lives on its own
 * page so we can set distinct permissions per region later.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    limine_requests PT_LOAD;
    text            PT_LOAD;
    rodata          PT_LOAD;
    data            PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;

    /* Request markers are scanned by Limine; we bracket them so the
     * bootloader can locate them quickly. KEEP() prevents --gc-sections
     * from discarding the static request structs even though no Rust code
     * "uses" them in a way the linker can see. */
    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :limine_requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    /* User-memory-access exception table: 16-byte (fault_pc, recovery_pc)
     * entries emitted from inline asm in copy primitives. The bracketing
     * symbols give a (start, stop) pair the #PF handler walks; KEEP keeps
     * the entries even though no Rust code references them by name. The
     * section may be empty (no entries registered yet) — start == stop is
     * the well-defined "no recovery sites" case. */
    . = ALIGN(8);
    .user_access_table : {
        __start_user_access_table = .;
        KEEP(*(.user_access_table .user_access_table.*))
        __stop_user_access_table = .;
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
