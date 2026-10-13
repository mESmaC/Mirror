# Per-CPU resumable user context, SysV ABI, soft-float kernel Rust.
# Kernel code runs with this CPU's CpuData in GS (interrupts.S, spec 0050-D1).
# The original kernel call stack remains mapped but is never used by ring-3
# interrupts: TSS.RSP0 selects a separate supervisor stack for those entries.
.section .text.userspace_entry,"ax",@progbits
.global mirror_resume_user
.type mirror_resume_user, @function
mirror_resume_user:
    # Caller guarantees IF=0. Save every SysV callee-saved GPR, original CR0,
    # and the supervisor-only aligned extended-state pointer (second argument).
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov rax, cr0
    push rax
    push rsi
    # The return path finds the suspended stack in this CPU's CpuData.
    mov qword ptr gs:[{cpu_user_rsp}], rsp
    # Preserve the source frame while setting up the data segments.
    mov r8, rdi

    # Eager restore before any user execution. FNINIT first clears stale x87
    # pointers on AMD CPUs where FXRSTOR may skip them when incoming FSW.ES=0.
    # Unlike FINIT/FWAIT it never delivers the previous owner's pending #MF.
    # Every participating CPU uses the immutable BSP format. R8 retains the
    # source frame and RSI the state address while EDX:EAX holds the XSAVE mask.
    and rax, -9
    mov cr0, rax
    fninit
    cmp byte ptr [rip + MIRROR_XSAVE_ACTIVE], 0
    je 1f
    mov eax, {xstate_mask}
    xor edx, edx
    xrstor64 [rsi]
    jmp 2f
1:
    fxrstor64 [rsi]
2:

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
    # IRETQ restored the suspended stack, kernel CS/SS and sanitized IF=0
    # flags. That CPL0 exit left the TLS state alone: the CPL3 entry already
    # nulled both selectors, zeroed the FS base and installed CpuData in GS.
    # Save even faulted/exited processes before clearing their register data.
    # Neither save tests pending x87 exceptions. FNINIT and a full restore
    # install a clean image, including empty x87 payloads and YMM upper halves.
    # Keep the state address in RSI: XSAVE/XRSTOR consume EDX:EAX as a mask.
    pop rsi
    cmp byte ptr [rip + MIRROR_XSAVE_ACTIVE], 0
    je 1f
    mov eax, {xstate_mask}
    xor edx, edx
    xsave64 [rsi]
    fninit
    xrstor64 [rip + MIRROR_FPU_CLEAN]
    jmp 2f
1:
    fxsave64 [rsi]
    fninit
    fxrstor64 [rip + MIRROR_FPU_CLEAN]
2:
    mov ax, {kernel_data}
    mov ds, ax
    mov es, ax
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
