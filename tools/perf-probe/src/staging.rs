//! Fixed lane binding and generation-last staging. Real VMX profile/off-state
//! adapters remain in 0015d; this controller is exercised with fake artifacts.

use crate::bounds::{self, Deadline};
use crate::package::{Artifact, Manifest, Progress, Verdict, validate_id};
use crate::paths;
use crate::record::{self, Value};
use crate::sha256;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    pub vmx: PathBuf,
    pub root: PathBuf,
    pub descriptor: PathBuf,
    pub image: PathBuf,
    pub serial: PathBuf,
}
impl Binding {
    /// Called after the future profile adapter has rejected extra disks,
    /// snapshots/parent chains, duplicate keys and malformed VMX configuration.
    pub fn new(
        vmx: &Path,
        root: &Path,
        configured_disk: &Path,
        configured_serial: &Path,
        worktrees: &[PathBuf],
    ) -> io::Result<Self> {
        let root = paths::validate_lane_root(root, worktrees)?;
        let vmx = paths::canonical_no_reparse(vmx)?;
        let descriptor = paths::resolve_no_reparse(&root.join("disk/mirror.vmdk"))?;
        let image = paths::resolve_no_reparse(&root.join("disk/mirror.img"))?;
        let serial = paths::resolve_no_reparse(&root.join("serial.log"))?;
        if paths::resolve_no_reparse(configured_disk)? != descriptor
            || paths::resolve_no_reparse(configured_serial)? != serial
        {
            return Err(record::invalid(
                "VMX disk/serial do not match fixed LaneRoot binding",
            ));
        }
        Ok(Self {
            vmx,
            root,
            descriptor,
            image,
            serial,
        })
    }
    pub fn record(&self) -> Value {
        Value::object([
            ("schema", Value::Number(1)),
            ("vmx", Value::text(self.vmx.to_string_lossy())),
            ("root", Value::text(self.root.to_string_lossy())),
            ("descriptor", Value::text(self.descriptor.to_string_lossy())),
            ("image", Value::text(self.image.to_string_lossy())),
            ("serial", Value::text(self.serial.to_string_lossy())),
        ])
    }
    pub fn check_retained(&self, retained: &Value) -> io::Result<()> {
        if retained != &self.record() {
            return Err(record::invalid("retained VMX/LaneRoot binding differs"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Image,
    Descriptor,
}

#[derive(Clone, Debug)]
pub struct Generation {
    pub run: String,
    pub package: String,
    pub binding: Value,
    pub image: Artifact,
    pub descriptor: Artifact,
    /// Hash of the canonical typed manifest, including source capabilities.
    pub manifest_sha256: String,
}
impl Generation {
    pub fn record(&self) -> Value {
        Value::object([
            ("schema", Value::Number(1)),
            ("run", Value::text(&self.run)),
            ("package", Value::text(&self.package)),
            ("binding", self.binding.clone()),
            ("image", self.image.record()),
            ("descriptor", self.descriptor.record()),
            ("manifest_sha256", Value::text(&self.manifest_sha256)),
        ])
    }
}

pub struct Permission {
    pub positive_off_proof: String,
    pub exclusive_lease_identity: String,
}

/// Source methods read only the calling checkout's build/dist. The package-final
/// method resolves LaneRoot/packages/<id>/final.json, never a supplied alternate
/// final or a manifest-only shortcut. Copies must use new temporaries and no
/// reparse traversal; publish operations are atomic renames on this binding.
pub trait Adapter {
    fn now(&self) -> Instant;
    fn retained_binding(&mut self) -> io::Result<Value>;
    fn package_final(&mut self, package: &str, deadline: Instant) -> io::Result<Progress>;
    fn source_manifest(&mut self, deadline: Instant) -> io::Result<Manifest>;
    fn source_artifact(&mut self, kind: Kind, deadline: Instant) -> io::Result<Artifact>;
    fn preparing(&mut self, generation: &Generation) -> io::Result<()>;
    fn invalidate_generation(&mut self) -> io::Result<()>;
    fn copy_verified(
        &mut self,
        kind: Kind,
        temporary: &Path,
        expected: &Artifact,
        deadline: Instant,
    ) -> io::Result<Artifact>;
    fn publish_file(&mut self, temporary: &Path, destination: &Path) -> io::Result<()>;
    fn publish_manifest(&mut self, manifest: &Manifest, deadline: Instant) -> io::Result<()>;
    fn publish_generation(&mut self, generation: &Generation) -> io::Result<()>;
    fn staged_artifact(&mut self, kind: Kind, deadline: Instant) -> io::Result<Artifact>;
}

pub fn stage(
    b: &mut impl Adapter,
    binding: &Binding,
    run: &str,
    permission: &Permission,
    case_deadline: Instant,
) -> io::Result<Generation> {
    validate_id(run)?;
    if permission.positive_off_proof.is_empty() || permission.exclusive_lease_identity.is_empty() {
        return Err(record::invalid(
            "staging requires positive off proof and held exclusive lease",
        ));
    }
    let end = Deadline::at(
        b.now(),
        bounds::PACKAGE_STAGE.min(case_deadline.saturating_duration_since(b.now())),
    )?;
    binding.check_retained(&b.retained_binding()?)?;
    let manifest = b.source_manifest(end.expires_at())?;
    // Revalidate even an in-memory manifest so a future adapter cannot admit an
    // unsupported profile, missing capability, or invalid inventory by accident.
    Manifest::parse(&manifest.record())?;
    let final_record = b.package_final(&manifest.package_id, end.expires_at())?;
    // The adapter may deserialize separately or retain an in-memory record;
    // either route must satisfy the same finalized-state/release-proof schema.
    let final_record = Progress::parse(&final_record.record())?;
    if final_record.verdict != Verdict::FinalizedValid
        || !final_record.no_live_build
        || final_record.first_failure.is_some()
        || final_record.manifest.as_ref() != Some(&manifest)
    {
        return Err(record::invalid(
            "package lacks matching durable finalized-valid record",
        ));
    }
    let image = b.source_artifact(Kind::Image, end.expires_at())?;
    let descriptor = b.source_artifact(Kind::Descriptor, end.expires_at())?;
    if image != manifest.image || descriptor != manifest.descriptor {
        return Err(record::invalid("published package hash or size mismatch"));
    }
    end.remaining_at(b.now())?;
    let generation = Generation {
        run: run.into(),
        package: manifest.package_id.clone(),
        binding: binding.record(),
        image,
        descriptor,
        manifest_sha256: sha256::bytes(manifest.record().encode().as_bytes()),
    };
    b.preparing(&generation)?;
    b.invalidate_generation()?;
    for (kind, destination, expected) in [
        (Kind::Image, &binding.image, &generation.image),
        (
            Kind::Descriptor,
            &binding.descriptor,
            &generation.descriptor,
        ),
    ] {
        end.remaining_at(b.now())?;
        let temporary = destination.with_extension(format!("stage-{run}.tmp"));
        paths::validate_child(&binding.root, &temporary)?;
        let copied = b.copy_verified(kind, &temporary, expected, end.expires_at())?;
        if &copied != expected {
            return Err(record::invalid("staging temporary hash mismatch"));
        }
        end.remaining_at(b.now())?;
        b.publish_file(&temporary, destination)?;
    }
    b.publish_manifest(&manifest, end.expires_at())?;
    end.remaining_at(b.now())?;
    // No valid generation exists after any interrupted copy/publication above.
    b.publish_generation(&generation)?;
    Ok(generation)
}

pub fn verify_after_off(
    b: &mut impl Adapter,
    generation: &Generation,
    positive_off_proof: &str,
    deadline: Instant,
) -> io::Result<()> {
    if positive_off_proof.is_empty() {
        return Err(record::invalid("post-run hashes require proven off state"));
    }
    let image = b.staged_artifact(Kind::Image, deadline)?;
    let descriptor = b.staged_artifact(Kind::Descriptor, deadline)?;
    if b.now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "post-off hash deadline",
        ));
    }
    if image != generation.image || descriptor != generation.descriptor {
        return Err(record::invalid(
            "staged image or descriptor changed; preserve both artifacts",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{CAPABILITIES, SourceFile};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        binding: Binding,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mirror-staging-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("Mirror.vmx"), b"synthetic").unwrap();
            let binding = Binding::new(
                &root.join("Mirror.vmx"),
                &root,
                &root.join("disk/mirror.vmdk"),
                &root.join("serial.log"),
                &[],
            )
            .unwrap();
            Self { root, binding }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn artifact(byte: char) -> Artifact {
        Artifact {
            size: 4096,
            sha256: byte.to_string().repeat(64),
        }
    }
    fn manifest() -> Manifest {
        Manifest {
            package_id: "synthetic-package".into(),
            commit: "1".repeat(40),
            tree: "2".repeat(40),
            release: false,
            toolchain: "rustc 1.99.0 (synthetic)".into(),
            capabilities: CAPABILITIES
                .iter()
                .map(|(name, prefix)| (name.to_string(), format!("{prefix:0<40}")))
                .collect(),
            inventory: vec![SourceFile {
                path: "Cargo.toml".into(),
                artifact: artifact('3'),
            }],
            image: artifact('a'),
            descriptor: artifact('b'),
        }
    }
    struct Fake {
        now: Instant,
        binding: Value,
        manifest: Manifest,
        final_record: Progress,
        log: Vec<&'static str>,
        fail: Option<&'static str>,
        generation: bool,
        mutated: bool,
    }
    impl Fake {
        fn new(binding: &Binding) -> Self {
            let manifest = manifest();
            Self {
                now: Instant::now(),
                binding: binding.record(),
                final_record: Progress {
                    phase: "finalized".into(),
                    verdict: Verdict::FinalizedValid,
                    no_live_build: true,
                    release_intent: true,
                    manifest: Some(manifest.clone()),
                    ..Progress::default()
                },
                manifest,
                log: Vec::new(),
                fail: None,
                generation: false,
                mutated: false,
            }
        }
        fn step(&mut self, event: &'static str) -> io::Result<()> {
            self.log.push(event);
            if self.fail == Some(event) {
                Err(io::Error::other(format!("injected {event}")))
            } else {
                Ok(())
            }
        }
    }
    impl Adapter for Fake {
        fn now(&self) -> Instant {
            self.now
        }
        fn retained_binding(&mut self) -> io::Result<Value> {
            Ok(self.binding.clone())
        }
        fn package_final(&mut self, _: &str, _: Instant) -> io::Result<Progress> {
            Ok(self.final_record.clone())
        }
        fn source_manifest(&mut self, _: Instant) -> io::Result<Manifest> {
            Ok(self.manifest.clone())
        }
        fn source_artifact(&mut self, kind: Kind, _: Instant) -> io::Result<Artifact> {
            Ok(if kind == Kind::Image {
                self.manifest.image.clone()
            } else {
                self.manifest.descriptor.clone()
            })
        }
        fn preparing(&mut self, _: &Generation) -> io::Result<()> {
            self.step("preparing")
        }
        fn invalidate_generation(&mut self) -> io::Result<()> {
            self.step("invalidate")?;
            self.generation = false;
            Ok(())
        }
        fn copy_verified(
            &mut self,
            _: Kind,
            _: &Path,
            expected: &Artifact,
            _: Instant,
        ) -> io::Result<Artifact> {
            self.step("copy")?;
            Ok(expected.clone())
        }
        fn publish_file(&mut self, _: &Path, _: &Path) -> io::Result<()> {
            self.step("publish-file")
        }
        fn publish_manifest(&mut self, _: &Manifest, _: Instant) -> io::Result<()> {
            self.step("manifest")
        }
        fn publish_generation(&mut self, _: &Generation) -> io::Result<()> {
            self.step("generation")?;
            self.generation = true;
            Ok(())
        }
        fn staged_artifact(&mut self, kind: Kind, _: Instant) -> io::Result<Artifact> {
            self.log.push(if kind == Kind::Image {
                "read-image"
            } else {
                "read-descriptor"
            });
            if self.mutated {
                Ok(artifact('c'))
            } else {
                self.source_artifact(kind, self.now)
            }
        }
    }
    fn permission() -> Permission {
        Permission {
            positive_off_proof: "synthetic off proof".into(),
            exclusive_lease_identity: "synthetic lease".into(),
        }
    }
    #[test]
    fn stage_requires_valid_final_and_matching_manifest_not_leftover_files() {
        let fixture = Fixture::new();
        for valid in [false, true] {
            let mut fake = Fake::new(&fixture.binding);
            if !valid {
                fake.final_record.verdict = Verdict::RecoveredInvalid;
            }
            let end = fake.now + Duration::from_secs(900);
            assert_eq!(
                stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_ok(),
                valid
            );
            assert_eq!(fake.generation, valid);
            if valid {
                assert_eq!(fake.log.last(), Some(&"generation"));
            } else {
                assert!(fake.log.is_empty());
            }
        }
    }
    #[test]
    fn stage_rejects_unfinalized_or_forged_proof_before_any_mutation() {
        let fixture = Fixture::new();
        for mutation in ["phase", "intent", "proof", "package", "manifest"] {
            let mut fake = Fake::new(&fixture.binding);
            match mutation {
                "phase" => fake.final_record.phase = "building".into(),
                "intent" => fake.final_record.release_intent = false,
                "proof" => fake.final_record.lock_release = Some(Value::text("owner-record")),
                "package" => {
                    fake.final_record.manifest.as_mut().unwrap().package_id = "other".into()
                }
                "manifest" => fake.final_record.manifest.as_mut().unwrap().image = artifact('c'),
                _ => unreachable!(),
            }
            let end = fake.now + Duration::from_secs(900);
            assert!(
                stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_err(),
                "{mutation}"
            );
            assert!(fake.log.is_empty(), "{mutation}");
            assert!(!fake.generation);
        }
        // A durable final precedes the owner-record append. Recovery may later
        // retain a previously valid package while recording abandoned scratch.
        let mut valid = Fake::new(&fixture.binding);
        valid
            .final_record
            .abandoned
            .push("previously-valid/source".into());
        assert!(valid.final_record.lock_release.is_none());
        let end = valid.now + Duration::from_secs(900);
        assert!(stage(&mut valid, &fixture.binding, "run1", &permission(), end).is_ok());
    }
    #[test]
    fn staging_crashes_never_publish_a_generation_and_missing_permissions_do_nothing() {
        let fixture = Fixture::new();
        for failure in ["copy", "publish-file", "manifest", "generation"] {
            let mut fake = Fake::new(&fixture.binding);
            fake.generation = true;
            fake.fail = Some(failure);
            let end = fake.now + Duration::from_secs(900);
            assert!(stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_err());
            assert!(!fake.generation);
        }
        let mut fake = Fake::new(&fixture.binding);
        let end = fake.now + Duration::from_secs(900);
        let mut permission = permission();
        permission.positive_off_proof.clear();
        assert!(stage(&mut fake, &fixture.binding, "run1", &permission, end).is_err());
        assert!(fake.log.is_empty());
    }
    #[test]
    fn binding_alias_and_other_root_cannot_bypass_retained_journal() {
        let fixture = Fixture::new();
        let mut fake = Fake::new(&fixture.binding);
        fake.binding
            .set("root", Value::text("different-root"))
            .unwrap();
        let end = fake.now + Duration::from_secs(900);
        assert!(stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_err());
        assert!(
            Binding::new(
                &fixture.root.join("Mirror.vmx"),
                &fixture.root,
                &fixture.root.join("other.vmdk"),
                &fixture.root.join("serial.log"),
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn post_off_compares_both_artifacts_and_preserves_changed_files() {
        let fixture = Fixture::new();
        let mut fake = Fake::new(&fixture.binding);
        let end = fake.now + Duration::from_secs(900);
        let generation = stage(&mut fake, &fixture.binding, "run1", &permission(), end).unwrap();
        fake.mutated = true;
        assert!(verify_after_off(&mut fake, &generation, "off-proof", end).is_err());
        assert!(fake.log.ends_with(&["read-image", "read-descriptor"]));
        assert!(fake.generation);
    }
    #[test]
    fn ps2_only_manifest_and_mismatched_final_manifest_are_rejected() {
        let fixture = Fixture::new();
        let mut fake = Fake::new(&fixture.binding);
        fake.manifest.capabilities.truncate(1);
        let end = fake.now + Duration::from_secs(900);
        assert!(stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_err());
        let mut fake = Fake::new(&fixture.binding);
        fake.final_record.manifest.as_mut().unwrap().release = true;
        let end = fake.now + Duration::from_secs(900);
        assert!(stage(&mut fake, &fixture.binding, "run1", &permission(), end).is_err());
    }
}
