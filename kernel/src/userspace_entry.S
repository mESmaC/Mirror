# Single-BSP synchronous user entry, SysV ABI, integer/soft-float only.
# The original kernel call stack remains mapped but is never used by ring-3
# interrupts: TSS.RSP0 selects a separate supervisor stack for those entries.
.section .text.userspace_entry,"ax",@progbits
.global mirror_enter_user
.type mirror_enter_user, @function
mirror_enter_user:
    # Caller guarantees IF=0. Save every SysV callee-saved GPR and original CR0.
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov rax, cr0
    push rax
    mov [rip + MIRROR_USER_KERNEL_RSP], rsp
    mov r8, rdx

    # No x87/SIMD state management yet. TS makes user use trap with #NM instead
    # of exposing prior extended state; the soft-float kernel needs no such state.
    or rax, 8
    mov cr0, rax

    mov ax, {user_data}
    mov ds, ax
    mov es, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax
    # Explicitly clear bases as a null selector does not clear them on all CPUs.
    mov ecx, 0xc0000100
    xor edx, edx
    wrmsr
    inc ecx
    wrmsr

    # IRETQ consumes SS:RSP, RFLAGS, CS:RIP and transitions to CPL3.
    push {user_data}
    push rsi
    push 0x202
    push {user_code}
    push rdi
    mov rdi, r8

    # RDI is the explicit integer argument. Other GPRs and flags are cleared.
    xor eax, eax
    xor ebx, ebx
    xor ecx, ecx
    xor edx, edx
    xor esi, esi
    xor ebp, ebp
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor r11d, r11d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    iretq
.size mirror_enter_user, .-mirror_enter_user

.global mirror_resume_kernel
.type mirror_resume_kernel, @function
mirror_resume_kernel:
    # Reached only by the common interrupt epilogue after Rust handlers return.
    # IRETQ restored the suspended stack, kernel CS/SS, and sanitized IF=0 flags.
    mov ax, {kernel_data}
    mov ds, ax
    mov es, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax
    mov ecx, 0xc0000100
    xor edx, edx
    wrmsr
    inc ecx
    wrmsr
    pop rax
    mov cr0, rax
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
.size mirror_resume_kernel, .-mirror_resume_kernel
