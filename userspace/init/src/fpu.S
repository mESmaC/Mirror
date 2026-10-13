.section .text,"ax"
// Runtime discovery precedes any AVX instruction or Rust execution. CPUID
// gates XGETBV itself, and XCR0 must enable x87/SSE/YMM. Leaf 7 then selects
// AVX2 arithmetic independently of whether AVX state management is available.
.global mirror_fpu_initial
.type mirror_fpu_initial,@function
mirror_fpu_initial:
    push rdi
    call .Lfpu_detect
    mov [rip + MIRROR_INIT_FPU_FEATURES], eax
    mov esi, eax
    lea rdi, [rip + MIRROR_INIT_INITIAL_FX]
    call mirror_fpu_snapshot
    pop rdi
    ret
.size mirror_fpu_initial, .-mirror_fpu_initial

.Lfpu_detect:
    push rbx
    xor r10d, r10d
    xor eax, eax
    cpuid
    mov r9d, eax
    cmp eax, 1
    jb .Lfpu_detect_done
    mov eax, 1
    cpuid
    and ecx, 0x1c000000
    cmp ecx, 0x1c000000
    jne .Lfpu_detect_done
    xor ecx, ecx
    xgetbv
    and eax, 7
    cmp eax, 7
    jne .Lfpu_detect_done
    mov r10d, 1
    cmp r9d, 7
    jb .Lfpu_detect_done
    mov eax, 7
    xor ecx, ecx
    cpuid
    test ebx, 0x20
    jz .Lfpu_detect_done
    or r10d, 2
.Lfpu_detect_done:
    mov eax, r10d
    pop rbx
    ret

// RDI names a live aligned Snapshot and ESI its feature bits. These helpers
// preserve every GPR and do not touch SIMD state unless restoring the image.
// VEX extracts record upper halves independently of the kernel's XSAVE code.
.global mirror_fpu_snapshot
.type mirror_fpu_snapshot,@function
mirror_fpu_snapshot:
    fxsave64 [rdi]
    test esi, 1
    jz .Lfpu_snapshot_done
    vextractf128 [rdi + 576], ymm0, 1
    vextractf128 [rdi + 592], ymm1, 1
    vextractf128 [rdi + 608], ymm2, 1
    vextractf128 [rdi + 624], ymm3, 1
    vextractf128 [rdi + 640], ymm4, 1
    vextractf128 [rdi + 656], ymm5, 1
    vextractf128 [rdi + 672], ymm6, 1
    vextractf128 [rdi + 688], ymm7, 1
    vextractf128 [rdi + 704], ymm8, 1
    vextractf128 [rdi + 720], ymm9, 1
    vextractf128 [rdi + 736], ymm10, 1
    vextractf128 [rdi + 752], ymm11, 1
    vextractf128 [rdi + 768], ymm12, 1
    vextractf128 [rdi + 784], ymm13, 1
    vextractf128 [rdi + 800], ymm14, 1
    vextractf128 [rdi + 816], ymm15, 1
.Lfpu_snapshot_done:
    ret
.size mirror_fpu_snapshot, .-mirror_fpu_snapshot

.global mirror_fpu_restore
.type mirror_fpu_restore,@function
mirror_fpu_restore:
    fxrstor64 [rdi]
    jmp mirror_fpu_load_upper
.size mirror_fpu_restore, .-mirror_fpu_restore

.global mirror_fpu_load_upper
.type mirror_fpu_load_upper,@function
mirror_fpu_load_upper:
    test esi, 1
    jz .Lfpu_load_done
    vinsertf128 ymm0, ymm0, [rdi + 576], 1
    vinsertf128 ymm1, ymm1, [rdi + 592], 1
    vinsertf128 ymm2, ymm2, [rdi + 608], 1
    vinsertf128 ymm3, ymm3, [rdi + 624], 1
    vinsertf128 ymm4, ymm4, [rdi + 640], 1
    vinsertf128 ymm5, ymm5, [rdi + 656], 1
    vinsertf128 ymm6, ymm6, [rdi + 672], 1
    vinsertf128 ymm7, ymm7, [rdi + 688], 1
    vinsertf128 ymm8, ymm8, [rdi + 704], 1
    vinsertf128 ymm9, ymm9, [rdi + 720], 1
    vinsertf128 ymm10, ymm10, [rdi + 736], 1
    vinsertf128 ymm11, ymm11, [rdi + 752], 1
    vinsertf128 ymm12, ymm12, [rdi + 768], 1
    vinsertf128 ymm13, ymm13, [rdi + 784], 1
    vinsertf128 ymm14, ymm14, [rdi + 800], 1
    vinsertf128 ymm15, ymm15, [rdi + 816], 1
.Lfpu_load_done:
    ret
.size mirror_fpu_load_upper, .-mirror_fpu_load_upper

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
    mov rdi, r12
    mov esi, [r12 + 1664]
    call mirror_fpu_load_upper
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
    test dword ptr [r12 + 1664], 2
    jnz .Lfpu_arithmetic_avx2
    test dword ptr [r12 + 1664], 1
    jnz .Lfpu_arithmetic_avx
    paddq xmm0, xmm15
    fadd st(0), st(7)
    psubq xmm0, xmm15
    fsub st(0), st(7)
    jmp .Lfpu_arithmetic_next
.Lfpu_arithmetic_avx:
    vxorps ymm0, ymm0, ymm15
    fadd st(0), st(7)
    vxorps ymm0, ymm0, ymm15
    fsub st(0), st(7)
    jmp .Lfpu_arithmetic_next
.Lfpu_arithmetic_avx2:
    vpaddq ymm0, ymm0, ymm15
    fadd st(0), st(7)
    vpsubq ymm0, ymm0, ymm15
    fsub st(0), st(7)
.Lfpu_arithmetic_next:
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

// Snapshot before any compiler-generated code can touch the live registers.
.Lfpu_compare:
    lea rdi, [r12 + 832]
    mov esi, [r12 + 1664]
    sub rsp, 8
    call mirror_fpu_snapshot
    add rsp, 8
    mov rdi, r12
    lea rsi, [r12 + 832]
    mov edx, [r12 + 1664]
    jmp mirror_fpu_equal

// RDI/RSI name aligned snapshots; EDX holds feature bits. Compare the controls,
// all eight 80-bit x87 values, all sixteen XMMs and (when enabled) upper YMMs.
// FIP/FDP, reserved padding and MXCSR_MASK are not portable register canaries.
.global mirror_fpu_equal
.type mirror_fpu_equal,@function
mirror_fpu_equal:
    xor ecx, ecx
.Lfpu_compare_control:
    mov al, [rdi + rcx]
    cmp al, [rsi + rcx]
    jne .Lfpu_mismatch
    inc ecx
    cmp ecx, 5
    jne .Lfpu_compare_control
    mov eax, [rdi + 24]
    cmp eax, [rsi + 24]
    jne .Lfpu_mismatch
    mov ecx, 32
.Lfpu_compare_x87:
    mov rax, [rdi + rcx]
    cmp rax, [rsi + rcx]
    jne .Lfpu_mismatch
    mov ax, [rdi + rcx + 8]
    cmp ax, [rsi + rcx + 8]
    jne .Lfpu_mismatch
    add ecx, 16
    cmp ecx, 160
    jne .Lfpu_compare_x87
.Lfpu_compare_xmm:
    mov rax, [rdi + rcx]
    cmp rax, [rsi + rcx]
    jne .Lfpu_mismatch
    add ecx, 8
    cmp ecx, 416
    jne .Lfpu_compare_xmm
    test edx, 1
    jz .Lfpu_equal
    mov ecx, 576
.Lfpu_compare_ymm:
    mov rax, [rdi + rcx]
    cmp rax, [rsi + rcx]
    jne .Lfpu_mismatch
    add ecx, 8
    cmp ecx, 832
    jne .Lfpu_compare_ymm
.Lfpu_equal:
    xor eax, eax
    ret
.Lfpu_mismatch:
    mov eax, 1
    ret
.size mirror_fpu_equal, .-mirror_fpu_equal

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
    // VXORPS XMM0,XMM0,XMM0 must #UD on the fallback CPU without AVX.
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
