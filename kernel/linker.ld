OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

PHDRS
{
    requests PT_LOAD FLAGS(6);
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0xffffffff80000000;

    .limine_requests : ALIGN(0x1000)
    {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :requests

    /* The crash backtrace accepts only return addresses inside these
       bounds (spec 0051, point 11). */
    .text : ALIGN(0x1000)
    {
        __mirror_text_start = .;
        *(.text .text.*)
        __mirror_text_end = .;
    } :text
    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) } :rodata
    .data : ALIGN(0x1000) { *(.data .data.*) *(.got .got.*) } :data
    .bss : ALIGN(0x1000) { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.eh_frame*) *(.note .note.*) }
}
