//! Acquire-only machine leases and exact-run recovery (0015c VMR-D6).
//!
//! Only `publish_release` publishes release; Drop merely closes handles. The caller
//! must durably persist an eligible final state and release intent first, then persist
//! the returned owner-record identity before closing or detaching this lease.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
#[path = "lease_windows.rs"]
mod platform;
#[cfg(windows)]
pub use platform::Identity;

#[cfg(not(windows))]
mod platform {
    use super::*;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Identity;
    impl std::fmt::Display for Identity {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unsupported")
        }
    }
    pub fn identity(_: &File) -> io::Result<Identity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "leases require Windows identity handles",
        ))
    }
    pub fn directory(_: &Path) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "leases require Windows identity handles",
        ))
    }
    pub fn owner(_: &Path, _: bool) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "leases require Windows identity handles",
        ))
    }
    pub fn rename(_: &File, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "leases require Windows identity handles",
        ))
    }
    pub fn verify_owner(_: &File, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "leases require Windows identity handles",
        ))
    }
}
#[cfg(not(windows))]
pub use platform::Identity;

const OWNER_CAP: usize = 16 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn busy(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub nick: String,
    pub task: String,
    pub worktree: PathBuf,
    pub started: String,
    pub kind: String,
    pub run: String,
    pub lease: String,
    pub lane_root: PathBuf,
}

impl Owner {
    pub fn operator(&self) -> String {
        format!("{}/{}", self.nick, self.task)
    }
    fn validate(&self) -> io::Result<()> {
        for value in [&self.nick, &self.task, &self.kind, &self.run, &self.lease] {
            valid_value(value)?;
        }
        if !matches!(self.kind.as_str(), "vm" | "package")
            || !crate::paths::safe_id(&self.run)
            || !crate::paths::safe_id(&self.lease)
        {
            return Err(invalid("invalid lane kind, run or lease identifier"));
        }
        if !utc(&self.started) {
            return Err(invalid("started must be a full UTC timestamp"));
        }
        valid_value(&path_text(&self.worktree)?)?;
        valid_value(&path_text(&self.lane_root)?)?;
        if !self.worktree.is_absolute() || !self.lane_root.is_absolute() {
            return Err(invalid("owner paths must be absolute"));
        }
        Ok(())
    }
    fn record(&self) -> io::Result<String> {
        self.validate()?;
        Ok(format!(
            "nick={}\ntask={}\nworktree={}\nstarted={}\nlane_schema=1\nlane_kind={}\nlane_run={}\nlease={}\nlane_root={}\n",
            self.nick,
            self.task,
            path_text(&self.worktree)?,
            self.started,
            self.kind,
            self.run,
            self.lease,
            path_text(&self.lane_root)?
        ))
    }
}

/// Capability supplied only after the caller verifies and durably records Cameron's
/// explicit authorization. Arbitrary CLI text must never create this capability.
#[derive(Clone, Debug)]
pub struct Replacement {
    pub run: String,
    pub lane_root: PathBuf,
    pub original_operator: String,
    pub replacement_operator: String,
    pub scope: String,
    pub authorization_reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRecord {
    pub slot: PathBuf,
    pub directory_identity: Identity,
    pub owner_identity: Identity,
    pub run: String,
    pub lease: String,
    pub released: String,
}

/// An abandoned detached generation is evidence, never a reason to reuse its lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detached {
    pub path: PathBuf,
    pub identity: Identity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachPhase {
    Intent,
    Detached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetachEvent {
    pub generation: Detached,
    pub phase: DetachPhase,
}

pub struct Lease {
    slot: PathBuf,
    directory: File,
    owner_file: Option<File>,
    owner: Owner,
    directory_identity: Identity,
    owner_identity: Identity,
    released: Option<ReleaseRecord>,
    detached_before_acquire: Vec<Detached>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("slot", &self.slot)
            .field("owner", &self.owner)
            .field("identity", &self.directory_identity)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// New work never adopts an unreleased generation, even with the same operator.
    /// A released generation may be detached, followed by a fresh atomic acquisition.
    pub fn acquire(slot: &Path, owner: Owner) -> io::Result<Self> {
        Self::acquire_with_observer(slot, owner, |_| Ok(()))
    }

    /// Production controllers durably record each event. Intent is emitted before
    /// rename, so a crash or a lost subsequent acquisition cannot hide the sibling.
    pub fn acquire_with_observer(
        slot: &Path,
        owner: Owner,
        mut observer: impl FnMut(&DetachEvent) -> io::Result<()>,
    ) -> io::Result<Self> {
        Self::obtain(slot, owner, false, None, &mut observer)
    }

    /// Continue only an exact retained run with no active holder. A released record
    /// always requires a new generation and fresh caller-provided lease identifier.
    pub fn recover(
        slot: &Path,
        owner: Owner,
        replacement: Option<&Replacement>,
    ) -> io::Result<Self> {
        Self::recover_with_observer(slot, owner, replacement, |_| Ok(()))
    }

    pub fn recover_with_observer(
        slot: &Path,
        owner: Owner,
        replacement: Option<&Replacement>,
        mut observer: impl FnMut(&DetachEvent) -> io::Result<()>,
    ) -> io::Result<Self> {
        Self::obtain(slot, owner, true, replacement, &mut observer)
    }

    fn obtain(
        slot: &Path,
        owner: Owner,
        recovery: bool,
        replacement: Option<&Replacement>,
        observer: &mut dyn FnMut(&DetachEvent) -> io::Result<()>,
    ) -> io::Result<Self> {
        owner.validate()?;
        if !cfg!(windows) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "leases require Windows identity handles",
            ));
        }
        let initial_record = owner.record()?;
        if initial_record.len() > OWNER_CAP {
            return Err(invalid("owner record exceeds cap"));
        }
        let slot = crate::paths::resolve_no_reparse(slot)?;
        let parent = slot
            .parent()
            .ok_or_else(|| invalid("lease slot has no parent"))?;
        crate::paths::canonical_no_reparse(parent)?;
        let mut detached = Vec::new();
        // Competing fresh acquisitions can win after detach; bounded attempts never wait.
        for _ in 0..4 {
            match fs::create_dir(&slot) {
                Ok(()) => {
                    let mut owner_file = platform::owner(&slot.join("owner.txt"), true)?;
                    let directory = platform::directory(&slot)?;
                    let directory_identity = platform::identity(&directory)?;
                    platform::verify_owner(&owner_file, &slot.join("owner.txt"))?;
                    let owner_identity = platform::identity(&owner_file)?;
                    owner_file.write_all(initial_record.as_bytes())?;
                    owner_file.sync_all()?;
                    return Ok(Self {
                        slot,
                        directory,
                        owner_file: Some(owner_file),
                        owner,
                        directory_identity,
                        owner_identity,
                        released: None,
                        detached_before_acquire: detached,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            // Owner-first locking lets the creator finish initializing a fresh mkdir;
            // an observer cannot briefly take its directory guard and strand it.
            let mut owner_file = platform::owner(&slot.join("owner.txt"), false)
                .map_err(|error| busy(&format!("owner record busy or missing: {error}")))?;
            // DELETE access without sharing DELETE excludes competing detachers. Bind
            // the held owner file back to the guarded directory before any mutation.
            let directory = platform::directory(&slot)
                .map_err(|error| busy(&format!("lease directory busy: {error}")))?;
            let directory_identity = platform::identity(&directory)?;
            platform::verify_owner(&owner_file, &slot.join("owner.txt"))?;
            let owner_identity = platform::identity(&owner_file)?;
            let record = read_record(&mut owner_file)?;
            if record.released.is_some() {
                if record.values.get("lease") == Some(&owner.lease) {
                    return Err(invalid("a released lease identifier cannot be reused"));
                }
                drop(owner_file);
                detached.push(detach_observed(
                    &slot,
                    &directory,
                    directory_identity,
                    observer,
                )?);
                continue;
            }
            if !recovery {
                return Err(busy("unreleased lease is busy; new work cannot adopt it"));
            }
            validate_recovery(&owner, &record, replacement)?;
            if let Some(replacement) = replacement
                && !record.values.contains_key("replacement_operator")
            {
                let suffix = format!(
                    "replacement_nick={}\nreplacement_operator={}\nreplacement_authorization={}\nreplacement_scope={}\n",
                    owner.nick,
                    replacement.replacement_operator,
                    replacement.authorization_reference,
                    replacement.scope
                );
                append(&mut owner_file, &suffix)?;
            }
            // Preserve the original lease and original worktree provenance; the active
            // recovery operator is explicit in the append-only replacement record.
            let mut recovered_owner = owner;
            recovered_owner.lease = record.required("lease")?.to_owned();
            recovered_owner.started = record.required("started")?.to_owned();
            recovered_owner.worktree = PathBuf::from(record.required("worktree")?);
            return Ok(Self {
                slot,
                directory,
                owner_file: Some(owner_file),
                owner: recovered_owner,
                directory_identity,
                owner_identity,
                released: None,
                detached_before_acquire: detached,
            });
        }
        Err(busy("lease changed during bounded acquisition"))
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn slot(&self) -> &Path {
        &self.slot
    }
    pub fn identity(&self) -> String {
        format!(
            "directory={};owner={}",
            self.directory_identity, self.owner_identity
        )
    }
    pub fn detached_before_acquire(&self) -> &[Detached] {
        &self.detached_before_acquire
    }

    /// Caller must first durably persist the eligible final state and release intent.
    /// Persist this returned record in the journal/final record before closing this lease.
    pub fn publish_release(&mut self, released: &str) -> io::Result<ReleaseRecord> {
        if !utc(released) {
            return Err(invalid("release requires a full UTC timestamp"));
        }
        if let Some(record) = &self.released {
            if record.released == released {
                return Ok(record.clone());
            }
            return Err(invalid("release already published with another timestamp"));
        }
        let file = self
            .owner_file
            .as_mut()
            .ok_or_else(|| invalid("owner handle already closed"))?;
        let current = read_record(file)?;
        if current.released.is_some()
            || current.required("lease")? != self.owner.lease
            || current.required("lane_run")? != self.owner.run
        {
            return Err(invalid("owner identity changed before release"));
        }
        append(
            file,
            &format!(
                "released_by={}\nrelease_run={}\nrelease_lease={}\nreleased={}\n",
                self.owner.operator(),
                self.owner.run,
                self.owner.lease,
                released
            ),
        )?;
        let record = ReleaseRecord {
            slot: self.slot.clone(),
            directory_identity: self.directory_identity,
            owner_identity: self.owner_identity,
            run: self.owner.run.clone(),
            lease: self.owner.lease.clone(),
            released: released.to_owned(),
        };
        self.released = Some(record.clone());
        Ok(record)
    }

    /// Optional cleanup after the caller persists the release record. Never deletes at
    /// the shared path. Rename denial leaves the durable released record untouched.
    pub fn detach_released(self) -> io::Result<Detached> {
        self.detach_released_with_observer(|_| Ok(()))
    }

    /// Optional normal-completion detachment with the same durable write-ahead events
    /// used by fresh acquisition and recovery. No contents are deleted by this API.
    pub fn detach_released_with_observer(
        mut self,
        mut observer: impl FnMut(&DetachEvent) -> io::Result<()>,
    ) -> io::Result<Detached> {
        if self.released.is_none() {
            return Err(invalid("cannot detach an unreleased lease"));
        }
        self.owner_file.take();
        detach_observed(
            &self.slot,
            &self.directory,
            self.directory_identity,
            &mut observer,
        )
    }
}

#[cfg(test)]
fn detach(slot: &Path, directory: &File, expected: Identity) -> io::Result<Detached> {
    detach_observed(slot, directory, expected, &mut |_| Ok(()))
}

fn detach_observed(
    slot: &Path,
    directory: &File,
    expected: Identity,
    observer: &mut dyn FnMut(&DetachEvent) -> io::Result<()>,
) -> io::Result<Detached> {
    if platform::identity(directory)? != expected {
        return Err(invalid("directory identity changed"));
    }
    let name = slot
        .file_name()
        .ok_or_else(|| invalid("invalid slot name"))?
        .to_string_lossy();
    let parent = slot
        .parent()
        .ok_or_else(|| invalid("missing slot parent"))?;
    for _ in 0..8 {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let target = parent.join(format!("{name}.released.{unique}"));
        let generation = Detached {
            path: target.clone(),
            identity: expected,
        };
        observer(&DetachEvent {
            generation: generation.clone(),
            phase: DetachPhase::Intent,
        })?;
        match platform::rename(directory, &target) {
            Ok(()) => {
                observer(&DetachEvent {
                    generation: generation.clone(),
                    phase: DetachPhase::Detached,
                })?;
                return Ok(generation);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(busy(
        "no unique detached destination after bounded attempts",
    ))
}

struct Record {
    values: BTreeMap<String, String>,
    released: Option<String>,
}
impl Record {
    fn required(&self, key: &str) -> io::Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| invalid("missing owner identity field"))
    }
}

fn read_record(file: &mut File) -> io::Result<Record> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take((OWNER_CAP + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > OWNER_CAP {
        return Err(invalid("owner record exceeds 16 KiB"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("owner record is not UTF-8"))?;
    if !text.ends_with('\n') {
        return Err(invalid("owner record is incomplete"));
    }
    let mut values = BTreeMap::new();
    let mut final_key = "";
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid("malformed owner record"))?;
        valid_value(value)?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            return Err(invalid("duplicate or invalid owner field"));
        }
        final_key = key;
    }
    if !values.contains_key("lane_schema") {
        if values.keys().any(|key| key.starts_with("lane_")) {
            return Err(invalid("lane identity has no schema"));
        }
        // Current manual team owners use both nick/started and owner/since. Accept
        // these observed legacy spellings only as aliases, never conflicting values.
        for (alias, canonical) in [("owner", "nick"), ("since", "started")] {
            if let Some(value) = values.remove(alias)
                && values.insert(canonical.to_owned(), value).is_some()
            {
                return Err(invalid("ambiguous manual owner aliases"));
            }
        }
    }
    for key in ["nick", "task", "worktree", "started"] {
        if !values.contains_key(key) {
            return Err(invalid("missing readable owner field"));
        }
    }
    if !utc(&values["started"]) {
        return Err(invalid("invalid owner start time"));
    }
    if let Some(schema) = values.get("lane_schema") {
        if schema != "1" {
            return Err(invalid("unsupported lane owner schema"));
        }
        let allowed = [
            "nick",
            "task",
            "worktree",
            "started",
            "lane_schema",
            "lane_kind",
            "lane_run",
            "lease",
            "lane_root",
            "replacement_nick",
            "replacement_operator",
            "replacement_authorization",
            "replacement_scope",
            "released_by",
            "release_run",
            "release_lease",
            "released",
        ];
        if values.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid("unknown lane owner field"));
        }
        for key in ["lane_kind", "lane_run", "lease", "lane_root"] {
            if !values.contains_key(key) {
                return Err(invalid("incomplete lane identity"));
            }
        }
        if !matches!(values["lane_kind"].as_str(), "vm" | "package")
            || !crate::paths::safe_id(&values["lane_run"])
            || !crate::paths::safe_id(&values["lease"])
        {
            return Err(invalid("invalid lane identity"));
        }
        let replacement_fields = [
            "replacement_nick",
            "replacement_operator",
            "replacement_authorization",
            "replacement_scope",
        ];
        let replacement_count = replacement_fields
            .iter()
            .filter(|key| values.contains_key(**key))
            .count();
        if replacement_count != 0 && replacement_count != replacement_fields.len() {
            return Err(invalid("incomplete named replacement history"));
        }
    }
    let released = values.get("released").cloned();
    if let Some(time) = &released {
        if final_key != "released" || !utc(time) {
            return Err(invalid("released must be the final valid UTC owner line"));
        }
        if values.contains_key("lane_schema")
            && (values.get("release_run") != values.get("lane_run")
                || values.get("release_lease") != values.get("lease")
                || !values.contains_key("released_by"))
        {
            return Err(invalid("released lane identity does not match its owner"));
        }
        if values.contains_key("lane_schema") {
            let original = format!("{}/{}", values["nick"], values["task"]);
            let holder = values.get("replacement_operator").unwrap_or(&original);
            if values.get("released_by") != Some(holder) {
                return Err(invalid("release was not published by the recorded holder"));
            }
        }
    }
    Ok(Record { values, released })
}

fn validate_recovery(
    owner: &Owner,
    record: &Record,
    replacement: Option<&Replacement>,
) -> io::Result<()> {
    if record.required("lane_schema")? != "1"
        || record.required("lane_kind")? != owner.kind
        || record.required("lane_run")? != owner.run
    {
        return Err(invalid("recovery does not match retained lane kind/run"));
    }
    let recorded_root =
        crate::paths::canonical_no_reparse(Path::new(record.required("lane_root")?))?;
    let root = crate::paths::canonical_no_reparse(&owner.lane_root)?;
    if !crate::paths::contains(&root, &recorded_root)
        || !crate::paths::contains(&recorded_root, &root)
    {
        return Err(invalid("recovery root does not match retained binding"));
    }
    let original = format!("{}/{}", record.required("nick")?, record.required("task")?);
    let current = record
        .values
        .get("replacement_operator")
        .map_or(original.as_str(), String::as_str);
    if owner.operator() == current && replacement.is_none() {
        return Ok(());
    }
    let replacement = replacement
        .ok_or_else(|| invalid("foreign recovery requires named explicit authorization"))?;
    for value in [
        &replacement.original_operator,
        &replacement.replacement_operator,
        &replacement.scope,
        &replacement.authorization_reference,
    ] {
        valid_value(value)?;
    }
    let authorization_root = crate::paths::canonical_no_reparse(&replacement.lane_root)?;
    if replacement.run != owner.run
        || replacement.original_operator != original
        || replacement.replacement_operator != owner.operator()
        || replacement.scope != format!("{}-recover", owner.kind)
        || !crate::paths::contains(&root, &authorization_root)
        || !crate::paths::contains(&authorization_root, &root)
    {
        return Err(invalid(
            "replacement authorization does not name this exact recovery",
        ));
    }
    // A crash after the durable owner append can leave the caller's journal
    // incomplete. Replaying that exact capability keeps the original append;
    // it cannot authorize a second replacement or alter the retained scope.
    if record.values.contains_key("replacement_operator")
        && (record.required("replacement_nick")? != owner.nick
            || record.required("replacement_operator")? != replacement.replacement_operator
            || record.required("replacement_authorization")? != replacement.authorization_reference
            || record.required("replacement_scope")? != replacement.scope)
    {
        return Err(invalid(
            "replacement capability differs from retained authorization",
        ));
    }
    Ok(())
}

fn append(file: &mut File, text: &str) -> io::Result<()> {
    let length = file.metadata()?.len();
    if length
        .checked_add(text.len() as u64)
        .is_none_or(|size| size > OWNER_CAP as u64)
    {
        return Err(invalid("owner append exceeds cap"));
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

fn path_text(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("owner path must be valid Unicode"))
}
fn valid_value(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        Err(invalid("empty, oversized or control-bearing owner value"))
    } else {
        Ok(())
    }
}

pub(crate) fn utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes.last() != Some(&b'Z')
    {
        return false;
    }
    for range in [0..4, 5..7, 8..10, 11..13, 14..16, 17..19] {
        if !bytes[range].iter().all(u8::is_ascii_digit) {
            return false;
        }
    }
    if bytes.len() > 20
        && (bytes[19] != b'.'
            || bytes.len() > 30
            || !bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit)
            || bytes.len() == 21)
    {
        return false;
    }
    let number = |start: usize, end: usize| value[start..end].parse::<u32>().unwrap_or(u32::MAX);
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    };
    year > 0
        && day > 0
        && day <= days
        && number(11, 13) < 24
        && number(14, 16) < 60
        && number(17, 19) < 60
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mirror-lease-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        root
    }
    fn owner(root: &Path, lease: &str) -> Owner {
        Owner {
            nick: "codex".into(),
            task: "fixture".into(),
            worktree: root.to_owned(),
            started: "2026-10-13T03:00:00Z".into(),
            kind: "package".into(),
            run: "run-1".into(),
            lease: lease.into(),
            lane_root: root.to_owned(),
        }
    }
    #[test]
    fn active_and_crashed_unreleased_owner_need_exact_recovery() {
        let root = root();
        let slot = root.join("qemu");
        let lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        assert!(Lease::acquire(&slot, owner(&root, "lease-2")).is_err());
        assert!(Lease::recover(&slot, owner(&root, "lease-2"), None).is_err());
        drop(lease);
        assert!(Lease::acquire(&slot, owner(&root, "lease-2")).is_err());
        let mut wrong = owner(&root, "lease-2");
        wrong.run = "other".into();
        assert!(Lease::recover(&slot, wrong, None).is_err());
        let mut recover_owner = owner(&root, "lease-2");
        recover_owner.worktree = root.join("new-own-checkout");
        let lease = Lease::recover(&slot, recover_owner, None).unwrap();
        assert_eq!(lease.owner().lease, "lease-1");
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn released_generation_gets_fresh_lease_and_abandoned_sibling_cannot_block() {
        let root = root();
        let slot = root.join("qemu");
        let mut lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        let record = lease.publish_release("2026-10-13T03:01:00Z").unwrap();
        assert!(Lease::acquire(&slot, owner(&root, "lease-2")).is_err()); // Journal must finish while exclusive handle stays held.
        drop(lease); // Simulated crash after owner publication, before detach.
        let lease = Lease::recover(&slot, owner(&root, "lease-2"), None).unwrap();
        assert_eq!(lease.owner().lease, "lease-2");
        assert_ne!(lease.directory_identity, record.directory_identity);
        assert_eq!(lease.detached_before_acquire().len(), 1);
        assert!(lease.detached_before_acquire()[0].path.exists());
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn detach_cannot_rename_successor_and_never_replaces_destination() {
        let root = root();
        let slot = root.join("qemu");
        let mut lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        lease.publish_release("2026-10-13T03:01:00Z").unwrap();
        // Retain the old handle after a handle-based detach to simulate a stale detacher.
        lease.owner_file.take();
        let detached = detach(&slot, &lease.directory, lease.directory_identity).unwrap();
        let successor = Lease::acquire(&slot, owner(&root, "lease-2")).unwrap();
        let successor_identity = successor.directory_identity;
        assert!(platform::rename(&lease.directory, &slot).is_err()); // No-replace must not displace successor.
        let detached_again = detach(&slot, &lease.directory, lease.directory_identity).unwrap();
        assert_eq!(
            platform::identity(&successor.directory).unwrap(),
            successor_identity
        );
        assert!(slot.join("owner.txt").exists());
        assert!(!detached.path.exists());
        assert!(detached_again.path.exists());
        drop(successor);
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn replacement_is_bound_and_original_history_survives() {
        let root = root();
        let slot = root.join("qemu");
        drop(Lease::acquire(&slot, owner(&root, "lease-1")).unwrap());
        let mut next = owner(&root, "lease-2");
        next.nick = "claude".into();
        assert!(Lease::recover(&slot, next.clone(), None).is_err());
        let mut authorization = Replacement {
            run: "wrong".into(),
            lane_root: root.clone(),
            original_operator: "codex/fixture".into(),
            replacement_operator: "claude/fixture".into(),
            scope: "package-recover".into(),
            authorization_reference: "direct-user-message-fixture".into(),
        };
        assert!(Lease::recover(&slot, next.clone(), Some(&authorization)).is_err());
        authorization.run = "run-1".into();
        let mut lease = Lease::recover(&slot, next, Some(&authorization)).unwrap();
        lease.publish_release("2026-10-13T03:01:00Z").unwrap();
        drop(lease);
        let text = fs::read_to_string(slot.join("owner.txt")).unwrap();
        assert!(text.starts_with("nick=codex\n"));
        assert!(text.contains("replacement_nick=claude\n"));
        assert!(text.contains("released_by=claude/fixture\n"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn replacement_replay_preserves_one_append_and_rejects_changed_capability() {
        let root = root();
        let slot = root.join("qemu");
        drop(Lease::acquire(&slot, owner(&root, "lease-1")).unwrap());
        let mut next = owner(&root, "lease-2");
        next.nick = "claude".into();
        let authorization = Replacement {
            run: "run-1".into(),
            lane_root: root.clone(),
            original_operator: "codex/fixture".into(),
            replacement_operator: "claude/fixture".into(),
            scope: "package-recover".into(),
            authorization_reference: "direct-user-message-fixture".into(),
        };
        drop(Lease::recover(&slot, next.clone(), Some(&authorization)).unwrap());
        let appended = fs::read(slot.join("owner.txt")).unwrap();
        drop(Lease::recover(&slot, next.clone(), Some(&authorization)).unwrap());
        assert_eq!(fs::read(slot.join("owner.txt")).unwrap(), appended);
        for field in [
            "reference",
            "scope",
            "original",
            "replacement",
            "run",
            "root",
        ] {
            let mut changed = authorization.clone();
            match field {
                "reference" => changed.authorization_reference.push_str("-other"),
                "scope" => changed.scope = "vm-recover".into(),
                "original" => changed.original_operator = "kimi/fixture".into(),
                "replacement" => changed.replacement_operator = "kimi/fixture".into(),
                "run" => changed.run = "run-other".into(),
                "root" => changed.lane_root = root.parent().unwrap().to_owned(),
                _ => unreachable!(),
            }
            assert!(
                Lease::recover(&slot, next.clone(), Some(&changed)).is_err(),
                "{field}"
            );
            assert_eq!(fs::read(slot.join("owner.txt")).unwrap(), appended);
        }
        assert!(Lease::recover(&slot, owner(&root, "lease-3"), None).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn malformed_or_nonfinal_release_never_authorizes_takeover() {
        let root = root();
        let slot = root.join("qemu");
        drop(Lease::acquire(&slot, owner(&root, "lease-1")).unwrap());
        let original = fs::read_to_string(slot.join("owner.txt")).unwrap();
        for suffix in [
            "released=not-a-time\n",
            "released=2026-10-13T03:01:00Z\nextra=value\n",
            "released=2026-02-30T03:01:00Z\n",
        ] {
            fs::write(slot.join("owner.txt"), format!("{original}{suffix}")).unwrap();
            assert!(Lease::acquire(&slot, owner(&root, "lease-2")).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn manual_final_release_can_be_detached_but_not_adopted() {
        let root = root();
        let slot = root.join("qemu");
        fs::create_dir(&slot).unwrap();
        fs::write(slot.join("owner.txt"), "nick=kimi\ntask=manual\nworktree=C:/temporary\nstarted=2026-10-13T03:00:00.0000000Z\nreleased=2026-10-13T03:01:00Z\n").unwrap();
        let lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        assert_eq!(lease.detached_before_acquire().len(), 1);
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observed_manual_owner_since_aliases_require_a_final_release() {
        let root = root();
        let slot = root.join("qemu");
        fs::create_dir(&slot).unwrap();
        let record =
            "owner=claude\ntask=manual\nworktree=C:/temporary\nsince=2026-10-13T03:26:24Z\n";
        fs::write(slot.join("owner.txt"), record).unwrap();
        assert!(Lease::acquire(&slot, owner(&root, "lease-1")).is_err());
        fs::write(
            slot.join("owner.txt"),
            format!("{record}released=2026-10-13T03:27:00Z\n"),
        )
        .unwrap();
        let lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        assert_eq!(lease.detached_before_acquire().len(), 1);
        assert_eq!(
            fs::read_to_string(lease.detached_before_acquire()[0].path.join("owner.txt")).unwrap(),
            format!("{record}released=2026-10-13T03:27:00Z\n")
        );
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn denied_detach_preserves_release_and_detached_delete_denial_is_harmless() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = root();
        let slot = root.join("qemu");
        let mut lease = Lease::acquire(&slot, owner(&root, "lease-1")).unwrap();
        lease.publish_release("2026-10-13T03:01:00Z").unwrap();
        lease.owner_file.take();
        let collision = root.join("existing-destination");
        fs::create_dir(&collision).unwrap();
        assert!(platform::rename(&lease.directory, &collision).is_err());
        assert!(
            fs::read_to_string(slot.join("owner.txt"))
                .unwrap()
                .ends_with("released=2026-10-13T03:01:00Z\n")
        );
        let detached = lease.detach_released().unwrap();
        let survivor = Lease::acquire(&slot, owner(&root, "lease-2")).unwrap();
        // A reader denying deletion is a real Windows cleanup denial, in a temp root.
        let deny_delete = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(detached.path.join("owner.txt"))
            .unwrap();
        assert!(fs::remove_file(detached.path.join("owner.txt")).is_err());
        assert!(slot.join("owner.txt").exists());
        drop(deny_delete);
        fs::remove_file(detached.path.join("owner.txt")).unwrap();
        // Simulate controller death between detached owner-file and directory cleanup.
        assert!(detached.path.exists());
        assert_eq!(survivor.owner().lease, "lease-2");
        drop(survivor);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_competing_recovery_has_one_holder() {
        let root = root();
        let slot = root.join("qemu");
        drop(Lease::acquire(&slot, owner(&root, "lease-1")).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for _ in 0..2 {
            let (root, slot, barrier, sender) =
                (root.clone(), slot.clone(), barrier.clone(), sender.clone());
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let recovered = Lease::recover(&slot, owner(&root, "unused-fresh"), None);
                sender.send(recovered.is_ok()).unwrap();
                // Keep the winner's handles open until both attempts have completed.
                barrier.wait();
                drop(recovered);
            }));
        }
        assert_eq!((0..2).filter(|_| receiver.recv().unwrap()).count(), 1);
        for thread in threads {
            thread.join().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn competing_fresh_acquisition_never_strands_an_ownerless_generation() {
        let root = root();
        let slot = root.join("qemu");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for id in 0..2 {
            let (root, slot, barrier, sender) =
                (root.clone(), slot.clone(), barrier.clone(), sender.clone());
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let lease = Lease::acquire(&slot, owner(&root, &format!("lease-{id}")));
                sender.send(lease.is_ok()).unwrap();
                barrier.wait();
                drop(lease);
            }));
        }
        assert_eq!((0..2).filter(|_| receiver.recv().unwrap()).count(), 1);
        for thread in threads {
            thread.join().unwrap();
        }
        let recovered = Lease::recover(&slot, owner(&root, "recovery-id"), None).unwrap();
        drop(recovered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detach_observer_records_intent_before_mutation_and_lost_acquire() {
        let root = root();
        let slot = root.join("qemu");
        let mut old = Lease::acquire(&slot, owner(&root, "old")).unwrap();
        old.publish_release("2026-10-13T03:01:00Z").unwrap();
        drop(old);
        let mut events = Vec::new();
        let mut competitor = None;
        let attempt = Lease::acquire_with_observer(&slot, owner(&root, "attempt"), |event| {
            match event.phase {
                DetachPhase::Intent => {
                    assert!(slot.exists());
                    assert!(!event.generation.path.exists());
                }
                DetachPhase::Detached => {
                    assert!(event.generation.path.exists());
                    assert!(!slot.exists());
                    competitor = Some(Lease::acquire(&slot, owner(&root, "competitor"))?);
                }
            }
            events.push(event.clone());
            Ok(())
        });
        assert!(attempt.is_err());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].generation, events[1].generation);
        assert!(events[1].generation.path.exists());
        assert_eq!(competitor.as_ref().unwrap().owner().lease, "competitor");
        drop(competitor);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_durable_detach_intent_prevents_rename() {
        let root = root();
        let slot = root.join("qemu");
        let mut old = Lease::acquire(&slot, owner(&root, "old")).unwrap();
        old.publish_release("2026-10-13T03:01:00Z").unwrap();
        let expected = old.directory_identity;
        drop(old);
        assert!(
            Lease::acquire_with_observer(&slot, owner(&root, "attempt"), |_| Err(
                io::Error::other("injected journal denial")
            ))
            .is_err()
        );
        let directory = platform::directory(&slot).unwrap();
        assert_eq!(platform::identity(&directory).unwrap(), expected);
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_buffer_terminates_all_utf16_alignment_lengths() {
        let root = root();
        let slot = root.join("qemu");
        let mut lease = Lease::acquire(&slot, owner(&root, "old")).unwrap();
        lease.publish_release("2026-10-13T03:01:00Z").unwrap();
        lease.owner_file.take();
        for length in 1..=8 {
            let target = root.join("x".repeat(length));
            platform::rename(&lease.directory, &target).unwrap();
            assert!(
                target.join("owner.txt").exists(),
                "exact destination missing at UTF-16 length {length}"
            );
        }
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
}
