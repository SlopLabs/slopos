use super::*;
use alloc::vec;
use alloc::vec::Vec;
use slopos_abi::caps;
use slopos_abi::fs::S_IFREG;
use slopos_tls_core::ed25519::SigningKey;
use slopos_tls_core::sshsig::{self, PublicKey};

use crate::elf::{Kind, build};
use crate::image::{Packer, Sink};
use crate::{INTERPRETER, NAMESPACE};

struct Dropped;

impl Sink for Dropped {
    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    fn put(&mut self, _: u64, _: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

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
            "action act\n  program /opt/hello/libexec/act\n  args act\n  caps {unasked}\n  allow user\n  describe \"act on the network\"\n"
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
            sha256: image.digest,
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
        self.whole(record, bytes, &authority)
    }

    /// As the mechanism admits: policy, then the copy, then its contents.
    fn whole(
        &self,
        record: Option<&Record>,
        bytes: &[u8],
        authority: &Authority<'_>,
    ) -> Result<Admitted, Refusal> {
        let mut image = Image::open(bytes, None).unwrap();
        let admitted = install(record, &image, authority, &self.bounds(), &[])?;
        if matches!(admitted, Admitted::Install { .. }) {
            image.copy(&mut Dropped).unwrap();
            contents(&mut image, &self.libc)?;
        }
        Ok(admitted)
    }

    fn ahead(&self, record: Option<&Record>, bytes: &[u8]) -> Result<Option<Admitted>, Refusal> {
        let (sidecar, signature) = self.sign(bytes);
        let (said, key) = signer(&sidecar, &signature, &self.keys)?;
        ahead(record, &said, key)
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
            signer: None,
            signed: None,
        };
        self.whole(record, bytes, &authority)
    }

    fn approved_signed(
        &self,
        record: Option<&Record>,
        bytes: &[u8],
        grant: &Grant,
        shown: Option<&str>,
    ) -> Result<Admitted, Refusal> {
        let (sidecar, signature) = self.sign(bytes);
        let authority = Authority::Person {
            name: "hello",
            grant,
            signer: shown,
            signed: Some((&sidecar, &signature)),
        };
        self.whole(record, bytes, &authority)
    }

    fn fingerprint(&self) -> String {
        PublicKey(self.key.public()).fingerprint()
    }

    fn runs(&self, record: &Record, bytes: &[u8]) -> Result<(), Refusal> {
        let (sidecar, signature) = self.sign(bytes);
        let mut image = Image::check(bytes).unwrap();
        activation(
            record,
            &mut image,
            Some((&sidecar, &signature)),
            &self.bounds(),
            None,
        )
        .map(drop)
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

fn admitted(admitted: Result<Admitted, Refusal>) -> (Installed, Option<String>) {
    match admitted {
        Ok(Admitted::Install { image, signer }) => (image, signer),
        other => panic!("not installed: {other:?}"),
    }
}

fn installed(admitted: Result<Admitted, Refusal>) -> Installed {
    self::admitted(admitted).0
}

fn put(record: &mut Record, admitted: Result<Admitted, Refusal>) {
    let (image, signer) = self::admitted(admitted);
    record.install(image, signer);
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
        "SetId, which no package",
    );
    let mut record = Record::new("hello");
    record.install(got, None);
    let mut runs = record.clone();
    runs.current.as_mut().unwrap().signer = None;
    let mut image = Image::check(&bytes[..]).unwrap();
    let needs = activation(&runs, &mut image, None, &owner.bounds(), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        activation(&runs, &mut image, None, &owner.bounds(), Some(&needs)),
        Ok(None)
    );
    let lacking = Owner {
        libc: build(Kind::Shared, None, &[], &[], &["gets"]),
        ..Owner::new(0, 0)
    };
    refused(
        activation(&runs, &mut image, None, &lacking.bounds(), Some(&needs))
            .map(|_| Admitted::Current),
        "binds puts",
    );
    assert_eq!(symbols::Needs::parse(&needs.render()), Some(needs));
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
    put(&mut record, owner.signed(None, &v1));
    assert_eq!(owner.signed(Some(&record), &v1), Ok(Admitted::Current));
    let got = owner.signed(Some(&record), &v2);
    put(&mut record, got);
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
        "another publisher's image of hello",
    );
}

#[test]
fn a_name_s_images_are_one_publisher_s_until_it_is_removed() {
    let owner = Owner::new(CEILING, 0);
    let other = Owner::of(3, CEILING, 0);
    let (v1, v2) = (
        image(1, "NetAdmin", "", &["puts"]),
        image(2, "NetAdmin", "", &["puts"]),
    );
    let approve = grant(caps::NET_ADMIN);
    let mut by_person = Record::new("hello");
    put(&mut by_person, owner.approved(None, &v1, &approve));
    assert_eq!(by_person.signer, None);
    refused(
        owner.signed(Some(&by_person), &v2),
        "nobody signed the one installed",
    );
    refused(
        owner.approved_signed(Some(&by_person), &v2, &approve, Some(&owner.fingerprint())),
        "another publisher's image",
    );
    let mut by_key = Record::new("hello");
    put(&mut by_key, owner.signed(None, &v1));
    refused(
        owner.approved(Some(&by_key), &v2, &approve),
        "and nobody this one",
    );
    assert_eq!(
        owner.approved(Some(&by_key), &v1, &approve),
        Ok(Admitted::Current)
    );
    refused(
        other.approved_signed(Some(&by_key), &v2, &approve, Some(&other.fingerprint())),
        "it goes in only once `pkgctl remove hello` gives up the name",
    );
    let (image2, signer) =
        admitted(owner.approved_signed(Some(&by_key), &v2, &approve, Some(&owner.fingerprint())));
    assert_eq!((image2.signer, signer), (None, Some(owner.fingerprint())));
    for record in [&mut by_person, &mut by_key] {
        record.remove();
        assert_eq!(record.floor, None);
        refused(owner.signed(Some(record), &v2), "it was removed");
        let got = other.approved_signed(Some(record), &v1, &approve, Some(&other.fingerprint()));
        put(record, got);
        assert_eq!(record.signer, Some(other.fingerprint()));
    }
    let got = other.signed(Some(&by_key), &v2);
    put(&mut by_key, got);
    assert_eq!(by_key.current.as_ref().map(|c| c.version), Some(2));
}

#[test]
fn a_person_s_install_names_who_signed_it() {
    let owner = Owner::new(CEILING, 0);
    let stranger = Owner {
        keys: Keys::default(),
        ..Owner::of(2, CEILING, 0)
    };
    let bytes = image(1, "NetAdmin", "", &["puts"]);
    let approve = grant(caps::NET_ADMIN);
    let (image1, signer) =
        admitted(stranger.approved_signed(None, &bytes, &approve, Some(&stranger.fingerprint())));
    assert_eq!(
        (image1.signer, signer),
        (None, Some(stranger.fingerprint()))
    );
    refused(
        stranger.approved_signed(None, &bytes, &approve, None),
        "signed by SHA256:",
    );
    refused(
        stranger.approved_signed(None, &bytes, &approve, Some(&owner.fingerprint())),
        "as was approved",
    );
    refused(
        owner.whole(
            None,
            &bytes,
            &Authority::Person {
                name: "hello",
                grant: &approve,
                signer: Some(&owner.fingerprint()),
                signed: None,
            },
        ),
        "signed by nobody, not by SHA256:",
    );
    let (sidecar, _) = owner.sign(&image(2, "NetAdmin", "", &["puts"]));
    let (_, signature) = owner.sign(&bytes);
    refused(
        owner.whole(
            None,
            &bytes,
            &Authority::Person {
                name: "hello",
                grant: &approve,
                signer: Some(&owner.fingerprint()),
                signed: Some((&sidecar, &signature)),
            },
        ),
        "describes another image",
    );
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
    put(&mut record, owner.signed(None, &v1));
    let got = owner.signed(Some(&record), &v2);
    put(&mut record, got);
    record.roll_back().unwrap();
    assert_eq!(record.current.as_ref().map(|c| c.version), Some(1));
    assert_eq!(
        (record.floor.map(|(v, _)| v), record.refused.clone()),
        (Some(1), vec![2])
    );
    refused(owner.signed(Some(&record), &v2), "rolled back from");
    let back = installed(owner.approved_signed(
        Some(&record),
        &v2,
        &grant(caps::NET_ADMIN),
        Some(&owner.fingerprint()),
    ));
    assert_eq!(back.version, 2);
    let got = owner.signed(Some(&record), &v3);
    put(&mut record, got);
    assert!(record.refused.is_empty());
    record.roll_back().unwrap();
    record.roll_back().unwrap();
    assert_eq!(
        (
            record.current.as_ref().map(|c| c.version),
            record.refused.clone()
        ),
        (Some(3), vec![1])
    );
    assert_eq!(owner.runs(&record, &v3), Ok(()));
    record.remove();
    refused(owner.signed(Some(&record), &v3), "it was removed");
    assert!(record.refused.is_empty());
    assert!(matches!(
        owner.approved(Some(&record), &v2, &grant(caps::NET_ADMIN)),
        Ok(Admitted::Install { .. })
    ));
}

#[test]
fn a_signed_install_judged_ahead_is_judged_as_it_is_whole() {
    let owner = Owner::new(CEILING, 0);
    let agree = |record: Option<&Record>, bytes: &[u8]| match (
        owner.ahead(record, bytes),
        owner.signed(record, bytes),
    ) {
        (Ok(Some(ahead)), Ok(whole)) => assert_eq!(ahead, whole),
        (Ok(None), Ok(Admitted::Install { .. })) => {}
        (Err(ahead), Err(whole)) => assert_eq!(ahead, whole),
        (ahead, whole) => panic!("ahead {ahead:?}, whole {whole:?}"),
    };
    let (v1, v2, v3) = (
        image(1, "NetAdmin", "", &["puts"]),
        image(2, "NetAdmin", "", &["puts"]),
        image(3, "NetAdmin", "", &["puts"]),
    );
    let mut record = Record::new("hello");
    agree(None, &v1);
    put(&mut record, owner.signed(None, &v1));
    agree(Some(&record), &v1);
    agree(Some(&record), &v2);
    let got = owner.signed(Some(&record), &v2);
    put(&mut record, got);
    agree(Some(&record), &v1);
    agree(Some(&record), &image(2, "Power", "", &["puts"]));
    record.roll_back().unwrap();
    agree(Some(&record), &v2);
    record.remove();
    agree(Some(&record), &v3);
}

#[test]
fn a_key_s_update_leaves_a_disabled_package_disabled() {
    let owner = Owner::new(CEILING, 0);
    let mut record = Record::new("hello");
    let (v1, v2, v3) = (
        image(1, "NetAdmin", "", &["puts"]),
        image(2, "NetAdmin", "", &["puts"]),
        image(3, "NetAdmin", "", &["puts"]),
    );
    put(&mut record, owner.signed(None, &v1));
    assert!(record.enabled);
    record.enabled = false;
    let got = owner.signed(Some(&record), &v2);
    put(&mut record, got);
    assert!(!record.enabled, "a key re-enabled what the person disabled");
    record.stop("its key's ceiling fell");
    let got = owner.signed(Some(&record), &v3);
    put(&mut record, got);
    assert!(
        record.enabled,
        "a fitting update left a stopped package stopped"
    );
    record.enabled = false;
    record.why = None;
    let v4 = image(4, "NetAdmin", "", &["puts"]);
    let got = owner.approved_signed(
        Some(&record),
        &v4,
        &grant(caps::NET_ADMIN),
        Some(&owner.fingerprint()),
    );
    put(&mut record, got);
    assert!(record.enabled);
}

#[test]
fn an_activation_holds_the_image_to_its_record_and_its_key() {
    let owner = Owner::new(CEILING, 0);
    let bytes = image(1, "NetAdmin", "", &["puts"]);
    let mut record = Record::new("hello");
    put(&mut record, owner.signed(None, &bytes));
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
    let mut image = Image::check(&bytes[..]).unwrap();
    refused(
        activation(&record, &mut image, None, &owner.bounds(), None).map(|_| Admitted::Current),
        "signature is not in the store",
    );
}

#[test]
fn an_action_that_breaks_the_copy_rules_is_no_image() {
    let pack = |said: &str| {
        let mut packer = Packer::default();
        packer.add("/libexec/act", S_IFREG | 0o755, Vec::new());
        let declare = alloc::format!(
            "user\naction act\n  program /opt/hello/libexec/act\n  args act\n  caps Power\n  allow user\n  {said}\n"
        );
        packer.finish("name hello\nversion 1\n", &declare)
    };
    assert!(pack("describe \"act on the network\"").is_ok());
    for (said, line) in [
        ("describe \"Act on the network\"", 7),
        ("describe \"act on the machine\"", 7),
        ("describe \"use RawBlock\"", 7),
        ("describe \"act\"\n  verb \"Disallow\"", 8),
        ("describe \"act\"\n  detail \"it acts\"", 8),
    ] {
        let why = pack(said).unwrap_err();
        assert!(
            why.contains(&alloc::format!("line {line}:")),
            "{said}: {why}"
        );
    }
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
fn two_packages_never_declare_one_command() {
    let owner = Owner::new(0, 0);
    let mut packer = Packer::default();
    let program = build(Kind::Executable, Some(INTERPRETER), &["libc.so"], &[], &[]);
    packer.add("/bin/hi", S_IFREG | 0o755, program);
    let bytes = packer
        .finish("name hello\nversion 1\n", "command hi\n")
        .unwrap();
    let image = Image::check(&bytes[..]).unwrap();
    let grant = image.declaration.grant();
    let authority = Authority::Person {
        name: "hello",
        grant: &grant,
        signer: None,
        signed: None,
    };
    let theirs = Grant {
        commands: vec!["hi".into()],
        ..Grant::default()
    };
    let taken = [("other", &theirs)];
    let ours = [("hello", &theirs)];
    refused(
        install(None, &image, &authority, &owner.bounds(), &taken),
        "other declares the command hi",
    );
    assert!(install(None, &image, &authority, &owner.bounds(), &ours).is_ok());
}

#[test]
fn two_packages_never_declare_one_user() {
    let owner = Owner::new(CEILING, 0);
    let bytes = image(1, "NetAdmin", "", &["puts"]);
    let image = Image::check(&bytes[..]).unwrap();
    let grant = image.declaration.grant();
    let authority = Authority::Person {
        name: "hello",
        grant: &grant,
        signer: None,
        signed: None,
    };
    let theirs = Grant {
        users: vec!["hel-lo".into(), "hello".into()],
        ..Grant::default()
    };
    refused(
        install(
            None,
            &image,
            &authority,
            &owner.bounds(),
            &[("hel", &theirs)],
        ),
        "hel declares the user hello",
    );
}

fn offered(version: u64) -> Vec<u8> {
    let mut packer = Packer::default();
    let program = build(Kind::Executable, Some(INTERPRETER), &["libc.so"], &[], &[]);
    packer.add("/bin/hi", S_IFREG | 0o755, program);
    packer
        .finish(
            &alloc::format!("name hello\nversion {version}\n"),
            "command hi\n",
        )
        .unwrap()
}

fn sidecar_of(bytes: &[u8]) -> Sidecar {
    let image = Image::check(bytes).unwrap();
    Sidecar {
        name: image.release.name.clone(),
        version: image.release.version,
        sha256: image.digest,
    }
}

#[test]
fn an_offered_image_goes_in_unasked_only_asking_for_nothing() {
    let owner = Owner::new(CEILING, 0);
    let got = installed(owner.whole(None, &offered(1), &Authority::Offered));
    assert_eq!((got.signer, got.grant.commands), (None, vec!["hi".into()]));
    refused(
        owner.whole(
            None,
            &image(1, "NetAdmin", "", &["puts"]),
            &Authority::Offered,
        ),
        "asks for a user",
    );
}

#[test]
fn an_offer_goes_above_the_floor_and_over_nothing_a_person_or_key_decided() {
    let owner = Owner::new(CEILING, 0);
    let (v1, v2) = (offered(1), offered(2));
    let mut record = Record::new("hello");
    record.install_offered(installed(owner.whole(None, &v1, &Authority::Offered)));
    assert!(record.enabled);
    assert_eq!(
        ahead_offered(Some(&record), &sidecar_of(&v1)),
        Ok(Some(Admitted::Current))
    );
    assert_eq!(ahead_offered(Some(&record), &sidecar_of(&v2)), Ok(None));
    record.enabled = false;
    record.install_offered(installed(owner.whole(
        Some(&record),
        &v2,
        &Authority::Offered,
    )));
    assert!(
        !record.enabled,
        "an offer re-enabled what the person disabled"
    );
    refused(
        owner.whole(Some(&record), &v1, &Authority::Offered),
        "older than version 2",
    );
    record.roll_back().unwrap();
    refused(
        owner.whole(Some(&record), &v2, &Authority::Offered),
        "version 2 was rolled back from",
    );
    record.remove();
    refused(
        ahead_offered(Some(&record), &sidecar_of(&offered(3))).map(|_| Admitted::Current),
        "it was removed",
    );
    let mut signed = Record::new("hello");
    put(&mut signed, owner.signed(None, &v1));
    refused(
        owner.whole(Some(&signed), &v2, &Authority::Offered),
        "another publisher's image",
    );
}
