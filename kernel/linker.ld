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
