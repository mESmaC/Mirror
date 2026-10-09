# Single-BSP resumable user context, SysV ABI, soft-float kernel Rust.
# The original kernel call stack remains mapped but is never used by ring-3
# interrupts: TSS.RSP0 selects a separate supervisor stack for those entries.
.section .text.userspace_entry,"ax",@progbits
.global mirror_resume_user
.type mirror_resume_user, @function
mirror_resume_user:
    # Caller guarantees IF=0. Save every SysV callee-saved GPR, original CR0,
    # and the supervisor-only aligned x87/SIMD image pointer (second argument).
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov rax, cr0
    push rax
    push rsi
    mov [rip + MIRROR_USER_KERNEL_RSP], rsp
    # Preserve the source frame while setting up the data segments.
    mov r8, rdi

    # Eager restore before any user execution. FNINIT first clears stale x87
    # pointers on AMD CPUs where FXRSTOR may skip them when incoming FSW.ES=0.
    # Unlike FINIT/FWAIT it never delivers the previous owner's pending #MF.
    and rax, -9
    mov cr0, rax
    fninit
    fxrstor64 [rsi]

    mov ax, {user_data}
    mov ds, ax
    mov es, ax

    # Copy the validated frame below the suspended kernel context. The borrowed
    # source lives in the caller's stack frame, above this destination, and the
    # full copy finishes before restoring any user-controlled register.
    sub rsp, {frame_size}
    mov rsi, r8
    mov rdi, rsp
    mov ecx, {frame_words}
    cld
    rep movsq
    # The shared epilogue restores FS/GS selectors, then bases, then all GPRs.
    jmp mirror_restore_frame
.size mirror_resume_user, .-mirror_resume_user

.global mirror_resume_kernel
.type mirror_resume_kernel, @function
mirror_resume_kernel:
    # Reached only by the common interrupt epilogue after Rust handlers return.
    # IRETQ restored the suspended stack, kernel CS/SS, sanitized IF=0 flags,
    # and the zero FS/GS state written by return_from_user to its continuation.
    # Save even faulted/exited processes before clearing their register data.
    # FXSAVE does not test pending x87 exceptions; FNINIT then FXRSTOR installs
    # an entirely clean kernel image, including data in empty x87 registers.
    pop rax
    fxsave64 [rax]
    fninit
    fxrstor64 [rip + MIRROR_FPU_CLEAN]
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
