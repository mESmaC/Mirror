.section .text,"ax"
.global mirror_thread_entry
.type mirror_thread_entry,@function
mirror_thread_entry:
    // Observe the raw launch contract before a compiler prologue touches it.
    test rax, rax
    jnz .Lthread_bad_entry
    test rbx, rbx
    jnz .Lthread_bad_entry
    test rcx, rcx
    jnz .Lthread_bad_entry
    test rdx, rdx
    jnz .Lthread_bad_entry
    test rsi, rsi
    jnz .Lthread_bad_entry
    test rbp, rbp
    jnz .Lthread_bad_entry
    test r8, r8
    jnz .Lthread_bad_entry
    test r9, r9
    jnz .Lthread_bad_entry
    test r10, r10
    jnz .Lthread_bad_entry
    test r11, r11
    jnz .Lthread_bad_entry
    test r12, r12
    jnz .Lthread_bad_entry
    test r13, r13
    jnz .Lthread_bad_entry
    test r14, r14
    jnz .Lthread_bad_entry
    test r15, r15
    jnz .Lthread_bad_entry
    mov esi, [rip + MIRROR_INIT_FPU_FEATURES]
    sub rsp, 8
    call mirror_fpu_snapshot
    add rsp, 8
    mov rsi, rsp
    jmp mirror_thread_main
.Lthread_bad_entry:
    mov eax, 3
    mov edi, 500
    int 0x80
    ud2
.size mirror_thread_entry, .-mirror_thread_entry

.global mirror_thread_fpu
.type mirror_thread_fpu,@function
mirror_thread_fpu:
    push rbx
    push r12
    push r13
    sub rsp, 16
    mov r12, rdi
    mov r13, rsi
    mov esi, [rip + MIRROR_INIT_FPU_FEATURES]
    call mirror_fpu_restore
    // Different contexts carry different x87/XMM/YMM payloads across scheduling.
    mov eax, 4
    xor edi, edi
    xor esi, esi
    xor edx, edx
    int 0x80
    mov eax, 5
    mov edi, 1
    int 0x80
    mov eax, 4
    xor edi, edi
    int 0x80
    // A fresh dispatch has a two-tick quantum. Waiting four wall-clock ticks
    // without blocking forces timer preemption while these registers are live.
    mov eax, 2
    int 0x80
    mov [rsp + 8], rax
    mov ebx, 1000000
.Lthread_busy:
    mov ecx, 4096
.Lthread_arithmetic:
    test dword ptr [rip + MIRROR_INIT_FPU_FEATURES], 2
    jnz .Lthread_arithmetic_avx2
    test dword ptr [rip + MIRROR_INIT_FPU_FEATURES], 1
    jnz .Lthread_arithmetic_avx
    pxor xmm0, xmm15
    fchs
    pxor xmm0, xmm15
    fchs
    jmp .Lthread_arithmetic_next
.Lthread_arithmetic_avx:
    vxorps ymm0, ymm0, ymm15
    fchs
    vxorps ymm0, ymm0, ymm15
    fchs
    jmp .Lthread_arithmetic_next
.Lthread_arithmetic_avx2:
    vpaddq ymm0, ymm0, ymm15
    fchs
    vpsubq ymm0, ymm0, ymm15
    fchs
.Lthread_arithmetic_next:
    dec ecx
    jnz .Lthread_arithmetic
    mov eax, 2
    int 0x80
    sub rax, [rsp + 8]
    cmp rax, 4
    jae .Lthread_snapshot
    dec ebx
    jnz .Lthread_busy
    mov eax, 3
    mov edi, 571
    int 0x80
    ud2
.Lthread_snapshot:
    mov rdi, r13
    mov esi, [rip + MIRROR_INIT_FPU_FEATURES]
    call mirror_fpu_snapshot
    fninit
    mov dword ptr [rsp], 0x1f80
    ldmxcsr [rsp]
    add rsp, 16
    pop r13
    pop r12
    pop rbx
    ret
.size mirror_thread_fpu, .-mirror_thread_fpu
.section .note.GNU-stack,"",@progbits
