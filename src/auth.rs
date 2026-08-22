use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use ssh_key::{AuthorizedKeys, HashAlg, PrivateKey, PublicKey, SshSig};

use crate::SSHSIG_NAMESPACE;

pub fn authentication_payload(challenge: &[u8], username: &str, server_instance: &str) -> Vec<u8> {
    let mut payload =
        Vec::with_capacity(32 + challenge.len() + username.len() + server_instance.len());
    payload.extend_from_slice(b"ASTRA-AUTH-V1\0");
    append_field(&mut payload, challenge);
    append_field(&mut payload, username.as_bytes());
    append_field(&mut payload, server_instance.as_bytes());
    payload
}

fn append_field(output: &mut Vec<u8>, field: &[u8]) {
    output.extend_from_slice(&(field.len() as u32).to_be_bytes());
    output.extend_from_slice(field);
}

pub fn sign_challenge(identity: &Path, challenge: &[u8]) -> Result<(String, String)> {
    check_private_key_permissions(identity)?;
    let private = PrivateKey::read_openssh_file(identity)
        .with_context(|| format!("failed to read OpenSSH private key {}", identity.display()))?;
    if private.is_encrypted() {
        bail!("encrypted private keys are not supported by the MVP; use an ssh-agent-free test key")
    }
    let signature = private
        .sign(SSHSIG_NAMESPACE, HashAlg::Sha512, challenge)
        .context("failed to sign server challenge")?;
    Ok((
        private.public_key().to_openssh()?,
        signature.to_pem(ssh_key::LineEnding::LF)?,
    ))
}

pub fn verify_authorized_key(
    authorized_keys: &Path,
    public_key: &str,
    signature_pem: &str,
    challenge: &[u8],
) -> Result<String> {
    let authorized_keys = [authorized_keys.to_path_buf()];
    verify_authorized_keys(&authorized_keys, public_key, signature_pem, challenge)
}

pub fn verify_authorized_keys(
    authorized_keys: &[std::path::PathBuf],
    public_key: &str,
    signature_pem: &str,
    challenge: &[u8],
) -> Result<String> {
    let offered = PublicKey::from_openssh(public_key).context("invalid offered SSH public key")?;
    let signature = SshSig::from_pem(signature_pem).context("invalid SSHSIG signature")?;

    let mut authorized = false;
    for path in authorized_keys {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read authorized_keys {}", path.display()))?;
        for entry in AuthorizedKeys::new(&contents) {
            let entry = entry.context("invalid authorized_keys entry")?;
            if entry.public_key().key_data() == offered.key_data() {
                if !entry.config_opts().is_empty() {
                    bail!("authorized_keys options are not supported for the matching key")
                }
                authorized = true;
                break;
            }
        }
        if authorized {
            break;
        }
    }
    if !authorized {
        bail!("public key is not authorized")
    }
    offered
        .verify(SSHSIG_NAMESPACE, challenge, &signature)
        .context("SSH signature verification failed")?;
    Ok(offered.fingerprint(HashAlg::Sha256).to_string())
}

#[cfg(unix)]
fn check_private_key_permissions(identity: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(identity)
        .with_context(|| format!("failed to stat {}", identity.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "private key {} has permissions {:o}; expected no group/other access",
            identity.display(),
            mode & 0o777
        )
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_key_permissions(_identity: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_key::{Algorithm, private::PrivateKey};

    #[test]
    fn accepts_authorized_signature_and_rejects_wrong_challenge() {
        assert_authorized_round_trip(Algorithm::Ed25519);
    }

    #[test]
    fn accepts_rsa_sha2_512_authorized_signature() {
        let dir = tempfile::tempdir().unwrap();
        let authorized = dir.path().join("authorized_keys");
        fs::write(&authorized, RSA_AUTHORIZED_KEY).unwrap();

        verify_authorized_key(&authorized, RSA_AUTHORIZED_KEY, RSA_SIGNATURE, b"challenge")
            .unwrap();
        assert!(
            verify_authorized_key(&authorized, RSA_AUTHORIZED_KEY, RSA_SIGNATURE, b"other")
                .is_err()
        );
    }

    fn assert_authorized_round_trip(algorithm: Algorithm) {
        let mut rng = ssh_key::rand_core::OsRng;
        let key = PrivateKey::random(&mut rng, algorithm).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let authorized = dir.path().join("authorized_keys");
        fs::write(&authorized, key.public_key().to_openssh().unwrap()).unwrap();
        let challenge = b"challenge";
        let signature = key
            .sign(SSHSIG_NAMESPACE, HashAlg::Sha512, challenge)
            .unwrap()
            .to_pem(ssh_key::LineEnding::LF)
            .unwrap();

        assert!(
            verify_authorized_key(
                &authorized,
                &key.public_key().to_openssh().unwrap(),
                &signature,
                challenge,
            )
            .is_ok()
        );
        assert!(
            verify_authorized_key(
                &authorized,
                &key.public_key().to_openssh().unwrap(),
                &signature,
                b"other",
            )
            .is_err()
        );
    }

    #[test]
    fn authentication_transcript_binds_username_and_server() {
        let challenge = [7_u8; 32];
        assert_ne!(
            authentication_payload(&challenge, "alice", "server-a"),
            authentication_payload(&challenge, "bob", "server-a")
        );
        assert_ne!(
            authentication_payload(&challenge, "alice", "server-a"),
            authentication_payload(&challenge, "alice", "server-b")
        );
    }

    const RSA_AUTHORIZED_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDEPC5SFWDm55oOA5bGB0lalZPTY8e8E9n2UwNVYFUYFHsDqk4TSebZsycRQj53mnRnnJJh3nIqwXpsc3hl53UqWDTc/jOnIl3ggakrpAdMBGDLw1x45TMQ7aULB3VbY5G9AIJfno5hi8r9NtB/xTmYxZUdhPHu/G/WgKhi1M+9Tq2aFAVbAjXsiPMpZ/KX/2e39SFWbyyuZeIjSDYZolLe0N+EVoUeGkEU8AB7QD1oFhLKfhzp4CgAl75/LUFbwjtf6x9JW0m52TQXktMPLnOHVrd9VN11GuCJMahFTRQjJCWg9DS8j07BIjB73a8EeDz1KvUBTgr7oMgMyPp/NDUL astra-rsa-test";

    const RSA_SIGNATURE: &str = r#"-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAARcAAAAHc3NoLXJzYQAAAAMBAAEAAAEBAMQ8LlIVYObnmg4DlsYHSV
qVk9Njx7wT2fZTA1VgVRgUewOqThNJ5tmzJxFCPneadGeckmHecirBemxzeGXndSpYNNz+
M6ciXeCBqSukB0wEYMvDXHjlMxDtpQsHdVtjkb0Agl+ejmGLyv020H/FOZjFlR2E8e78b9
aAqGLUz71OrZoUBVsCNeyI8yln8pf/Z7f1IVZvLK5l4iNINhmiUt7Q34RWhR4aQRTwAHtA
PWgWEsp+HOngKACXvn8tQVvCO1/rH0lbSbnZNBeS0w8uc4dWt31U3XUa4IkxqEVNFCMkJa
D0NLyPTsEiMHvdrwR4PPUq9QFOCvugyAzI+n80NQsAAAATYXN0cmEtc2hlbGwtYXV0aC12
MQAAAAAAAAAGc2hhNTEyAAABFAAAAAxyc2Etc2hhMi01MTIAAAEAFSUQhyYw9m0gMP1My2
F947VJ6rEabYarZKRImG95XLvvsLh+oton8ggJNTkIYX3GQM6UFm38eN/TQMF90Ls+idUt
HAwl5EDXM7WYhFRaZEV8wiXxpHtcVTr3hRHasMAIJcK5VT0QWnww6b8z/aJsAzyv8dLtUr
Ht/feM8HLlmJs0R+sC5gXrAgFGNSXT0UYvjpVX/SzZ8ykmWFm2k1r1ZCBYXvpytdFfPQew
LYIqdhihuvMRihJPyqhXZGSrNVwkaTrtneyRFMi4Mad4rlHFFuPS4+q4msJ2FfhXGZh8+8
r852yAd+Ic93alI6J1RtI6sI06Wv6nY+MfUg+LawwDAQ==
-----END SSH SIGNATURE-----"#;
}
