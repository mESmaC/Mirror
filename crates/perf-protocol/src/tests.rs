use super::*;
use std::{format, string::ToString, vec::Vec};

fn context() -> Context {
    Context {
        smoke: false,
        agent_demo: false,
        has_clock: true,
        has_display: true,
        has_session: true,
    }
}

fn boot_prefix(config: Config, context: Context) -> Vec<Record> {
    let ns = context.has_clock.then_some(20);
    let mut records = std::vec![
        Record::on(config),
        Record::Time {
            event: Event::Clock,
            ns
        }
    ];
    if context.has_display {
        records.push(Record::Time {
            event: Event::Display,
            ns,
        });
    }
    records.push(Record::Time {
        event: Event::Input,
        ns,
    });
    for item in Item::ALL {
        if let Some(reason) = context.skip(config, item) {
            records.push(Record::Skipped { item, reason });
        }
    }
    records
}

fn complete_log(config: Config, context: Context) -> Vec<Record> {
    let mut records = boot_prefix(config, context);
    let ns = context.has_clock.then_some(30);
    if context.has_session && !context.smoke {
        records.push(Record::Time {
            event: Event::Userspace,
            ns,
        });
        if config.mem {
            records.push(Record::Memory {
                ns,
                available_kib: Some(400),
                pool_kib: Some(800),
            });
        }
        records.push(Record::Time {
            event: Event::Frame,
            ns,
        });
        records.push(Record::Time {
            event: Event::ShellReady,
            ns,
        });
    }
    records
}

fn validate(config: Config, context: Context, records: &[Record]) -> Result<Validator, Error> {
    let mut validator = Validator::new(Expectation::Enabled(config), context);
    for &record in records {
        validator.push(record)?;
    }
    validator.finish()?;
    Ok(validator)
}

#[test]
fn grammar_valid_combinations_and_order_independence() {
    let all = parse_items("boot,1,burn,flip,telemetry,agent").unwrap();
    assert_eq!(
        all,
        Config {
            mem: true,
            input: true,
            burn: true,
            flip: true,
            telemetry: true,
            agent: true
        }
    );
    assert_eq!(
        all,
        parse_items("agent,telemetry,flip,burn,1,boot").unwrap()
    );
    assert_eq!(parse_items("boot"), Ok(Config::default()));
    assert_eq!(parse_items("boot,mem"), parse_items("mem"));
    assert_eq!(parse_items("boot,1"), parse_items("1"));
    assert!(parse_items("mem,flip").unwrap().mem);
    assert_eq!(
        parse_command_line(b"console=on\t\r\nmirror.perf=boot,1\x0b\x0cother=yes"),
        Ok(Some(parse_items("1").unwrap()))
    );
}

#[test]
fn grammar_rejects_empty_duplicate_unknown_conflicting_items() {
    for (items, expected) in [
        ("", Error::EmptyItem),
        (",boot", Error::EmptyItem),
        ("boot,", Error::EmptyItem),
        ("boot,,mem", Error::EmptyItem),
        ("boot,boot", Error::DuplicateItem),
        ("mem,mem", Error::DuplicateItem),
        ("1,1", Error::DuplicateItem),
        ("1,burn,burn", Error::DuplicateItem),
        ("flip,flip", Error::DuplicateItem),
        ("telemetry,telemetry", Error::DuplicateItem),
        ("agent,agent", Error::DuplicateItem),
        ("1,mem", Error::ConflictingItems),
        ("mem,1", Error::ConflictingItems),
        ("burn", Error::BurnWithoutInput),
        ("mem,burn", Error::BurnWithoutInput),
        ("BOOT", Error::UnknownItem),
        ("boot mem", Error::UnknownItem),
        ("boot\t", Error::UnknownItem),
        ("bogus", Error::UnknownItem),
        ("böot", Error::NonAscii),
    ] {
        assert_eq!(parse_items(items), Err(expected), "{items:?}");
    }
}

#[test]
fn grammar_token_boundary_presence_and_duplicate_tokens() {
    assert_eq!(parse_command_line(b""), Ok(None));
    assert_eq!(
        parse_command_line(b"mirror.perfX=1 Mirror.perf=1"),
        Ok(None)
    );
    assert_eq!(
        parse_command_line(b"mirror.perf=boot mirror.perf=mem"),
        Err(Error::DuplicateToken)
    );
    assert_eq!(parse_command_line(b"mirror.perf="), Err(Error::EmptyItem));
    assert_eq!(
        parse_command_line(b"mirror.perf=\xff"),
        Err(Error::NonAscii)
    );
    let at_limit = "x".repeat(MAX_TOKEN_BYTES - TOKEN_PREFIX.len());
    assert_eq!(parse_items(&at_limit), Err(Error::UnknownItem));
    assert_eq!(
        parse_command_line(format!("{TOKEN_PREFIX}{at_limit}").as_bytes()),
        Err(Error::UnknownItem)
    );
    let too_long = format!("{at_limit}x");
    assert_eq!(parse_items(&too_long), Err(Error::TooLong));
    assert_eq!(
        parse_command_line(format!("{TOKEN_PREFIX}{too_long}").as_bytes()),
        Err(Error::TooLong)
    );
}

#[test]
fn all_record_schemas_round_trip() {
    let mut records = std::vec![
        Record::on(parse_items("1,burn").unwrap()),
        Record::on(Config::default()),
        Record::Off,
        Record::Memory {
            ns: None,
            available_kib: None,
            pool_kib: None
        },
        Record::Memory {
            ns: Some(0),
            available_kib: Some(4),
            pool_kib: Some(800)
        },
    ];
    for event in [
        Event::Clock,
        Event::Display,
        Event::Input,
        Event::Userspace,
        Event::Frame,
        Event::ShellReady,
    ] {
        records.push(Record::Time {
            event,
            ns: Some(u64::MAX - 1),
        });
        records.push(Record::Time { event, ns: None });
    }
    for item in Item::ALL {
        for reason in [
            SkipReason::Smoke,
            SkipReason::AgentDemo,
            SkipReason::NoClock,
            SkipReason::NoSession,
            SkipReason::Unavailable,
        ] {
            records.push(Record::Skipped { item, reason });
        }
    }
    for record in records {
        assert_eq!(Record::parse(&record.to_string()), Ok(record));
    }
    assert_eq!(
        Record::Time {
            event: Event::Clock,
            ns: Some(17)
        }
        .to_string(),
        "MIRROR_PERF_T event=clock ns=17 since_entry_ns=none"
    );
}

#[test]
fn malformed_unknown_duplicate_or_reordered_fields_fail() {
    for line in [
        "MIRROR_PERF_MISSING event=clock ns=1",
        "MIRROR_PERF_INPUT seq=1",
        "MIRROR_PERF_WINDOW count=0",
        "MIRROR_PERF_BURN reason=0",
        "MIRROR_PERF_ON v=2 mem=0 input=0 burn=0",
        "MIRROR_PERF_ON v=1 input=0 mem=0 burn=0",
        "MIRROR_PERF_ON v=1 mem=0 input=0 burn=0 burn=0",
        "MIRROR_PERF_ON v=1 mem=0 input=1 burn=0",
        "MIRROR_PERF_ON v=1 mem=1 input=0 burn=1",
        "MIRROR_PERF_ON v=1 mem=true input=0 burn=0",
        "MIRROR_PERF_OFF reason=unknown",
        "MIRROR_PERF_T event=clock ns=1",
        "MIRROR_PERF_T event=clock ns=1 since_entry_ns=0",
        "MIRROR_PERF_T event=input ns=1 since_entry_ns=none",
        "MIRROR_PERF_T event=clock2 ns=1",
        "MIRROR_PERF_T event=frame ns=1 ns=1",
        "MIRROR_PERF_T event=frame ns=+1",
        "MIRROR_PERF_T event=frame ns=-1",
        "MIRROR_PERF_T event=frame ns=0x10",
        "MIRROR_PERF_T event=frame ns=",
        "MIRROR_PERF_T event=frame ns=1.0",
        "MIRROR_PERF_T event=frame ns=١",
        "MIRROR_PERF_T  event=frame ns=1",
        "MIRROR_PERF_T event=frame ns=1 ",
        " MIRROR_PERF_T event=frame ns=1",
        "MIRROR_PERF_T\tevent=frame ns=1",
        "MIRROR_PERF_T event=frame ns=1\r",
        "MIRROR_PERF_MEM ns=1 available_kib=none pool_kib=800",
        "MIRROR_PERF_MEM ns=1 available_kib=804 pool_kib=800",
        "MIRROR_PERF_MEM ns=1 available_kib=3 pool_kib=800",
        "MIRROR_PERF_SKIPPED item=boot reason=unavailable",
        "MIRROR_PERF_SKIPPED item=burn reason=create_failed",
    ] {
        assert!(Record::parse(line).is_err(), "{line:?}");
    }
    assert_eq!(
        Record::parse("MIRROR_PERF_T event=frame ns=0001"),
        Ok(Record::Time {
            event: Event::Frame,
            ns: Some(1)
        })
    );
}

#[test]
fn overflow_saturation_and_memory_arithmetic() {
    for number in [
        "18446744073709551615",
        "18446744073709551616",
        "9999999999999999999999999",
    ] {
        assert_eq!(
            Record::parse(&format!("MIRROR_PERF_T event=frame ns={number}")),
            Err(Error::Overflow)
        );
    }
    assert_eq!(memory_kib(3, 7, 20), Ok((40, 80)));
    assert_eq!(memory_kib(u64::MAX, 1, 20), Err(Error::Overflow));
    assert_eq!(
        memory_kib(u64::MAX / 4 + 1, 0, u64::MAX / 4),
        Err(Error::Overflow)
    );
    assert_eq!(memory_kib(0, 0, u64::MAX / 4 + 1), Err(Error::Overflow));
    assert_eq!(memory_kib(3, 7, 9), Err(Error::MemoryBounds));
    assert_eq!(memory_kib(0, 0, 0), Ok((0, 0)));
}

#[test]
fn skips_apply_exact_precedence_and_reservations() {
    let config = parse_items("1,burn,flip,telemetry,agent").unwrap();
    let all_absent = Context {
        smoke: true,
        agent_demo: true,
        has_clock: false,
        has_display: false,
        has_session: false,
    };
    for item in [Item::Input, Item::Mem, Item::Burn] {
        assert_eq!(all_absent.skip(config, item), Some(SkipReason::Smoke));
    }
    for item in [Item::Flip, Item::Telemetry, Item::Agent] {
        assert_eq!(all_absent.skip(config, item), Some(SkipReason::Unavailable));
    }
    let demo = Context {
        smoke: false,
        ..all_absent
    };
    assert_eq!(demo.skip(config, Item::Input), Some(SkipReason::AgentDemo));
    assert_eq!(demo.skip(config, Item::Burn), Some(SkipReason::AgentDemo));
    assert_eq!(demo.skip(config, Item::Mem), Some(SkipReason::NoClock));
    let no_clock = Context {
        agent_demo: false,
        ..demo
    };
    assert_eq!(
        no_clock.skip(config, Item::Input),
        Some(SkipReason::NoClock)
    );
    let no_session = Context {
        has_clock: true,
        ..no_clock
    };
    assert_eq!(
        no_session.skip(config, Item::Input),
        Some(SkipReason::NoSession)
    );
    assert_eq!(
        no_session.skip(config, Item::Mem),
        Some(SkipReason::NoSession)
    );
    assert_eq!(
        context().skip(config, Item::Input),
        Some(SkipReason::Unavailable)
    );
    assert_eq!(context().skip(config, Item::Mem), None);
    assert_eq!(
        context().skip(parse_items("boot").unwrap(), Item::Input),
        None
    );
}

#[test]
fn complete_logs_cover_smoke_missing_display_no_devices_and_no_clock() {
    let config = parse_items("1,burn,flip,telemetry,agent").unwrap();
    for context in [
        context(),
        Context {
            smoke: true,
            ..context()
        },
        Context {
            agent_demo: true,
            ..context()
        },
        Context {
            has_clock: false,
            ..context()
        },
        Context {
            has_session: false,
            ..context()
        },
        Context {
            has_display: false,
            has_session: false,
            ..context()
        },
        Context {
            has_clock: false,
            has_session: false,
            ..context()
        },
        Context {
            has_clock: false,
            has_display: false,
            has_session: false,
            ..context()
        },
        Context {
            agent_demo: true,
            has_clock: false,
            ..context()
        },
    ] {
        let log = complete_log(config, context);
        validate(config, context, &log).unwrap();
        if !context.has_session || context.smoke {
            assert!(
                !log.iter()
                    .any(|record| matches!(record, Record::Memory { .. }))
            );
        }
    }
}

#[test]
fn order_duplicates_and_missing_records_are_rejected() {
    let config = parse_items("1,burn").unwrap();
    let good = complete_log(config, context());
    for index in 0..good.len() {
        let mut duplicate = good.clone();
        duplicate.insert(index, good[index]);
        assert!(
            validate(config, context(), &duplicate).is_err(),
            "duplicate {index}"
        );
        let mut missing = good.clone();
        missing.remove(index);
        assert!(
            validate(config, context(), &missing).is_err(),
            "missing {index}"
        );
    }
    let mut wrong = good.clone();
    wrong.swap(2, 3);
    assert_eq!(
        validate(config, context(), &wrong).unwrap_err(),
        Error::Order
    );
    wrong = good;
    wrong.swap(4, 5);
    assert_eq!(
        validate(config, context(), &wrong).unwrap_err(),
        Error::Order
    );
}

#[test]
fn memory_spacing_and_milestones_have_independent_time_order() {
    let config = parse_items("mem").unwrap();
    let mut validator = validate(config, context(), &complete_log(config, context())).unwrap();
    let sample = |ns| Record::Memory {
        ns: Some(ns),
        available_kib: Some(380),
        pool_kib: Some(800),
    };
    assert_eq!(
        validator.push(sample(MEMORY_INTERVAL_NS + 29)),
        Err(Error::MemorySpacing)
    );
    assert_eq!(validator.memory_samples(), 1);
    validator.push(sample(MEMORY_INTERVAL_NS + 30)).unwrap();
    assert_eq!(validator.push(sample(10)), Err(Error::MemorySpacing));
    validator.push(sample(3 * MEMORY_INTERVAL_NS + 30)).unwrap();
    assert_eq!(validator.memory_samples(), 3);
    validator.finish().unwrap();

    // Buffered family timestamps need not be globally ordered: a valid MEM
    // snapshot older than the final milestone remains valid as its first MEM.
    let mut records = complete_log(config, context());
    records.retain(|record| !matches!(record, Record::Memory { .. }));
    records.push(sample(25));
    validate(config, context(), &records).unwrap();
}

#[test]
fn no_clock_retains_one_initial_memory_but_no_session_retains_none() {
    let config = parse_items("1,burn").unwrap();
    let no_clock = Context {
        has_clock: false,
        ..context()
    };
    let mut validator = validate(config, no_clock, &complete_log(config, no_clock)).unwrap();
    let sample = Record::Memory {
        ns: None,
        available_kib: None,
        pool_kib: None,
    };
    assert_eq!(validator.memory_samples(), 1);
    assert_eq!(validator.push(sample), Err(Error::MemorySpacing));
    let neither = Context {
        has_session: false,
        has_display: false,
        ..no_clock
    };
    let mut validator = validate(config, neither, &complete_log(config, neither)).unwrap();
    assert_eq!(validator.push(sample), Err(Error::UnexpectedRecord));
}

#[test]
fn timestamp_monotonicity_and_availability_are_checked() {
    let config = parse_items("boot").unwrap();
    let mut validator = Validator::new(Expectation::Enabled(config), context());
    validator.push(Record::on(config)).unwrap();
    assert_eq!(
        validator.push(Record::Time {
            event: Event::Clock,
            ns: None
        }),
        Err(Error::Timestamp)
    );
    validator
        .push(Record::Time {
            event: Event::Clock,
            ns: Some(100),
        })
        .unwrap();
    assert_eq!(
        validator.push(Record::Time {
            event: Event::Display,
            ns: Some(99)
        }),
        Err(Error::Timestamp)
    );
    validator
        .push(Record::Time {
            event: Event::Display,
            ns: Some(100),
        })
        .unwrap();
    assert_eq!(
        validator.push(Record::Time {
            event: Event::Input,
            ns: Some(u64::MAX)
        }),
        Err(Error::Timestamp)
    );
}

#[test]
fn tokenless_invalid_and_fatal_prefixes_are_distinct() {
    let mut tokenless = Validator::new(Expectation::Tokenless, context());
    tokenless.finish().unwrap();
    assert_eq!(tokenless.push(Record::Off), Err(Error::UnexpectedRecord));
    let mut invalid = Validator::new(Expectation::Invalid, context());
    assert_eq!(invalid.finish(), Err(Error::MissingRecord));
    invalid.push(Record::Off).unwrap();
    invalid.finish().unwrap();
    assert_eq!(invalid.push(Record::Off), Err(Error::UnexpectedRecord));

    let config = parse_items("boot").unwrap();
    let mut prefix = Validator::new(Expectation::Enabled(config), context());
    prefix.push(Record::on(config)).unwrap();
    prefix
        .push(Record::Time {
            event: Event::Clock,
            ns: Some(10),
        })
        .unwrap();
    prefix.finish_prefix().unwrap();
    assert_eq!(prefix.finish(), Err(Error::MissingRecord));
}

#[test]
fn skip_reason_must_match_context_and_precede_userspace() {
    let config = parse_items("1,burn").unwrap();
    let mut records = complete_log(config, context());
    records[4] = Record::Skipped {
        item: Item::Input,
        reason: SkipReason::NoClock,
    };
    assert_eq!(
        validate(config, context(), &records).unwrap_err(),
        Error::Order
    );
    let mut validator = validate(config, context(), &complete_log(config, context())).unwrap();
    assert_eq!(
        validator.push(Record::Skipped {
            item: Item::Burn,
            reason: SkipReason::Unavailable
        }),
        Err(Error::Order)
    );
}
