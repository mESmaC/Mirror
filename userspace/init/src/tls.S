.section .text,"ax"
.global mirror_probe_fsgsbase_fault
.type mirror_probe_fsgsbase_fault,@function
mirror_probe_fsgsbase_fault:
    xor eax, eax
    // WRFSBASE RAX must #UD even on a CPU advertising FSGSBASE. Direct writes
    // remain disabled until the native ABI explicitly supports that policy.
    .byte 0xf3, 0x48, 0x0f, 0xae, 0xd0
    ret
.size mirror_probe_fsgsbase_fault, .-mirror_probe_fsgsbase_fault
