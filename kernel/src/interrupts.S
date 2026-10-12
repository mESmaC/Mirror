# CPU exception/IRQ entry, x86_64-unknown-none: SysV ABI, no red zone, soft float.
# Same-process IRQ/syscall returns retain the live user x87/SIMD registers.
# Scheduling saves/restores them in userspace_entry.S. Enabling SIMD in kernel
# Rust would additionally require saving extended processor state here.
.section .text.exceptions,"ax",@progbits

.macro MIRROR_EXCEPTION vector, has_error=0
.type mirror_exception_\vector, @function
mirror_exception_\vector:
    .if \has_error == 0
        push 0
    .endif
    push \vector
    jmp mirror_exception_common
.size mirror_exception_\vector, .-mirror_exception_\vector
.endm

MIRROR_EXCEPTION 0
MIRROR_EXCEPTION 1
MIRROR_EXCEPTION 2
MIRROR_EXCEPTION 3
MIRROR_EXCEPTION 4
MIRROR_EXCEPTION 5
MIRROR_EXCEPTION 6
MIRROR_EXCEPTION 7
MIRROR_EXCEPTION 8, 1
MIRROR_EXCEPTION 9
MIRROR_EXCEPTION 10, 1
MIRROR_EXCEPTION 11, 1
MIRROR_EXCEPTION 12, 1
MIRROR_EXCEPTION 13, 1
MIRROR_EXCEPTION 14, 1
MIRROR_EXCEPTION 15
MIRROR_EXCEPTION 16
MIRROR_EXCEPTION 17, 1
MIRROR_EXCEPTION 18
MIRROR_EXCEPTION 19
MIRROR_EXCEPTION 20
MIRROR_EXCEPTION 21, 1
MIRROR_EXCEPTION 22
MIRROR_EXCEPTION 23
MIRROR_EXCEPTION 24
MIRROR_EXCEPTION 25
MIRROR_EXCEPTION 26
MIRROR_EXCEPTION 27
MIRROR_EXCEPTION 28
MIRROR_EXCEPTION 29, 1
MIRROR_EXCEPTION 30, 1
MIRROR_EXCEPTION 31

# ISA IRQ0..15 use these vectors through either the I/O APIC or remapped PIC.
# Neither interrupt controller pushes a hardware error code.
.irp vector,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    MIRROR_EXCEPTION \vector
.endr

# Native syscall interrupt gate is the only software gate callable at CPL3.
.global mirror_exception_128
MIRROR_EXCEPTION 128

# Fixed delivery LAPIC IPIs use a kernel-only gate and the shared full frame.
.global mirror_exception_240
MIRROR_EXCEPTION 240

# BSP local timer uses the same complete register/TLS frame as device IRQs.
.global mirror_exception_241
MIRROR_EXCEPTION 241

# LAPIC spurious interrupts need no EOI and share the full TLS-preserving entry.
.global mirror_exception_255
MIRROR_EXCEPTION 255

.type mirror_exception_common, @function
mirror_exception_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    # Selector loads can change the hidden bases even with CR4.FSGSBASE clear.
    # Capture the actual hardware state, including on CPL0 entries: an NMI or
    # exception can interrupt the assembly that is restoring a user context.
    sub rsp, {tls_prefix}
    xor eax, eax
    mov ax, fs
    mov [rsp + {fs_selector}], rax
    mov ax, gs
    mov [rsp + {gs_selector}], rax
    mov ecx, 0xc0000100
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rsp + {fs_base}], rax
    inc ecx
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rsp + {gs_base}], rax
    # Rust never runs with a user's bases or selectors installed. Loading a
    # null selector alone does not clear the hidden base on every x86 CPU.
    xor eax, eax
    mov fs, ax
    mov gs, ax
    xor edx, edx
    mov ecx, 0xc0000100
    wrmsr
    inc ecx
    wrmsr
    # Rust requires DF=0. IRET restores the interrupted RFLAGS, including DF.
    cld
    # With SMAP on, Rust also runs with AC=0, or an entry from CPL3 with AC set
    # would let the kernel touch user pages. IRET restores AC too. The BSP sets
    # this byte before any CPU enables SMAP, and only when its CPUID reports
    # SMAP, where CLAC is defined. An AP whose CPUID differs fails before it
    # loads the IDT (hardening.rs, smp.rs). No code is patched.
    cmp byte ptr [rip + MIRROR_SMAP_ACTIVE], 0
    je 2f
    clac
2:
    mov rdi, rsp
    # R12 is callee-saved and its interrupted value is already on the stack.
    # Keep the exact frame address while independently aligning the call.
    mov r12, rsp
    and rsp, -16
    call mirror_exception_dispatch
    mov rsp, r12
    jmp mirror_restore_frame
.size mirror_exception_common, .-mirror_exception_common

# Both interrupt return and scheduled user entry use this exact epilogue.
# The supervisor-owned frame was validated by Rust for a CPL3 destination;
# kernel entries preserve their captured state, including in-flight restores.
.global mirror_restore_frame
.type mirror_restore_frame, @function
mirror_restore_frame:
    # Reload selectors before bases: MOV to FS/GS may overwrite the base.
    mov rax, [rsp + {fs_selector}]
    mov fs, ax
    mov rax, [rsp + {gs_selector}]
    mov gs, ax
    mov ecx, 0xc0000100
    mov rax, [rsp + {fs_base}]
    mov rdx, rax
    shr rdx, 32
    wrmsr
    inc ecx
    mov rax, [rsp + {gs_base}]
    mov rdx, rax
    shr rdx, 32
    wrmsr
    add rsp, {tls_prefix}
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    # Remove vector plus hardware/synthetic error code, then the CPU frame.
    add rsp, 16
    iretq
.size mirror_restore_frame, .-mirror_restore_frame

.section .rodata.exception_entries,"a",@progbits
.balign 8
.global mirror_exception_entries
.type mirror_exception_entries, @object
mirror_exception_entries:
.irp vector,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    .quad mirror_exception_\vector
.endr
.size mirror_exception_entries, .-mirror_exception_entries
