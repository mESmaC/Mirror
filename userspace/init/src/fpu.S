.section .text,"ax"
.global mirror_probe_fpu
.type mirror_probe_fpu,@function
mirror_probe_fpu:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    sub rsp, 40
    mov r12, rdi
    mov r15, rsi
    mov r13, rdx
    mov r14, rcx
    mov ebx, 1

    // Exercise floating-point SSE2 and x87 arithmetic before filling canaries.
    // Integer PIDs and their doubles are exactly representable in both formats.
    fninit
    cvtsi2sd xmm0, r15
    addsd xmm0, xmm0
    cvttsd2si rax, xmm0
    lea rdx, [r15 + r15]
    cmp rax, rdx
    jne .Lfpu_fail
    mov [rsp], r15
    fild qword ptr [rsp]
    fadd st(0), st(0)
    fistp qword ptr [rsp]
    cmp [rsp], rdx
    jne .Lfpu_fail

    fldcw word ptr [r12]
    ldmxcsr dword ptr [r12 + 24]
    // Masked divide-by-zero leaves a nonzero x87 status bit to preserve. The
    // temporary infinity is popped before eight finite, distinct values load.
    fld1
    fldz
    fdivp st(1), st(0)
    fstp st(0)
    lea rax, [r15 * 8]
    mov ecx, 8
.Lfpu_seed_x87:
    inc rax
    mov [rsp], rax
    fild qword ptr [rsp]
    loop .Lfpu_seed_x87
    // A nonzero TOP catches restore paths that preserve data but lose the x87
    // logical-stack rotation. All eight entries remain occupied and finite.
    fincstp
    // Establish condition bits that arithmetic otherwise leaves undefined.
    ftst
    movdqu xmm0, [r12 + 160]
    movdqu xmm1, [r12 + 176]
    movdqu xmm2, [r12 + 192]
    movdqu xmm3, [r12 + 208]
    movdqu xmm4, [r12 + 224]
    movdqu xmm5, [r12 + 240]
    movdqu xmm6, [r12 + 256]
    movdqu xmm7, [r12 + 272]
    movdqu xmm8, [r12 + 288]
    movdqu xmm9, [r12 + 304]
    movdqu xmm10, [r12 + 320]
    movdqu xmm11, [r12 + 336]
    movdqu xmm12, [r12 + 352]
    movdqu xmm13, [r12 + 368]
    movdqu xmm14, [r12 + 384]
    movdqu xmm15, [r12 + 400]
    fxsave64 [r12]
    cmp byte ptr [r12 + 4], 0xff
    jne .Lfpu_fail
    test word ptr [r12 + 2], 4
    jz .Lfpu_fail

    // An ordinary syscall must preserve the entire extended context too.
    mov ebx, 2
    mov eax, 6
    int 0x80
    cmp rax, r15
    jne .Lfpu_fail
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail

    mov qword ptr [rsp + 8], 4
.Lfpu_switches:
    mov ebx, 3
    mov eax, 4
    int 0x80
    test rax, rax
    jnz .Lfpu_fail
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail
    mov ebx, 4
    mov eax, 5
    mov edi, 2
    int 0x80
    test rax, rax
    jnz .Lfpu_fail
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail
    dec qword ptr [rsp + 8]
    jnz .Lfpu_switches

    // Both pages are reserved but untouched: first fault directly in userspace,
    // then make VM_STATS fault in its output page through deferred syscall copy.
    mov ebx, 5
    mov [r13], r15
    cmp [r13], r15
    jne .Lfpu_fail
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail
    mov ebx, 6
    mov eax, 26
    lea rdi, [r13 + 4096]
    mov esi, 48
    xor edx, edx
    int 0x80
    cmp rax, 48
    jne .Lfpu_fail
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail

    // The kernel records its preemption count at this marker and requires that
    // it increase before the success marker. This interval never yields/sleeps.
    mov ebx, 7
    mov eax, 1
    lea rdi, [rip + .Lfpu_preemption_marker]
    mov esi, {preemption_marker_len}
    mov rdx, r14
    int 0x80
    cmp rax, {preemption_marker_len}
    jne .Lfpu_fail
    mov eax, 2
    int 0x80
    test rax, rax
    js .Lfpu_fail
    mov [rsp + 16], rax
    mov [rsp + 24], rax
    mov qword ptr [rsp + 32], 1000000
.Lfpu_busy:
    mov ecx, 4096
.Lfpu_arithmetic:
    // IRQs may arrive between either pair. Every other SIMD/x87 register stays
    // live, and exact reversible arithmetic must recover the original canary.
    paddq xmm0, xmm15
    fadd st(0), st(7)
    psubq xmm0, xmm15
    fsub st(0), st(7)
    dec ecx
    jnz .Lfpu_arithmetic
    ftst
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail
    mov eax, 2
    int 0x80
    cmp rax, [rsp + 24]
    jl .Lfpu_fail
    mov [rsp + 24], rax
    sub rax, [rsp + 16]
    cmp rax, 8
    jae .Lfpu_pass
    dec qword ptr [rsp + 32]
    jnz .Lfpu_busy
    jmp .Lfpu_fail
.Lfpu_pass:
    call .Lfpu_compare
    test eax, eax
    jnz .Lfpu_fail
    xor ebx, ebx
.Lfpu_fail:
    // Leave register payloads dirty but honor C's empty x87 stack and preserved
    // default control modes before compiler-generated Rust code resumes.
    fninit
    ldmxcsr dword ptr [rip + .Lfpu_default_mxcsr]
    mov eax, ebx
    add rsp, 40
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
.size mirror_probe_fpu, .-mirror_probe_fpu

// R12 points at two aligned FXSAVE64 areas. Ignore reserved padding, MXCSR_MASK
// and instruction/data pointers. Compare every meaningful control/status byte,
// all eight 80-bit x87 values (the registers also alias MMX), and all 16 XMMs.
.Lfpu_compare:
    fxsave64 [r12 + 512]
    xor ecx, ecx
.Lfpu_compare_control:
    mov al, [r12 + rcx]
    cmp al, [r12 + rcx + 512]
    jne .Lfpu_mismatch
    inc ecx
    cmp ecx, 5
    jne .Lfpu_compare_control
    mov eax, [r12 + 24]
    cmp eax, [r12 + 536]
    jne .Lfpu_mismatch
    mov ecx, 32
.Lfpu_compare_x87:
    mov rax, [r12 + rcx]
    cmp rax, [r12 + rcx + 512]
    jne .Lfpu_mismatch
    mov ax, [r12 + rcx + 8]
    cmp ax, [r12 + rcx + 520]
    jne .Lfpu_mismatch
    add ecx, 16
    cmp ecx, 160
    jne .Lfpu_compare_x87
.Lfpu_compare_xmm:
    mov rax, [r12 + rcx]
    cmp rax, [r12 + rcx + 512]
    jne .Lfpu_mismatch
    add ecx, 8
    cmp ecx, 416
    jne .Lfpu_compare_xmm
    xor eax, eax
    ret
.Lfpu_mismatch:
    mov eax, 1
    ret

.global mirror_probe_x87_fault
.type mirror_probe_x87_fault,@function
mirror_probe_x87_fault:
    fninit
    fldcw word ptr [rip + .Lfpu_unmasked_x87]
    fld1
    fldz
    fdivp st(1), st(0)
    fwait
    ret
.size mirror_probe_x87_fault, .-mirror_probe_x87_fault

.global mirror_probe_sse_fault
.type mirror_probe_sse_fault,@function
mirror_probe_sse_fault:
    ldmxcsr dword ptr [rip + .Lfpu_unmasked_sse]
    pxor xmm0, xmm0
    mov eax, 1
    cvtsi2sd xmm1, rax
    divsd xmm1, xmm0
    ret
.size mirror_probe_sse_fault, .-mirror_probe_sse_fault

.global mirror_probe_avx_fault
.type mirror_probe_avx_fault,@function
mirror_probe_avx_fault:
    // VXORPS XMM0,XMM0,XMM0 must #UD while this kernel has not enabled AVX.
    // Raw encoding keeps this negative probe independent of compiler features.
    .byte 0xc5, 0xf8, 0x57, 0xc0
    ret
.size mirror_probe_avx_fault, .-mirror_probe_avx_fault

.section .rodata,"a"
.balign 4
.Lfpu_default_mxcsr:
    .long 0x1f80
.Lfpu_unmasked_sse:
    .long 0x1d80
.Lfpu_unmasked_x87:
    .short 0x037b
.Lfpu_preemption_marker:
    .ascii "Mirror init: FPU preemption begin\n"
