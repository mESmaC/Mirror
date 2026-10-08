# Single-BSP resumable user context, SysV ABI, integer/soft-float only.
# The original kernel call stack remains mapped but is never used by ring-3
# interrupts: TSS.RSP0 selects a separate supervisor stack for those entries.
.section .text.userspace_entry,"ax",@progbits
.global mirror_resume_user
.type mirror_resume_user, @function
mirror_resume_user:
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
    # Preserve the source frame while setting up segments and writing MSRs.
    mov r8, rdi

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

    # Copy the validated frame below the suspended kernel context. The borrowed
    # source lives in the caller's stack frame, above this destination, and the
    # full copy finishes before restoring any user-controlled register.
    sub rsp, {frame_size}
    mov rsi, r8
    mov rdi, rsp
    mov ecx, {frame_words}
    cld
    rep movsq
    # Match TrapFrame and the common interrupt epilogue exactly.
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
.size mirror_resume_user, .-mirror_resume_user

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
