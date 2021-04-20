// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Lightning message signing and verification lives here. These tools can be used to sign messages using the node's
//! secret so receivers are sure that they come from you. You can also use this to verify that a given message comes
//! from a specific node.
//! Furthermore, these tools can be used to sign / verify messages using ephemeral keys not tied to node's identities.
//!
//! Note this is not part of the specs, but follows lnd's signing and verifying protocol, which can is defined as follows:
//!
//! signature = zbase32(SigRec(sha256d(("Lightning Signed Message:" + msg)))
//! zbase32 from https://philzimmermann.com/docs/human-oriented-base-32-encoding.txt
//! SigRec has first byte 31 + recovery id, followed by 64 byte sig.
//!
//! This implementation is compatible with both lnd's and c-lightning's
//!
//! https://lightning.readthedocs.io/lightning-signmessage.7.html
//! https://api.lightning.community/#signmessage

use crate::util::zbase32;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::secp256k1::recovery::{RecoverableSignature, RecoveryId};
use bitcoin::secp256k1::{Error, Message, PublicKey, Secp256k1, SecretKey};

static LN_MESSAGE_PREFIX: &[u8] = b"Lightning Signed Message:";

fn sigrec_encode(sig_rec: RecoverableSignature) -> Vec<u8> {
    let (rid, rsig) = sig_rec.serialize_compact();
    let prefix = rid.to_i32() as u8 + 31;

    [&[prefix], &rsig[..]].concat()
}

fn sigrec_decode(sig_rec: Vec<u8>) -> Result<RecoverableSignature, Error> {
    let rsig = &sig_rec[1..];
    let rid = sig_rec[0] as i32 - 31;

    match RecoveryId::from_i32(rid) {
        Ok(x) => RecoverableSignature::from_compact(rsig, x),
        Err(e) => Err(e)
    }
}

/// Creates a digital signature of a message given a SecretKey, like the node's secret.
/// A receiver knowing the PublicKey (e.g. the node's id) and the message can be sure that the signature was generated by the caller.
/// Signatures are EC recoverable, meaning that given the message and the signature the PublicKey of the signer can be extracted.
pub fn sign(msg: &[u8], sk: SecretKey) -> Result<String, Error> {
    let secp_ctx = Secp256k1::signing_only();
    let msg_hash = sha256d::Hash::hash(&[LN_MESSAGE_PREFIX, msg].concat());

    let sig = secp_ctx.sign_recoverable(&Message::from_slice(&msg_hash)?, &sk);
    Ok(zbase32::encode(&sigrec_encode(sig)))
}

/// Recovers the PublicKey of the signer of the message given the message and the signature.
pub fn recover_pk(msg: &[u8], sig: &str) ->  Result<PublicKey, Error> {
    let secp_ctx = Secp256k1::verification_only();
    let msg_hash = sha256d::Hash::hash(&[LN_MESSAGE_PREFIX, msg].concat());

    match zbase32::decode(&sig) {
        Ok(sig_rec) => {
            match sigrec_decode(sig_rec) {
                Ok(sig) => secp_ctx.recover(&Message::from_slice(&msg_hash)?, &sig),
                Err(e) => Err(e)
            }
        },
        Err(_) => Err(Error::InvalidSignature)
    }
}

/// Verifies a message was signed by a PrivateKey that derives to a given PublicKey, given a message, a signature,
/// and the PublicKey.
pub fn verify(msg: &[u8], sig: &str, pk: PublicKey) -> bool {
    match recover_pk(msg, sig) {
        Ok(x) => x == pk,
        Err(_) => false
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use util::message_signing::{sign, recover_pk, verify};
    use bitcoin::secp256k1::key::ONE_KEY;
    use bitcoin::secp256k1::{PublicKey, Secp256k1};

    #[test]
    fn test_sign() {
        let message = "test message";
        let zbase32_sig = sign(message.as_bytes(), ONE_KEY);

        assert_eq!(zbase32_sig.unwrap(), "d9tibmnic9t5y41hg7hkakdcra94akas9ku3rmmj4ag9mritc8ok4p5qzefs78c9pqfhpuftqqzhydbdwfg7u6w6wdxcqpqn4sj4e73e")
    }

    #[test]
    fn test_recover_pk() {
        let message = "test message";
        let sig = "d9tibmnic9t5y41hg7hkakdcra94akas9ku3rmmj4ag9mritc8ok4p5qzefs78c9pqfhpuftqqzhydbdwfg7u6w6wdxcqpqn4sj4e73e";
        let pk = recover_pk(message.as_bytes(), sig);

        assert_eq!(pk.unwrap(), PublicKey::from_secret_key(&Secp256k1::signing_only(), &ONE_KEY))
    }

    #[test]
    fn test_verify() {
        let message = "another message";
        let sig = sign(message.as_bytes(), ONE_KEY).unwrap();
        let pk = PublicKey::from_secret_key(&Secp256k1::signing_only(), &ONE_KEY);

        assert!(verify(message.as_bytes(), &sig, pk))
    }

    #[test]
    fn test_verify_ground_truth_ish() {
        // There are no standard tests vectors for Sign/Verify, using the same tests vectors as c-lightning to see if they are compatible.
        // Taken from https://github.com/ElementsProject/lightning/blob/1275af6fbb02460c8eb2f00990bb0ef9179ce8f3/tests/test_misc.py#L1925-L1938

        let corpus = [
            ["@bitconner",
             "is this compatible?",
             "rbgfioj114mh48d8egqx8o9qxqw4fmhe8jbeeabdioxnjk8z3t1ma1hu1fiswpakgucwwzwo6ofycffbsqusqdimugbh41n1g698hr9t",
             "02b80cabdf82638aac86948e4c06e82064f547768dcef977677b9ea931ea75bab5"],
            ["@duck1123",
             "hi",
             "rnrphcjswusbacjnmmmrynh9pqip7sy5cx695h6mfu64iac6qmcmsd8xnsyczwmpqp9shqkth3h4jmkgyqu5z47jfn1q7gpxtaqpx4xg",
             "02de60d194e1ca5947b59fe8e2efd6aadeabfb67f2e89e13ae1a799c1e08e4a43b"],
            ["@jochemin",
             "hi",
             "ry8bbsopmduhxy3dr5d9ekfeabdpimfx95kagdem7914wtca79jwamtbw4rxh69hg7n6x9ty8cqk33knbxaqftgxsfsaeprxkn1k48p3",
             "022b8ece90ee891cbcdac0c1cc6af46b73c47212d8defbce80265ac81a6b794931"],
        ];

        for c in &corpus {
            assert!(verify(c[1].as_bytes(), c[2], PublicKey::from_str(c[3]).unwrap()))
        }
    }
}
