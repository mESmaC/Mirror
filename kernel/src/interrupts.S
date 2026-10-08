# CPU exception/IRQ entry, x86_64-unknown-none: SysV ABI, no red zone, soft float.
# Enabling SIMD in kernel Rust requires saving extended processor state here.
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

MIRROR_EXCEPTION 0
MIRROR_EXCEPTION 1
MIRROR_EXCEPTION 2
MIRROR_EXCEPTION 3
MIRROR_EXCEPTION 4
MIRROR_EXCEPTION 5
MIRROR_EXCEPTION 6
MIRROR_EXCEPTION 7
MIRROR_EXCEPTION 8, 1
MIRROR_EXCEPTION 9
MIRROR_EXCEPTION 10, 1
MIRROR_EXCEPTION 11, 1
MIRROR_EXCEPTION 12, 1
MIRROR_EXCEPTION 13, 1
MIRROR_EXCEPTION 14, 1
MIRROR_EXCEPTION 15
MIRROR_EXCEPTION 16
MIRROR_EXCEPTION 17, 1
MIRROR_EXCEPTION 18
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

# Remapped legacy PIC IRQ0..15 never push a hardware error code.
.irp vector,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    MIRROR_EXCEPTION \vector
.endr

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
    # Rust requires DF=0. IRET restores the interrupted RFLAGS, including DF.
    cld
    mov rdi, rsp
    # R12 is callee-saved and its interrupted value is already on the stack.
    # Keep the exact frame address while independently aligning the call.
    mov r12, rsp
    and rsp, -16
    call mirror_exception_dispatch
    mov rsp, r12
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
.size mirror_exception_common, .-mirror_exception_common

.section .rodata.exception_entries,"a",@progbits
.balign 8
.global mirror_exception_entries
.type mirror_exception_entries, @object
mirror_exception_entries:
.irp vector,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    .quad mirror_exception_\vector
.endr
.size mirror_exception_entries, .-mirror_exception_entries
