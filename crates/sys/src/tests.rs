use super::*;

#[test]
fn syscall_numbers_preserve_the_existing_wire_abi_and_reservations() {
    let live = [
        SYS_WRITE,
        SYS_TICKS,
        SYS_EXIT,
        SYS_YIELD,
        SYS_SLEEP,
        SYS_PID,
        SYS_BOOT_HANDLE,
        SYS_SEND,
        SYS_RECV,
        SYS_RECV_WAIT,
        SYS_CLOSE,
        SYS_DUP,
        SYS_SURFACE_INFO,
        SYS_SURFACE_PRESENT,
        SYS_INPUT_WAIT,
        SYS_INPUT_POLL,
        SYS_APP_LAUNCH,
        SYS_APP_INFO,
        SYS_APP_EVENT,
        SYS_APP_READ,
        SYS_APP_CLOSE,
        SYS_SURFACE_COMMIT,
        SYS_VM_RESERVE,
        SYS_VM_RELEASE,
        SYS_VM_PROTECT,
        SYS_VM_STATS,
        SYS_TLS_SET,
        SYS_TLS_GET,
        SYS_THREAD_CREATE,
        SYS_THREAD_EXIT,
        SYS_THREAD_JOIN,
        SYS_THREAD_ID,
        SYS_WAIT_U32,
        SYS_WAKE_U32,
        SYS_CLOCK_GET,
    ];
    for (index, number) in live.into_iter().enumerate() {
        assert_eq!(number, index as u64 + 1);
    }
    assert_eq!(SYS_REALTIME_GET, 36);
    assert_eq!(SYS_RANDOM_BYTES, 37);
}

#[test]
fn errors_preserve_the_existing_signed_wire_results() {
    assert_eq!(
        [
            ESRCH, EBADF, EAGAIN, ENOMEM, EACCES, EFAULT, EBUSY, EEXIST, ENODEV, EINVAL, EMFILE,
            EPIPE, EDEADLK, ENOSYS, EOVERFLOW, EMSGSIZE, ETIMEDOUT
        ],
        [
            -3, -9, -11, -12, -13, -14, -16, -17, -19, -22, -24, -32, -35, -38, -75, -90, -110
        ]
    );
    assert_eq!(ENOSYS as u64, 0xffff_ffff_ffff_ffda);
}

#[test]
fn strings_accept_all_utf8_widths_without_normalizing_or_nul_termination() {
    for text in [
        "",
        "Mirror",
        "\0after",
        "a\0b",
        "éλ中🙂",
        "\u{7f}\u{80}\u{7ff}\u{800}",
        "\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}",
        "e\u{301}",
    ] {
        assert_eq!(decode_utf8(text.as_bytes()), Ok(text));
    }
}

#[test]
fn strings_reject_invalid_utf8_including_invalid_tails_after_nul() {
    for bytes in [
        &b"\x80"[..],
        &b"\xc0\x80"[..],
        &b"\xc1\xbf"[..],
        &b"\xc2"[..],
        &b"\xe0\x80\x80"[..],
        &b"\xe2\x82"[..],
        &b"\xed\xa0\x80"[..],
        &b"\xf0\x80\x80\x80"[..],
        &b"\xf4\x90\x80\x80"[..],
        &b"\xf0\x9f\x99"[..],
        &b"\xff"[..],
        &b"valid\0\xff"[..],
    ] {
        assert_eq!(decode_utf8(bytes), Err(EINVAL), "{bytes:?}");
    }
}
