.section .text,"ax"
// i64 mirror_smp_syscall(u64 number, u64 arg0, u64 arg1, u64 arg2, u64 canary)
//
// The SMP battery's checked native call (spec 0051, risk 1): load RBX and
// R12-R15, and both halves of XMM8-XMM12, with values derived from the
// canary, make the call, and require every one back unchanged. The kernel
// restores all of a thread's registers but RAX (and a successful WAIT_U32's
// RSI and RDX, which carry its stamps), on whichever CPU it resumes, so a
// context or FPU image restored into the wrong thread exits the process with
// status 700. R8 carries the canary across the call.
.global mirror_smp_syscall
.type mirror_smp_syscall,@function
mirror_smp_syscall:
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov r12, r8
    mov r13, r8
    not r13
    mov r14, r8
    rol r14, 17
    mov r15, r8
    ror r15, 29
    lea rbx, [r8 + 1]
    movq xmm8, r12
    punpcklqdq xmm8, xmm8
    movq xmm9, r13
    punpcklqdq xmm9, xmm9
    movq xmm10, r14
    punpcklqdq xmm10, xmm10
    movq xmm11, r15
    punpcklqdq xmm11, xmm11
    movq xmm12, rbx
    punpcklqdq xmm12, xmm12
    mov rax, rdi
    mov rdi, rsi
    mov rsi, rdx
    mov rdx, rcx
    int 0x80
    cmp r12, r8
    jne .Lsmp_bad
    mov rcx, r8
    not rcx
    cmp r13, rcx
    jne .Lsmp_bad
    mov rcx, r8
    rol rcx, 17
    cmp r14, rcx
    jne .Lsmp_bad
    mov rcx, r8
    ror rcx, 29
    cmp r15, rcx
    jne .Lsmp_bad
    lea rcx, [r8 + 1]
    cmp rbx, rcx
    jne .Lsmp_bad
    movq rcx, xmm8
    cmp rcx, r12
    jne .Lsmp_bad
    pshufd xmm0, xmm8, 0x4e
    movq rcx, xmm0
    cmp rcx, r12
    jne .Lsmp_bad
    movq rcx, xmm9
    cmp rcx, r13
    jne .Lsmp_bad
    pshufd xmm0, xmm9, 0x4e
    movq rcx, xmm0
    cmp rcx, r13
    jne .Lsmp_bad
    movq rcx, xmm10
    cmp rcx, r14
    jne .Lsmp_bad
    pshufd xmm0, xmm10, 0x4e
    movq rcx, xmm0
    cmp rcx, r14
    jne .Lsmp_bad
    movq rcx, xmm11
    cmp rcx, r15
    jne .Lsmp_bad
    pshufd xmm0, xmm11, 0x4e
    movq rcx, xmm0
    cmp rcx, r15
    jne .Lsmp_bad
    movq rcx, xmm12
    cmp rcx, rbx
    jne .Lsmp_bad
    pshufd xmm0, xmm12, 0x4e
    movq rcx, xmm0
    cmp rcx, rbx
    jne .Lsmp_bad
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    ret
.Lsmp_bad:
    mov eax, 3
    mov edi, 700
    int 0x80
    ud2
.size mirror_smp_syscall, .-mirror_smp_syscall
.section .note.GNU-stack,"",@progbits
