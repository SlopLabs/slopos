//! Against `ssh-keygen -Y`, in both directions, and a signature it made.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::String;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::{format, fs};

use super::*;
use crate::ed25519::SigningKey;
use crate::tests::unhex;

const NAMESPACE: &str = "extension@slopos";

/// RFC 8032's TEST 1 key, which signed `abc` here with `ssh-keygen -Y sign
/// -n extension@slopos -O hashalg=sha512` (OpenSSH 9.6p1).
const TEST1_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const TEST1_LINE: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea";
const TEST1_FINGERPRINT: &str = "SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8";
const TEST1_ABC: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAg11qYAYKxCrfVS/7TyWQHOg7hcv
PapiMlrwIaaPcHURoAAAAQZXh0ZW5zaW9uQHNsb3BvcwAAAAAAAAAGc2hhNTEyAAAAUwAA
AAtzc2gtZWQyNTUxOQAAAEDQ/kAMtIFPKjxhZ9X7jxEP4XHEd8RthFxcpJBteRSSv/vYKl
5UvFvJVOKj/4KZAxBH4J2DyMubNC2kbITob0AO
-----END SSH SIGNATURE-----
";

fn test1() -> SigningKey {
    SigningKey::from_seed(&unhex(TEST1_SEED).try_into().expect("seed"))
}

fn line_key(line: &str) -> Option<PublicKey> {
    let mut words = line.split_ascii_whitespace();
    PublicKey::from_openssh(words.next()?, words.next()?)
}

#[test]
fn ssh_keygen_s_signature_verifies_and_names_its_key() {
    let key = verify(TEST1_ABC.as_bytes(), NAMESPACE, b"abc").expect("verifies");
    assert_eq!(key, PublicKey(test1().public()));
    assert_eq!(Some(key), line_key(TEST1_LINE));
    assert_eq!(key.fingerprint(), TEST1_FINGERPRINT);
    assert_eq!(format!("{KEY_TYPE} {}", key.base64()), TEST1_LINE);
}

/// Ed25519 is deterministic, so the same key over the same message writes
/// the same bytes `ssh-keygen` did.
#[test]
fn a_signature_made_here_is_ssh_keygen_s_byte_for_byte() {
    assert_eq!(
        String::from_utf8(sign(&test1(), NAMESPACE, b"abc")).expect("ASCII"),
        TEST1_ABC
    );
}

#[test]
fn what_another_namespace_message_or_key_signed_is_refused() {
    let armored = TEST1_ABC.as_bytes();
    assert_eq!(verify(armored, "file", b"abc"), Err(Error::Namespace));
    assert_eq!(verify(armored, "", b"abc"), Err(Error::Namespace));
    assert_eq!(verify(armored, NAMESPACE, b"abd"), Err(Error::Signature));
    let other = SigningKey::from_seed(&[9; 32]);
    let mut blob = dearmor(&sign(&other, NAMESPACE, b"abc")).expect("armor");
    let theirs = dearmor(armored).expect("armor");
    blob[10..65].copy_from_slice(&theirs[10..65]);
    let swapped = crate::pem::armor("SSH SIGNATURE", &blob, 70);
    assert_eq!(verify(&swapped, NAMESPACE, b"abc"), Err(Error::Signature));
}

#[test]
fn a_blob_out_of_shape_is_refused() {
    let blob = dearmor(TEST1_ABC.as_bytes()).expect("armor");
    let rearmor = |blob: &[u8]| crate::pem::armor("SSH SIGNATURE", blob, 70);
    let mut trailing = blob.clone();
    trailing.push(0);
    assert_eq!(
        verify(&rearmor(&trailing), NAMESPACE, b"abc"),
        Err(Error::Malformed)
    );
    for cut in [0, 6, 10, 40, blob.len() - 1] {
        assert_eq!(
            verify(&rearmor(&blob[..cut]), NAMESPACE, b"abc"),
            Err(Error::Malformed),
            "cut at {cut}"
        );
    }
    let mut version = blob.clone();
    version[9] = 2;
    assert_eq!(
        verify(&rearmor(&version), NAMESPACE, b"abc"),
        Err(Error::Version)
    );
    let mut key_type = blob.clone();
    key_type[18] = b'X';
    assert_eq!(
        verify(&rearmor(&key_type), NAMESPACE, b"abc"),
        Err(Error::KeyType)
    );
    let at = blob
        .windows(6)
        .position(|w| w == b"sha512")
        .expect("hash name");
    let mut hash = blob.clone();
    hash[at + 5] = b'1';
    assert_eq!(
        verify(&rearmor(&hash), NAMESPACE, b"abc"),
        Err(Error::HashAlgorithm)
    );
    assert_eq!(verify(b"not armor", NAMESPACE, b"abc"), Err(Error::Armor));
    let unclosed = TEST1_ABC.replace("-----END SSH SIGNATURE-----", "");
    assert_eq!(
        verify(unclosed.as_bytes(), NAMESPACE, b"abc"),
        Err(Error::Armor)
    );
}

#[test]
fn a_public_key_line_is_read_strictly() {
    assert!(line_key(TEST1_LINE).is_some());
    let fields: Vec<&str> = TEST1_LINE.split(' ').collect();
    assert_eq!(PublicKey::from_openssh("ssh-rsa", fields[1]), None);
    assert_eq!(PublicKey::from_openssh(KEY_TYPE, &fields[1][..60]), None);
    assert_eq!(PublicKey::from_openssh(KEY_TYPE, "AAAA"), None);
}

fn scratch() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("slopos-sshsig-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn ssh_keygen(args: &[&str], dir: &Path) -> String {
    let out = Command::new("ssh-keygen")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the interop tests need the ssh-keygen command");
    assert!(
        out.status.success(),
        "ssh-keygen {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8")
}

#[test]
fn ssh_keygen_and_this_verify_each_other() {
    let dir = scratch();
    ssh_keygen(
        &["-q", "-t", "ed25519", "-N", "", "-C", "owner", "-f", "id"],
        &dir,
    );
    let line = fs::read_to_string(dir.join("id.pub")).expect("id.pub");
    let theirs = line_key(&line).expect("an ed25519 key line");
    let listed = ssh_keygen(&["-l", "-f", "id.pub"], &dir);
    assert_eq!(
        listed.split(' ').nth(1),
        Some(theirs.fingerprint().as_str())
    );
    let message = b"name hello\nversion 7\n".repeat(40);
    fs::write(dir.join("meta"), &message).expect("meta");
    for hash in ["sha512", "sha256"] {
        ssh_keygen(
            &["-Y", "sign", "-q", "-f", "id", "-n", NAMESPACE, "-O"]
                .into_iter()
                .chain([format!("hashalg={hash}").as_str(), "meta"])
                .collect::<Vec<_>>(),
            &dir,
        );
        let signed = fs::read(dir.join("meta.sig")).expect("meta.sig");
        assert_eq!(verify(&signed, NAMESPACE, &message), Ok(theirs), "{hash}");
        assert_eq!(verify(&signed, "file", &message), Err(Error::Namespace));
        fs::remove_file(dir.join("meta.sig")).expect("meta.sig");
    }
    let mine = SigningKey::from_seed(&[0x42; 32]);
    let public = PublicKey(mine.public());
    fs::write(
        dir.join("allowed"),
        format!("me@slopos {KEY_TYPE} {}\n", public.base64()),
    )
    .expect("allowed_signers");
    fs::write(dir.join("ours.sig"), sign(&mine, NAMESPACE, &message)).expect("ours.sig");
    let verified = Command::new("ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            "allowed",
            "-I",
            "me@slopos",
            "-n",
            NAMESPACE,
        ])
        .args(["-s", "ours.sig"])
        .current_dir(&dir)
        .stdin(fs::File::open(dir.join("meta")).expect("meta"))
        .output()
        .expect("ssh-keygen");
    assert!(
        verified.status.success(),
        "ssh-keygen -Y verify: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
}
