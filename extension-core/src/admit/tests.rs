use super::*;
use alloc::vec;
use alloc::vec::Vec;
use slopos_abi::caps;
use slopos_abi::fs::S_IFREG;
use slopos_tls_core::ed25519::SigningKey;
use slopos_tls_core::sshsig::{self, PublicKey};

use crate::elf::{Kind, build};
use crate::image::Packer;
use crate::{INTERPRETER, NAMESPACE, sha256};

const CEILING: u64 = caps::POWER | caps::NET_ADMIN | caps::MOUNT | caps::SCHED_HIGH;

fn libc() -> Vec<u8> {
    build(Kind::Shared, None, &[], &[], &["puts"])
}

/// `hello` at `version`, its daemon holding `caps` and binding `imports`,
/// with an unasked action holding `unasked` when that is not empty.
fn image(version: u64, caps: &str, unasked: &str, imports: &[&str]) -> Vec<u8> {
    let mut packer = Packer::default();
    let program = build(
        Kind::Executable,
        Some(INTERPRETER),
        &["libc.so"],
        imports,
        &[],
    );
    packer.add("/bin/hellod", S_IFREG | 0o755, program.clone());
    packer.add("/libexec/act", S_IFREG | 0o755, program);
    let mut declare =
        alloc::format!("user\nservice hellod\n  program /opt/hello/bin/hellod\n  caps {caps}\n");
    if !unasked.is_empty() {
        declare.push_str(&alloc::format!(
            "action act\n  program /opt/hello/libexec/act\n  args act\n  caps {unasked}\n  allow user\n  describe \"Act\"\n"
        ));
    }
    packer
        .finish(&alloc::format!("name hello\nversion {version}\n"), &declare)
        .unwrap()
}

struct Owner {
    key: SigningKey,
    keys: Keys,
    libc: Vec<u8>,
}

impl Owner {
    fn new(ceiling: u64, user: u64) -> Owner {
        Owner::of(1, ceiling, user)
    }

    fn of(seed: u8, ceiling: u64, user: u64) -> Owner {
        let key = SigningKey::from_seed(&[seed; 32]);
        let mut keys = Keys::default();
        keys.put(Key::new(PublicKey(key.public()), ceiling, user, "owner").unwrap())
            .unwrap();
        Owner {
            key,
            keys,
            libc: libc(),
        }
    }

    fn bounds(&self) -> Bounds<'_> {
        Bounds {
            keys: &self.keys,
            ceiling: CEILING,
            libc: &self.libc,
        }
    }

    fn sign(&self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let image = Image::check(bytes).unwrap();
        let sidecar = Sidecar {
            name: image.release.name.clone(),
            version: image.release.version,
            sha256: sha256(bytes),
        }
        .render()
        .into_bytes();
        let signature = sshsig::sign(&self.key, NAMESPACE, &sidecar);
        (sidecar, signature)
    }

    fn signed(&self, record: Option<&Record>, bytes: &[u8]) -> Result<Admitted, Refusal> {
        let (sidecar, signature) = self.sign(bytes);
        let authority = Authority::Signed {
            sidecar: &sidecar,
            signature: &signature,
        };
        let image = Image::check(bytes).unwrap();
        install(
            record,
            &image,
            sha256(bytes),
            &authority,
            &self.bounds(),
            &[],
        )
    }

    fn approved(
        &self,
        record: Option<&Record>,
        bytes: &[u8],
        grant: &Grant,
    ) -> Result<Admitted, Refusal> {
        let authority = Authority::Person {
            name: "hello",
            grant,
        };
        let image = Image::check(bytes).unwrap();
        install(
            record,
            &image,
            sha256(bytes),
            &authority,
            &self.bounds(),
            &[],
        )
    }

    fn runs(&self, record: &Record, bytes: &[u8]) -> Result<(), Refusal> {
        let (sidecar, signature) = self.sign(bytes);
        let image = Image::check(bytes).unwrap();
        activation(
            record,
            &image,
            sha256(bytes),
            Some((&sidecar, &signature)),
            &self.bounds(),
        )
    }
}

fn grant(caps: u64) -> Grant {
    Grant {
        caps,
        unasked: Vec::new(),
        users: vec!["hello".into()],
        commands: Vec::new(),
    }
}

fn installed(admitted: Result<Admitted, Refusal>) -> Installed {
    match admitted {
        Ok(Admitted::Install(installed)) => installed,
        other => panic!("not installed: {other:?}"),
    }
}

fn refused(admitted: Result<Admitted, Refusal>, says: &str) {
    match admitted {
        Err(Refusal(why)) => assert!(why.contains(says), "{why:?} does not say {says:?}"),
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn a_first_install_grants_exactly_what_the_person_read() {
    let owner = Owner::new(0, 0);
    let bytes = image(1, "NetAdmin", "", &["puts"]);
    let got = installed(owner.approved(None, &bytes, &grant(caps::NET_ADMIN)));
    assert_eq!(
        (got.version, got.signer.as_ref(), got.grant.clone()),
        (1, None, grant(caps::NET_ADMIN))
    );
    refused(
        owner.approved(None, &bytes, &grant(0)),
        "not what was approved",
    );
    refused(
        owner.approved(None, &bytes, &grant(caps::NET_ADMIN | caps::POWER)),
        "not what was approved",
    );
    let wizard = image(1, "SetId", "", &["puts"]);
    refused(
        owner.approved(None, &wizard, &grant(caps::SET_ID)),
        "SetId, which no extension",
    );
    let mut record = Record::new("hello");
    record.install(got);
    let mut runs = record.clone();
    runs.current.as_mut().unwrap().signer = None;
    let image = Image::check(&bytes).unwrap();
    assert_eq!(
        activation(&runs, &image, sha256(&bytes), None, &owner.bounds()),
        Ok(())
    );
}

#[test]
fn a_signed_image_goes_in_unasked_within_its_key_s_ceilings() {
    let owner = Owner::new(caps::NET_ADMIN | caps::POWER, caps::POWER);
    let got = installed(owner.signed(None, &image(1, "NetAdmin", "Power", &["puts"])));
    assert_eq!(
        got.signer,
        Some(PublicKey(owner.key.public()).fingerprint())
    );
    assert_eq!(got.grant.unasked, ["act"]);
    refused(
        owner.signed(None, &image(1, "Mount", "", &["puts"])),
        "Mount, past its key's ceiling",
    );
    let low_user = Owner::new(caps::NET_ADMIN | caps::POWER, 0);
    refused(
        low_user.signed(None, &image(1, "NetAdmin", "Power", &["puts"])),
        "past its key's user ceiling",
    );
    let stranger = Owner {
        keys: Owner::new(CEILING, 0).keys,
        ..Owner::of(2, CEILING, 0)
    };
    refused(
        stranger.signed(None, &image(1, "NetAdmin", "", &["puts"])),
        "does not trust",
    );
}

#[test]
fn updates_by_the_same_key_go_in_and_nothing_older_does() {
    let owner = Owner::new(CEILING, 0);
    let mut record = Record::new("hello");
    let v1 = image(1, "NetAdmin", "", &["puts"]);
    let v2 = image(2, "NetAdmin", "", &["puts"]);
    record.install(installed(owner.signed(None, &v1)));
    assert_eq!(owner.signed(Some(&record), &v1), Ok(Admitted::Current));
    record.install(installed(owner.signed(Some(&record), &v2)));
    assert_eq!(record.floor.map(|(v, _)| v), Some(2));
    assert_eq!(record.previous.as_ref().map(|p| p.version), Some(1));
    refused(owner.signed(Some(&record), &v1), "older than version 2");
    refused(
        owner.signed(Some(&record), &image(2, "Power", "", &["puts"])),
        "other bytes",
    );
    let other = Owner::of(3, CEILING, 0);
    let mut both = other.keys.clone();
    both.put(owner.keys.iter().next().unwrap().clone()).unwrap();
    let other = Owner {
        keys: both,
        ..other
    };
    refused(
        other.signed(Some(&record), &image(3, "NetAdmin", "", &["puts"])),
        "another authority",
    );
    let person = Owner::new(0, 0);
    let mut by_person = Record::new("hello");
    by_person.install(installed(person.approved(
        None,
        &v1,
        &grant(caps::NET_ADMIN),
    )));
    refused(owner.signed(Some(&by_person), &v2), "another authority");
}

#[test]
fn a_rollback_refuses_the_version_it_left_until_a_newer_one() {
    let owner = Owner::new(CEILING, 0);
    let mut record = Record::new("hello");
    let (v1, v2, v3) = (
        image(1, "NetAdmin", "", &["puts"]),
        image(2, "NetAdmin", "", &["puts"]),
        image(3, "NetAdmin", "", &["puts"]),
    );
    record.install(installed(owner.signed(None, &v1)));
    record.install(installed(owner.signed(Some(&record), &v2)));
    record.roll_back().unwrap();
    assert_eq!(record.current.as_ref().map(|c| c.version), Some(1));
    assert_eq!(
        (record.floor.map(|(v, _)| v), record.refused.clone()),
        (Some(1), vec![2])
    );
    refused(owner.signed(Some(&record), &v2), "rolled back from");
    let back = installed(owner.approved(Some(&record), &v2, &grant(caps::NET_ADMIN)));
    assert_eq!(back.version, 2);
    record.install(installed(owner.signed(Some(&record), &v3)));
    assert!(record.refused.is_empty());
    record.remove();
    refused(owner.signed(Some(&record), &v2), "older than version 3");
    assert!(matches!(
        owner.signed(Some(&record), &v3),
        Ok(Admitted::Install(_))
    ));
}

#[test]
fn an_activation_holds_the_image_to_its_record_and_its_key() {
    let owner = Owner::new(CEILING, 0);
    let bytes = image(1, "NetAdmin", "", &["puts"]);
    let mut record = Record::new("hello");
    record.install(installed(owner.signed(None, &bytes)));
    assert_eq!(owner.runs(&record, &bytes), Ok(()));
    let lowered = Owner::new(caps::POWER, 0);
    refused(
        lowered.runs(&record, &bytes).map(|()| Admitted::Current),
        "past its key's ceiling",
    );
    let removed = Owner {
        keys: Keys::default(),
        ..Owner::new(CEILING, 0)
    };
    refused(
        removed.runs(&record, &bytes).map(|()| Admitted::Current),
        "does not trust",
    );
    let other = image(1, "Power", "", &["puts"]);
    refused(
        owner.runs(&record, &other).map(|()| Admitted::Current),
        "not the one installed",
    );
    let image = Image::check(&bytes).unwrap();
    refused(
        activation(&record, &image, sha256(&bytes), None, &owner.bounds())
            .map(|()| Admitted::Current),
        "signature is not in the store",
    );
}

#[test]
fn an_image_binding_what_libc_lacks_is_refused() {
    let owner = Owner::new(CEILING, 0);
    refused(
        owner.signed(None, &image(1, "NetAdmin", "", &["puts", "lacking"])),
        "binds lacking",
    );
    refused(
        owner.approved(
            None,
            &image(1, "NetAdmin", "", &["lacking"]),
            &grant(caps::NET_ADMIN),
        ),
        "binds lacking",
    );
}

#[test]
fn two_extensions_never_declare_one_command() {
    let owner = Owner::new(0, 0);
    let mut packer = Packer::default();
    let program = build(Kind::Executable, Some(INTERPRETER), &["libc.so"], &[], &[]);
    packer.add("/bin/hi", S_IFREG | 0o755, program);
    let bytes = packer
        .finish("name hello\nversion 1\n", "command hi\n")
        .unwrap();
    let image = Image::check(&bytes).unwrap();
    let grant = image.declaration.grant();
    let authority = Authority::Person {
        name: "hello",
        grant: &grant,
    };
    let taken = [("other", &["hi".into()][..])];
    let ours = [("hello", &["hi".into()][..])];
    refused(
        install(
            None,
            &image,
            sha256(&bytes),
            &authority,
            &owner.bounds(),
            &taken,
        ),
        "other declares one of its commands",
    );
    assert!(
        install(
            None,
            &image,
            sha256(&bytes),
            &authority,
            &owner.bounds(),
            &ours
        )
        .is_ok()
    );
}
