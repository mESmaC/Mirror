use super::*;

fn replace(id: u32, revision: u64, start: u8, end: u8, text: &[u8]) -> Request {
    Request {
        id,
        command: Command::Replace {
            expected_revision: revision,
            start,
            end,
            text: Chunk::new(text).unwrap(),
        },
    }
}

#[test]
fn all_request_and_response_shapes_round_trip() {
    let requests = [
        Request {
            id: 1,
            command: Command::Discover,
        },
        Request {
            id: 2,
            command: Command::Inspect,
        },
        Request {
            id: 3,
            command: Command::Read {
                target: TextTarget::Draft,
                offset: 0,
                length: 32,
            },
        },
        Request {
            id: 4,
            command: Command::Read {
                target: TextTarget::Applied,
                offset: 0,
                length: 1,
            },
        },
        replace(5, 0, 0, 0, b"Hello, world!"),
        Request {
            id: 6,
            command: Command::Apply {
                expected_revision: 1,
            },
        },
        replace(7, 0, 0, 0, b"stale"),
    ];
    let mut editor = Editor::new();
    for request in requests {
        let bytes = request.encode().unwrap();
        assert_eq!(Request::decode(&bytes), Ok(request));
        let (response, _) = editor.handle_request(request);
        let bytes = response.encode().unwrap();
        assert_eq!(bytes[19], CAPACITY as u8);
        assert_eq!(bytes[20], CHUNK_BYTES as u8);
        assert_eq!(bytes[23], SUPPORTED_OPERATIONS);
        assert_eq!(Response::decode(&bytes), Ok(response));
    }
    assert_eq!(editor.applied(), b"Hello, world!");
}

#[test]
fn malformed_requests_reject_length_version_opcode_padding_and_text() {
    let original = replace(17, 0, 0, 0, b"a").encode().unwrap();
    assert_eq!(Request::decode(&original[..63]), Err(WireError::Length));
    for (offset, value) in [
        (0, 2),
        (1, 0),
        (1, 7),
        (1, 0x85),
        (2, 1),
        (3, 1),
        (16, 1),
        (17, 97),
        (18, 33),
        (19, 1),
        (24, 1),
        (31, 1),
        (32, 31),
        (32, 127),
        (33, 1),
        (63, 1),
    ] {
        let mut bytes = original;
        bytes[offset] = value;
        assert!(
            Request::decode(&bytes).is_err(),
            "offset {offset}, value {value}"
        );
    }
    let inspect = Request {
        id: 1,
        command: Command::Inspect,
    }
    .encode()
    .unwrap();
    for offset in 8..64 {
        let mut bytes = inspect;
        bytes[offset] = 1;
        assert!(Request::decode(&bytes).is_err(), "inspect offset {offset}");
    }
    for length in [0, 33, 255] {
        assert!(
            Request {
                id: 1,
                command: Command::Read {
                    target: TextTarget::Draft,
                    offset: 0,
                    length
                }
            }
            .encode()
            .is_err()
        );
    }
}

#[test]
fn malformed_responses_cannot_claim_impossible_snapshots() {
    let mut editor = Editor::new();
    let (response, _) = editor.handle_request(replace(1, 0, 0, 0, b"a"));
    let original = response.encode().unwrap();
    assert!(Response::decode(&original[..63]).is_err());
    for (offset, value) in [
        (0, 2),
        (1, 5),
        (1, 0x87),
        (2, 8),
        (3, 2),
        (16, 97),
        (17, 1),
        (18, 2),
        (19, 95),
        (20, 31),
        (21, 1),
        (22, 1),
        (23, 0),
        (24, 1),
        (32, 1),
        (63, 1),
    ] {
        let mut bytes = original;
        bytes[offset] = value;
        assert!(
            Response::decode(&bytes).is_err(),
            "offset {offset}, value {value}"
        );
    }
    for status in [
        Status::Malformed,
        Status::StaleRevision,
        Status::InvalidRange,
        Status::InvalidText,
        Status::CapacityExceeded,
        Status::RevisionExhausted,
        Status::Denied,
    ] {
        let response = Response {
            opcode: if status == Status::Malformed {
                Opcode::Invalid
            } else {
                Opcode::Apply
            },
            status,
            ..response
        };
        assert_eq!(Response::decode(&response.encode().unwrap()), Ok(response));
    }
}

#[test]
fn chunks_cover_full_capacity_and_rejected_edits_are_atomic() {
    let mut editor = Editor::new();
    for index in 0..3 {
        let text = [b'a' + index; CHUNK_BYTES];
        let start = index * CHUNK_BYTES as u8;
        let (response, _) =
            editor.handle_request(replace(index as u32, index as u64, start, start, &text));
        assert_eq!(response.status, Status::Ok);
    }
    assert_eq!(editor.draft().len(), CAPACITY);
    let before = editor.clone();
    for operation in [
        Operation::Replace {
            start: 96,
            end: 96,
            text: b"!",
        },
        Operation::Replace {
            start: 97,
            end: 97,
            text: b"",
        },
        Operation::Replace {
            start: 2,
            end: 1,
            text: b"",
        },
        Operation::Replace {
            start: 0,
            end: 1,
            text: b"\n",
        },
        Operation::SetCaret(97),
    ] {
        assert!(editor.dispatch(None, operation).is_err());
        assert_eq!(editor, before);
    }
    for offset in [0, 32, 64, 96] {
        let (response, change) = editor.handle_request(Request {
            id: offset as u32,
            command: Command::Read {
                target: TextTarget::Draft,
                offset,
                length: 32,
            },
        });
        assert_eq!(response.status, Status::Ok);
        assert_eq!(change, Change::default());
        let expected =
            &editor.draft()[usize::from(offset)..(usize::from(offset) + 32).min(CAPACITY)];
        assert_eq!(response.read.unwrap().text.as_bytes(), expected);
        assert_eq!(Response::decode(&response.encode().unwrap()), Ok(response));
    }
    assert_eq!(editor, before);
}

#[test]
fn human_changes_invalidate_agent_revision_without_overwriting_either_note() {
    let mut editor = Editor::new();
    editor.handle_request(replace(1, 0, 0, 0, b"agent draft"));
    editor.dispatch(None, Operation::Apply).unwrap();
    let observed = editor.snapshot().revision;
    editor
        .dispatch(
            None,
            Operation::Replace {
                start: 0,
                end: 5,
                text: b"human",
            },
        )
        .unwrap();
    let before = editor.clone();
    for request in [
        replace(2, observed, 0, 5, b"agent"),
        Request {
            id: 3,
            command: Command::Apply {
                expected_revision: observed,
            },
        },
    ] {
        let (response, change) = editor.handle_request(request);
        assert_eq!(response.status, Status::StaleRevision);
        assert_eq!(response.snapshot, before.snapshot());
        assert_eq!(change, Change::default());
        assert_eq!(editor, before);
    }
    assert_eq!(editor.draft(), b"human draft");
    assert_eq!(editor.applied(), b"agent draft");
}

#[test]
fn human_and_wire_operations_have_identical_effects_and_loss_preserves_applied() {
    let mut human = Editor::new();
    let mut agent = Editor::new();
    human
        .dispatch(
            None,
            Operation::Replace {
                start: 0,
                end: 0,
                text: b"abcd",
            },
        )
        .unwrap();
    agent.handle_request(replace(1, 0, 0, 0, b"abcd"));
    human
        .dispatch(
            None,
            Operation::Replace {
                start: 2,
                end: 2,
                text: b"X",
            },
        )
        .unwrap();
    agent.handle_request(replace(2, 1, 2, 2, b"X"));
    human.dispatch(None, Operation::Apply).unwrap();
    agent.handle_request(Request {
        id: 3,
        command: Command::Apply {
            expected_revision: 2,
        },
    });
    assert_eq!(human, agent);
    human.dispatch(None, Operation::ClearDraft).unwrap();
    assert!(human.draft().is_empty());
    assert_eq!(human.applied(), b"abXcd");
    assert_eq!(human.snapshot().revision, 4);
}

#[test]
fn revision_overflow_never_mutates_and_caret_invalidates_observation() {
    let mut editor = Editor::new();
    editor.handle_request(replace(1, 0, 0, 0, b"note"));
    let observed = editor.snapshot().revision;
    editor.dispatch(None, Operation::SetCaret(0)).unwrap();
    assert_eq!(
        editor.dispatch(Some(observed), Operation::Apply),
        Err(Status::StaleRevision)
    );
    editor.revision = u64::MAX;
    let before = editor.clone();
    for operation in [
        Operation::Apply,
        Operation::ClearDraft,
        Operation::SetCaret(1),
        Operation::Replace {
            start: 0,
            end: 0,
            text: b"!",
        },
    ] {
        assert_eq!(
            editor.dispatch(Some(u64::MAX), operation),
            Err(Status::RevisionExhausted)
        );
        assert_eq!(editor, before);
    }
}

#[test]
fn shortening_and_deleting_clear_old_data_and_empty_apply_is_valid() {
    let mut editor = Editor::new();
    editor.dispatch(None, Operation::Apply).unwrap();
    assert!(editor.snapshot().has_applied);
    editor.handle_request(replace(1, 1, 0, 0, b"abcdef"));
    editor.handle_request(replace(2, 2, 1, 5, b"x"));
    assert_eq!(editor.draft(), b"axf");
    assert!(editor.draft[3..].iter().all(|byte| *byte == 0));
    editor.handle_request(replace(3, 3, 0, 3, b""));
    assert_eq!(editor.draft(), b"");
    assert!(editor.draft.iter().all(|byte| *byte == 0));
}
