//! Fixed-size process handle tables with globally unique, attenuable tokens.
//! Object lifetimes belong to the caller; this module neither grants nor drops
//! references to the underlying object when a table entry changes. Input and
//! surface sessions outlive all their handles and keep exclusive device borrows.

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const SEND: u64 = 1;
pub(crate) const RECEIVE: u64 = 2;
pub(crate) const WRITE: u64 = 4;
const KNOWN_RIGHTS: u64 = SEND | RECEIVE | WRITE;
const TABLE_CAPACITY: usize = 8;
// Preserve the positive signed range for syscall results; negative values are
// reserved for errno. MAX_TOKEN + 1 is a sticky exhausted allocator state.
const MAX_TOKEN: u64 = i64::MAX as u64;
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Object {
    Console,
    /// Only init's smoke pair holds channels until a spawn call can grant
    /// them (Phase 3); the agent-demo session that used them retired (RS6).
    #[cfg_attr(
        not(feature = "boot-smoke"),
        expect(dead_code, reason = "only boot-smoke sessions grant channels")
    )]
    Channel(usize),
    /// The single borrowed graphics surface lives for the complete run session.
    Surface,
    /// The desktop alone receives the borrowed, ordered input stream.
    Input,
    Supervisor,
    Application(u64),
    AppSurface(u64),
    AppInput(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Capability {
    pub object: Object,
    pub rights: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapError {
    BadHandle,
    Denied,
    InvalidRights,
    TableFull,
    IdsExhausted,
}

#[derive(Clone, Copy)]
struct Entry {
    token: u64,
    capability: Capability,
}

pub(crate) struct HandleTable {
    entries: [Option<Entry>; TABLE_CAPACITY],
}

impl HandleTable {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; TABLE_CAPACITY],
        }
    }

    pub(crate) fn insert(&mut self, object: Object, rights: u64) -> Result<u64, CapError> {
        let permitted = match object {
            Object::Console
            | Object::Surface
            | Object::Supervisor
            | Object::Application(_)
            | Object::AppSurface(_) => WRITE,
            Object::Channel(_) => SEND | RECEIVE,
            Object::Input | Object::AppInput(_) => RECEIVE,
        };
        if rights & !permitted != 0 {
            return Err(CapError::InvalidRights);
        }
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or(CapError::TableFull)?;
        let token = allocate_token(&NEXT_TOKEN)?;
        self.entries[slot] = Some(Entry {
            token,
            capability: Capability { object, rights },
        });
        Ok(token)
    }

    pub(crate) fn get(&self, handle: u64) -> Result<Capability, CapError> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.token == handle)
            .map(|entry| entry.capability)
            .ok_or(CapError::BadHandle)
    }

    pub(crate) fn require(&self, handle: u64, rights: u64) -> Result<Capability, CapError> {
        let capability = self.get(handle)?;
        if rights & !KNOWN_RIGHTS != 0 {
            return Err(CapError::InvalidRights);
        }
        if capability.rights & rights != rights {
            return Err(CapError::Denied);
        }
        Ok(capability)
    }

    pub(crate) fn duplicate(
        &mut self,
        handle: u64,
        rights: u64,
    ) -> Result<(u64, Capability), CapError> {
        let original = self.require(handle, rights)?;
        let token = self.insert(original.object, rights)?;
        Ok((
            token,
            Capability {
                object: original.object,
                rights,
            },
        ))
    }

    pub(crate) fn close(&mut self, handle: u64) -> Result<Capability, CapError> {
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_some_and(|entry| entry.token == handle))
            .ok_or(CapError::BadHandle)?;
        Ok(slot.take().expect("selected live handle").capability)
    }

    /// Remove one entry for owner-driven object-reference cleanup on teardown.
    pub(crate) fn take_one(&mut self) -> Option<Capability> {
        self.entries
            .iter_mut()
            .find_map(|entry| entry.take().map(|entry| entry.capability))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// Revoke every duplicate when a supervisor closes a particular instance.
    /// Application controls own no channel references; PIDs are never reused.
    pub(crate) fn revoke_application(&mut self, pid: u64) {
        for entry in &mut self.entries {
            if entry.is_some_and(|entry| entry.capability.object == Object::Application(pid)) {
                *entry = None;
            }
        }
    }

    #[cfg(feature = "boot-smoke")]
    pub(crate) fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }
}

fn allocate_token(next: &AtomicU64) -> Result<u64, CapError> {
    // Relaxed ordering supplies uniqueness without implying publication of a
    // table entry. Table access itself requires the caller's synchronization.
    next.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        if (1..=MAX_TOKEN).contains(&value) {
            Some(value + 1)
        } else {
            None
        }
    })
    .map_err(|_| CapError::IdsExhausted)
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    let mut owner = HandleTable::new();
    let mut other = HandleTable::new();
    assert!(owner.is_empty());
    assert_eq!(owner.len(), 0);
    assert_eq!(owner.get(0), Err(CapError::BadHandle));
    assert_eq!(owner.get(u64::MAX), Err(CapError::BadHandle));
    let inert_console = owner.insert(Object::Console, 0).unwrap();
    assert_eq!(owner.require(inert_console, WRITE), Err(CapError::Denied));
    assert_eq!(owner.close(inert_console).unwrap().rights, 0);
    assert_eq!(
        owner.insert(Object::Console, SEND),
        Err(CapError::InvalidRights)
    );
    assert_eq!(
        owner.insert(Object::Channel(0), WRITE),
        Err(CapError::InvalidRights)
    );
    assert_eq!(
        owner.insert(Object::Channel(0), 8),
        Err(CapError::InvalidRights)
    );
    let original = owner.insert(Object::Channel(0), SEND | RECEIVE).unwrap();
    let foreign = other.insert(Object::Channel(0), SEND | RECEIVE).unwrap();
    assert_ne!(original, foreign);
    assert_eq!(owner.get(foreign), Err(CapError::BadHandle));
    assert_eq!(other.get(original), Err(CapError::BadHandle));
    assert_eq!(owner.require(foreign, SEND), Err(CapError::BadHandle));
    assert_eq!(owner.duplicate(foreign, SEND), Err(CapError::BadHandle));
    assert_eq!(owner.close(foreign), Err(CapError::BadHandle));
    let (send_only, sent) = owner.duplicate(original, SEND).unwrap();
    assert_eq!(
        sent,
        Capability {
            object: Object::Channel(0),
            rights: SEND
        }
    );
    assert_eq!(owner.require(send_only, SEND), Ok(sent));
    assert_eq!(owner.require(send_only, RECEIVE), Err(CapError::Denied));
    assert_eq!(
        owner.duplicate(send_only, SEND | RECEIVE),
        Err(CapError::Denied)
    );
    assert_eq!(owner.duplicate(send_only, WRITE), Err(CapError::Denied));
    let (inert, inert_capability) = owner.duplicate(send_only, 0).unwrap();
    assert_eq!(inert_capability.rights, 0);
    assert_eq!(owner.get(inert), Ok(inert_capability));
    assert_eq!(owner.require(inert, 0), Ok(inert_capability));
    assert_eq!(owner.require(inert, SEND), Err(CapError::Denied));
    assert_eq!(owner.duplicate(inert, SEND), Err(CapError::Denied));
    assert_eq!(owner.duplicate(original, 8), Err(CapError::InvalidRights));
    let (same_rights, copy) = owner.duplicate(send_only, SEND).unwrap();
    assert!(same_rights > send_only);
    assert_eq!(copy, sent);
    assert_eq!(owner.close(original).unwrap().rights, SEND | RECEIVE);
    assert_eq!(owner.get(original), Err(CapError::BadHandle));
    assert_eq!(owner.close(original), Err(CapError::BadHandle));
    let replacement = owner.insert(Object::Console, WRITE).unwrap();
    assert!(replacement > same_rights);
    assert_eq!(owner.get(original), Err(CapError::BadHandle));
    while owner.len() < TABLE_CAPACITY {
        owner.insert(Object::Console, WRITE).unwrap();
    }
    assert_eq!(
        owner.insert(Object::Console, WRITE),
        Err(CapError::TableFull)
    );
    assert_eq!(owner.duplicate(send_only, SEND), Err(CapError::TableFull));
    assert_eq!(owner.get(send_only), Ok(sent));
    let mut removed = 0;
    while owner.take_one().is_some() {
        removed += 1;
    }
    assert_eq!(removed, TABLE_CAPACITY);
    assert!(owner.is_empty());
    assert_eq!(owner.take_one(), None);
    assert_eq!(owner.get(replacement), Err(CapError::BadHandle));

    // Surface authority is scoped like any other object and permits only WRITE.
    // An inert duplicate retains object identity without granting info or paint.
    for rights in [SEND, RECEIVE, SEND | WRITE, 8] {
        assert_eq!(
            owner.insert(Object::Surface, rights),
            Err(CapError::InvalidRights)
        );
    }
    let surface = owner.insert(Object::Surface, WRITE).unwrap();
    assert_eq!(
        owner.require(surface, WRITE).unwrap().object,
        Object::Surface
    );
    assert_eq!(other.get(surface), Err(CapError::BadHandle));
    let (inert_surface, inert) = owner.duplicate(surface, 0).unwrap();
    assert_eq!(inert.object, Object::Surface);
    assert_eq!(owner.require(inert_surface, WRITE), Err(CapError::Denied));
    assert_eq!(owner.duplicate(inert_surface, WRITE), Err(CapError::Denied));
    assert_eq!(owner.duplicate(surface, SEND), Err(CapError::Denied));
    owner.close(surface).unwrap();
    owner.close(inert_surface).unwrap();
    assert_eq!(owner.require(surface, WRITE), Err(CapError::BadHandle));
    assert!(owner.is_empty());

    for rights in [SEND, WRITE, RECEIVE | WRITE] {
        assert_eq!(
            owner.insert(Object::Input, rights),
            Err(CapError::InvalidRights)
        );
    }
    let input = owner.insert(Object::Input, RECEIVE).unwrap();
    assert_eq!(owner.require(input, RECEIVE).unwrap().object, Object::Input);
    assert_eq!(other.get(input), Err(CapError::BadHandle));
    let (inert_input, inert) = owner.duplicate(input, 0).unwrap();
    assert_eq!(inert.object, Object::Input);
    assert_eq!(owner.require(inert_input, RECEIVE), Err(CapError::Denied));
    assert_eq!(owner.duplicate(inert_input, RECEIVE), Err(CapError::Denied));
    owner.close(input).unwrap();
    owner.close(inert_input).unwrap();
    assert_eq!(owner.get(input), Err(CapError::BadHandle));
    assert!(owner.is_empty());

    // Do not exhaust the real boot-wide allocator; later processes need tokens.
    let near_end = AtomicU64::new(MAX_TOKEN);
    assert_eq!(allocate_token(&near_end), Ok(MAX_TOKEN));
    assert_eq!(allocate_token(&near_end), Err(CapError::IdsExhausted));
    assert_eq!(allocate_token(&near_end), Err(CapError::IdsExhausted));
    assert_eq!(near_end.load(Ordering::Relaxed), MAX_TOKEN + 1);
    assert_eq!(
        allocate_token(&AtomicU64::new(u64::MAX)),
        Err(CapError::IdsExhausted)
    );
    crate::serial::println!("MIRROR_CAPABILITY_TESTS_OK");
}
