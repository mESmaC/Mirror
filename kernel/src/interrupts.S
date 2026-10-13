# CPU exception/IRQ entry, x86_64-unknown-none: SysV ABI, no red zone, soft float.
# Same-process IRQ/syscall returns retain the live user x87/SIMD registers.
# Scheduling saves/restores them in userspace_entry.S. Enabling SIMD in kernel
# Rust would additionally require saving extended processor state here.
#
# GS discipline (spec 0050-D1). Whenever kernel code runs, IA32_GS_BASE names
# this CPU's CpuData. IA32_KERNEL_GS_BASE is scratch in kernel mode, and holds
# CpuData whenever IA32_GS_BASE is not kernel-half. An entry from CPL3 saves
# the user's selectors and bases into the frame, nulls both selectors, zeroes
# the FS base, then swaps GS. An exit to CPL3 resets IA32_KERNEL_GS_BASE to
# CpuData, swaps, loads the user's selectors, then writes both bases. CPL0
# entries and exits touch no selector and no base. NMI, #DF and #MC use the
# paranoid entry below, which swaps only when IA32_GS_BASE is not kernel-half
# and undoes exactly that swap.
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

# The IST vectors can interrupt any instruction, including the entry and exit
# sequences below while a user GS base is live.
.macro MIRROR_PARANOID vector, has_error=0
.type mirror_exception_\vector, @function
mirror_exception_\vector:
    .if \has_error == 0
        push 0
    .endif
    push \vector
    jmp mirror_paranoid_common
.size mirror_exception_\vector, .-mirror_exception_\vector
.endm

MIRROR_EXCEPTION 0
MIRROR_EXCEPTION 1
MIRROR_PARANOID 2
MIRROR_EXCEPTION 3
MIRROR_EXCEPTION 4
MIRROR_EXCEPTION 5
MIRROR_EXCEPTION 6
MIRROR_EXCEPTION 7
MIRROR_PARANOID 8, 1
MIRROR_EXCEPTION 9
MIRROR_EXCEPTION 10, 1
MIRROR_EXCEPTION 11, 1
MIRROR_EXCEPTION 12, 1
MIRROR_EXCEPTION 13, 1
MIRROR_EXCEPTION 14, 1
MIRROR_EXCEPTION 15
MIRROR_EXCEPTION 16
MIRROR_EXCEPTION 17, 1
MIRROR_PARANOID 18
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

# Every CPU's local timer uses the same complete register/TLS frame as device
# IRQs.
.global mirror_exception_241
MIRROR_EXCEPTION 241

# The reschedule IPI, which any CPU may send, uses a kernel-only gate and the
# shared full frame too (spec 0051, point 3).
.global mirror_exception_242
MIRROR_EXCEPTION 242

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
    sub rsp, {tls_prefix}
    # The hardware frame's CS says which side was interrupted.
    test byte ptr [rsp + {frame_cs}], 3
    jz 1f
    # CPL3 entry: capture the user's selectors and actual hidden bases. A user
    # selector load may have changed a base without a TLS syscall.
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
    # A selector load overwrites the hidden base, so the selectors are nulled
    # before the swap. A null load alone does not clear the FS base on every
    # CPU, so the base is written. SWAPGS then installs this CPU's CpuData,
    # which the last CPL3 exit left in IA32_KERNEL_GS_BASE.
    xor eax, eax
    mov fs, ax
    mov gs, ax
    xor edx, edx
    mov ecx, 0xc0000100
    wrmsr
    swapgs
    jmp 2f
1:
    # CPL0 entry: the kernel's own TLS state stays untouched, and the exit
    # ignores these fields.
    xor eax, eax
    mov [rsp + {fs_selector}], rax
    mov [rsp + {gs_selector}], rax
    mov [rsp + {fs_base}], rax
    mov [rsp + {gs_base}], rax
2:
    # Rust requires DF=0. IRET restores the interrupted RFLAGS, including DF.
    cld
    # With SMAP on, Rust also runs with AC=0, or an entry from CPL3 with AC set
    # would let the kernel touch user pages. IRET restores AC too. The BSP sets
    # this byte before any CPU enables SMAP, and only when its CPUID reports
    # SMAP, where CLAC is defined. An AP whose CPUID differs fails before it
    # loads the IDT (hardening.rs, smp.rs). No code is patched.
    cmp byte ptr [rip + MIRROR_SMAP_ACTIVE], 0
    je 3f
    clac
3:
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
# Rust validated a CPL3 destination frame, selectors and bases included. A
# CPL0 destination, including the suspend continuation, needs no TLS work.
.global mirror_restore_frame
.type mirror_restore_frame, @function
mirror_restore_frame:
    test byte ptr [rsp + {frame_cs}], 3
    jz 1f
    # CPL3 exit. IA32_GS_BASE names CpuData here; put it back in the scratch
    # MSR as well, so from the SWAPGS on both name it until the base write.
    mov rax, qword ptr gs:[{cpu_self}]
    mov rdx, rax
    shr rdx, 32
    mov ecx, 0xc0000102
    wrmsr
    swapgs
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
1:
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

# Paranoid entry for NMI, #DF and #MC. Below the trap frame sits a trailer:
# the paranoid word (bit 0: swapped GS, bit 1: nulled the FS selector, bit 2:
# zeroed the FS base), then the interrupted FS selector and FS base. Rust
# receives only the trap frame, whose TLS fields record the interrupted state
# for diagnostics. The exit restores from the trailer alone and never runs the
# CPL3 exit above.
.type mirror_paranoid_common, @function
mirror_paranoid_common:
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
    sub rsp, {tls_prefix}
    xor eax, eax
    mov ax, fs
    mov [rsp + {fs_selector}], rax
    mov r13, rax
    mov ax, gs
    mov [rsp + {gs_selector}], rax
    mov ecx, 0xc0000100
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rsp + {fs_base}], rax
    mov r14, rax
    inc ecx
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rsp + {gs_base}], rax
    # Valid user GS bases are below 2^47 and kernel code always runs with
    # CpuData, so a clear bit 63 means a user base was live. The D1 invariant
    # then holds CpuData in IA32_KERNEL_GS_BASE (both are 0 before install).
    xor ebx, ebx
    test rax, rax
    js 1f
    swapgs
    or ebx, 1
1:
    # Kernel Rust runs with a null FS selector and a zero FS base. Null a
    # non-null selector first, since that load can change the base.
    test r13, r13
    jz 2f
    xor eax, eax
    mov fs, ax
    or ebx, 2
2:
    mov ecx, 0xc0000100
    rdmsr
    mov esi, eax
    or esi, edx
    jz 3f
    xor eax, eax
    xor edx, edx
    wrmsr
    or ebx, 4
3:
    cld
    cmp byte ptr [rip + MIRROR_SMAP_ACTIVE], 0
    je 4f
    clac
4:
    # The trailer, lowest first: the paranoid word, FS selector, FS base.
    push r14
    push r13
    push rbx
    lea rdi, [rsp + 24]
    mov r12, rsp
    and rsp, -16
    call mirror_paranoid_dispatch
    mov rsp, r12
    pop rbx
    pop r13
    pop r14
    # Undo exactly what the entry changed: the selector, then the base its
    # reload overwrites, then the swap.
    test ebx, 2
    jz 5f
    mov eax, r13d
    mov fs, ax
5:
    test ebx, 6
    jz 6f
    mov ecx, 0xc0000100
    mov rax, r14
    mov rdx, r14
    shr rdx, 32
    wrmsr
6:
    test ebx, 1
    jz 7f
    swapgs
7:
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
    add rsp, 16
    iretq
.size mirror_paranoid_common, .-mirror_paranoid_common

.section .rodata.exception_entries,"a",@progbits
.balign 8
.global mirror_exception_entries
.type mirror_exception_entries, @object
mirror_exception_entries:
.irp vector,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    .quad mirror_exception_\vector
.endr
.size mirror_exception_entries, .-mirror_exception_entries
