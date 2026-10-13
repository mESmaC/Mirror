.section .text,"ax"
// i64 mirror_smp_syscall(u64 number, u64 arg0, u64 arg1, u64 arg2, u64 canary)
//
// Seed every x87/XMM/YMM register and its controls, plus RBX/R12-R15, before
// the native call. Snapshot and compare before returning to Rust. The same
// wrapper covers blocking waits and the forced BSP/AP/BSP migration prelude.
// A mismatch terminates the process with status 700.
.global mirror_smp_syscall
.type mirror_smp_syscall,@function
mirror_smp_syscall:
    push rbp
    mov rbp, rsp
    push rbx
    push r12
    push r13
    push r14
    push r15
    and rsp, -64
    // Two 832-byte snapshots followed by the arguments, return value and MXCSR.
    sub rsp, 1728
    mov [rsp + 1664], rdi
    mov [rsp + 1672], rsi
    mov [rsp + 1680], rdx
    mov [rsp + 1688], rcx
    mov [rsp + 1696], r8
    mov rdi, rsp
    mov ecx, 208
    xor eax, eax
    rep stosq

    // Alternate rounding directions and masked sticky exception bits by thread.
    mov rax, r8
    shr rax, 4
    and eax, 1
    mov ecx, eax
    inc eax
    mov edx, eax
    shl eax, 10
    or eax, 0x037f
    mov [rsp], ax
    mov word ptr [rsp + 2], 4
    mov byte ptr [rsp + 4], 0xff
    shl edx, 13
    or edx, 0x1f80
    mov eax, 1
    shl eax, cl
    or edx, eax
    mov [rsp + 24], edx

    // Eight exact normal x87 values, all different between slots and threads.
    mov ecx, 32
.Lsmp_seed_x87:
    mov rax, r8
    xor rax, rcx
    bts rax, 63
    mov [rsp + rcx], rax
    mov word ptr [rsp + rcx + 8], 0x3fff
    add ecx, 16
    cmp ecx, 160
    jne .Lsmp_seed_x87

    mov r9, 0xa731209d488c0000
    mov ecx, 160
.Lsmp_seed_xmm:
    mov rax, r8
    xor rax, r9
    xor rax, rcx
    mov [rsp + rcx], rax
    add ecx, 8
    cmp ecx, 416
    jne .Lsmp_seed_xmm
    mov r9, 0x583b7021ee09a46d
    mov ecx, 576
.Lsmp_seed_ymm:
    mov rax, r8
    xor rax, r9
    xor rax, rcx
    mov [rsp + rcx], rax
    add ecx, 8
    cmp ecx, 832
    jne .Lsmp_seed_ymm

    mov rdi, rsp
    mov esi, [rip + MIRROR_INIT_FPU_FEATURES]
    call mirror_fpu_restore
    mov r12, r8
    mov r13, r8
    not r13
    mov r14, r8
    rol r14, 17
    mov r15, r8
    ror r15, 29
    lea rbx, [r8 + 1]
    mov rax, [rsp + 1664]
    mov rdi, [rsp + 1672]
    mov rsi, [rsp + 1680]
    mov rdx, [rsp + 1688]
    int 0x80
    mov [rsp + 1704], rax
    cmp r8, [rsp + 1696]
    jne .Lsmp_bad
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
    lea rdi, [rsp + 832]
    mov esi, [rip + MIRROR_INIT_FPU_FEATURES]
    call mirror_fpu_snapshot
    mov rdi, rsp
    lea rsi, [rsp + 832]
    mov edx, [rip + MIRROR_INIT_FPU_FEATURES]
    call mirror_fpu_equal
    test eax, eax
    jnz .Lsmp_bad

    // Honor the C control-mode contract; leave register payloads dirty.
    fninit
    mov dword ptr [rsp + 1712], 0x1f80
    ldmxcsr [rsp + 1712]
    mov rax, [rsp + 1704]
    lea rsp, [rbp - 40]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
.Lsmp_bad:
    mov eax, 3
    mov edi, 700
    int 0x80
    ud2
.size mirror_smp_syscall, .-mirror_smp_syscall
.section .note.GNU-stack,"",@progbits
