/* Linker script for the `hello` userspace program.
 *
 * Produces a static, non-PIE ET_EXEC at a fixed low user virtual address,
 * with page-aligned PT_LOAD segments so the kernel ELF loader's
 * `p_vaddr % PAGE == p_offset % PAGE` rule holds. Everything dynamic
 * (.interp/.dynamic/.got/.plt) is discarded — the loader rejects PT_INTERP
 * and we want no GOT/PLT indirection (relocation-model=static).
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* R + X */
    rodata PT_LOAD FLAGS(4);  /* R     */
    data   PT_LOAD FLAGS(6);  /* R + W */
}

SECTIONS
{
    /* Low canonical base, well below USER_VIRT_END (0x0000_8000_0000_0000)
     * and the loader's initial stack region near 0x0000_7FFF_FFFF_0000. */
    . = 0x400000;

    .text : { *(.text .text.*) } :text

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(0x1000);
    .data : { *(.data .data.*) } :data
    /* Keep the GOT in a loaded segment — precompiled code (e.g. alloc's
     * `__rust_realloc` shim) may reference symbols like `memcpy` via `jmp *[GOT]`;
     * discarding `.got` leaves those pointing at null. `.plt` stays discarded (a
     * static non-PIE binary has no PLT). See `userspace/init/user.ld`. */
    .got     : { *(.got) }     :data
    .got.plt : { *(.got.plt) } :data
    .bss  : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
        *(.interp)
        *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash)
        *(.plt) *(.plt.*)
    }
}
