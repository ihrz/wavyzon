//! DAVE end-to-end encryption, driven by the voice gateway.
//!
//! This is the Rust counterpart of godave's libdave session manager, built on
//! [`davey`]. It never touches the socket itself: every handler returns the
//! messages the gateway must send ([`DaveOut`]), in order.
//!
//! The flow follows libdave's `DaveSessionManager` sample:
//! - select-protocol ack (session description) with version > 0 prepares
//!   epoch 1: (re)initialise the MLS session and send a key package; with
//!   version 0, apply the passthrough transition 0 immediately.
//! - external sender (25) is remembered and installed in the session.
//! - proposals (27) are committed; the commit (+ welcome) goes out as 28.
//! - announce-commit (29) / welcome (30) prepare the transition they carry
//!   and acknowledge it with 23; on failure send 31 and start over.
//! - prepare-transition (21) prepares and acknowledges, execute (22) switches
//!   the encryption key, prepare-epoch (24) with epoch 1 starts a new group.

use crate::Error;
use crate::protocol::Opcode;

/// A message the DAVE state machine asks the gateway to send.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "dave"), allow(dead_code))]
pub(crate) enum DaveOut {
    /// A client binary frame (key package 26, commit-welcome 28).
    Binary(Opcode, Vec<u8>),
    /// JSON op 23 `{transition_id}`.
    TransitionReady(u16),
    /// JSON op 31 `{transition_id}`.
    InvalidCommitWelcome(u16),
}

/// Whether this build can do DAVE.
pub fn dave_available() -> bool {
    cfg!(feature = "dave")
}

/// Whether the sender must hold audio: a DAVE session exists, the call
/// negotiated E2EE (protocol > 0) and the session cannot encrypt yet.
pub(crate) fn should_hold(has_session: bool, protocol_version: u16, ready: bool) -> bool {
    has_session && protocol_version > 0 && !ready
}

#[cfg(feature = "dave")]
pub(crate) use imp::Dave;
#[cfg(not(feature = "dave"))]
pub(crate) use stub::Dave;

#[cfg(feature = "dave")]
mod imp {
    use std::collections::HashSet;
    use std::num::NonZeroU16;

    use davey::{Codec, DaveSession, MediaType, ProposalsOperationType};
    use tracing::{debug, info, warn};

    use super::*;

    const INIT_TRANSITION_ID: u16 = 0;
    const NEW_GROUP_EPOCH: u64 = 1;

    /// The DAVE state of one voice connection.
    pub(crate) struct Dave {
        user_id: u64,
        channel_id: u64,
        session: Option<DaveSession>,
        external_sender: Option<Vec<u8>>,
        recognized: HashSet<u64>,
    }

    impl Dave {
        pub fn new(user_id: &str, channel_id: &str) -> Result<Dave, Error> {
            let user_id = user_id
                .parse()
                .map_err(|_| Error::BadSnowflake("user", user_id.to_string()))?;
            let channel_id = channel_id
                .parse()
                .map_err(|_| Error::BadSnowflake("channel", channel_id.to_string()))?;
            let mut recognized = HashSet::new();
            recognized.insert(user_id);
            Ok(Dave {
                user_id,
                channel_id,
                session: None,
                external_sender: None,
                recognized,
            })
        }

        /// The highest DAVE protocol version we support (sent in identify).
        pub fn max_protocol_version() -> u16 {
            davey::DAVE_PROTOCOL_VERSION
        }

        /// Whether the session has a key to encrypt with.
        pub fn ready(&self) -> bool {
            self.session.as_ref().is_some_and(DaveSession::can_encrypt)
        }

        fn protocol_version(&self) -> u16 {
            self.session.as_ref().map_or(0, |s| s.protocol_version().get())
        }

        fn recognized(&self) -> Vec<u64> {
            self.recognized.iter().copied().collect()
        }

        pub fn add_user(&mut self, id: &str) {
            if let Ok(id) = id.parse() {
                self.recognized.insert(id);
            }
        }

        pub fn remove_user(&mut self, id: &str) {
            if let Ok(id) = id.parse::<u64>()
                && id != self.user_id
            {
                self.recognized.remove(&id);
            }
        }

        pub fn on_select_protocol_ack(&mut self, version: u16) -> Vec<DaveOut> {
            debug!(version, "dave: select protocol ack");
            let mut out = Vec::new();
            self.protocol_init(version, &mut out);
            out
        }

        fn protocol_init(&mut self, version: u16, out: &mut Vec<DaveOut>) {
            if version > 0 {
                self.prepare_epoch(NEW_GROUP_EPOCH, version, out);
            } else {
                self.prepare_transition(INIT_TRANSITION_ID, 0, out);
            }
        }

        pub fn on_prepare_epoch(&mut self, epoch: u64, version: u16) -> Vec<DaveOut> {
            let mut out = Vec::new();
            self.prepare_epoch(epoch, version, &mut out);
            out
        }

        fn prepare_epoch(&mut self, epoch: u64, version: u16, out: &mut Vec<DaveOut>) {
            if epoch != NEW_GROUP_EPOCH {
                return;
            }
            let Some(version) = NonZeroU16::new(version) else {
                return;
            };
            let res = match &mut self.session {
                Some(s) => s
                    .reinit(version, self.user_id, self.channel_id, None)
                    .map_err(|e| e.to_string()),
                None => match DaveSession::new(version, self.user_id, self.channel_id, None) {
                    Ok(mut s) => {
                        if let Some(es) = &self.external_sender
                            && let Err(e) = s.set_external_sender(es)
                        {
                            warn!(error = %e, "dave: stored external sender rejected");
                        }
                        self.session = Some(s);
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                },
            };
            if let Err(e) = res {
                warn!(error = %e, version, "dave: session init failed");
                return;
            }
            info!(version, "dave: new MLS group expected, sending key package");
            self.send_key_package(out);
        }

        fn send_key_package(&mut self, out: &mut Vec<DaveOut>) {
            let Some(s) = &mut self.session else { return };
            match s.create_key_package() {
                Ok(kp) => out.push(DaveOut::Binary(Opcode::DAVE_KEY_PACKAGE, kp)),
                Err(e) => warn!(error = %e, "dave: key package creation failed"),
            }
        }

        pub fn on_prepare_transition(&mut self, transition_id: u16, version: u16) -> Vec<DaveOut> {
            let mut out = Vec::new();
            self.prepare_transition(transition_id, version, &mut out);
            out
        }

        fn prepare_transition(&mut self, transition_id: u16, version: u16, out: &mut Vec<DaveOut>) {
            if let Some(s) = &mut self.session
                && let Err(e) = s.prepare_transition(transition_id, version)
            {
                warn!(error = %e, transition_id, version, "dave: prepare transition failed");
            }
            if transition_id != INIT_TRANSITION_ID {
                out.push(DaveOut::TransitionReady(transition_id));
            }
        }

        pub fn on_execute_transition(&mut self, transition_id: u16) {
            if let Some(s) = &mut self.session {
                match s.execute_transition(transition_id) {
                    Ok(true) => debug!(transition_id, "dave: transition executed"),
                    Ok(false) => debug!(transition_id, "dave: no prepared transition to execute"),
                    Err(e) => warn!(error = %e, transition_id, "dave: execute transition failed"),
                }
            }
        }

        pub fn on_external_sender(&mut self, package: &[u8]) {
            self.external_sender = Some(package.to_vec());
            if let Some(s) = &mut self.session
                && let Err(e) = s.set_external_sender(package)
            {
                warn!(error = %e, "dave: external sender rejected");
            }
        }

        pub fn on_proposals(&mut self, payload: &[u8]) -> Vec<DaveOut> {
            let mut out = Vec::new();
            let Some((&op, proposals)) = payload.split_first() else {
                warn!("dave: empty proposals frame");
                return out;
            };
            let op = match op {
                0 => ProposalsOperationType::APPEND,
                1 => ProposalsOperationType::REVOKE,
                other => {
                    warn!(op = other, "dave: unknown proposals operation");
                    return out;
                }
            };
            let recognized = self.recognized();
            let Some(s) = &mut self.session else { return out };
            match s.process_proposals(op, proposals, Some(&recognized)) {
                Ok(Some(cw)) => {
                    let mut buf = cw.commit;
                    if let Some(w) = cw.welcome {
                        buf.extend_from_slice(&w);
                    }
                    out.push(DaveOut::Binary(Opcode::DAVE_COMMIT_WELCOME, buf));
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "dave: proposals rejected"),
            }
            out
        }

        pub fn on_commit(&mut self, transition_id: u16, commit: &[u8]) -> Vec<DaveOut> {
            let mut out = Vec::new();
            let Some(s) = &mut self.session else { return out };
            match s.process_commit(commit) {
                Ok(true) => {
                    let v = self.protocol_version();
                    self.prepare_transition(transition_id, v, &mut out);
                }
                Ok(false) => debug!(transition_id, "dave: commit ignored"),
                Err(e) => {
                    warn!(error = %e, transition_id, "dave: commit rejected, starting over");
                    self.recover(transition_id, &mut out);
                }
            }
            out
        }

        pub fn on_welcome(&mut self, transition_id: u16, welcome: &[u8]) -> Vec<DaveOut> {
            let mut out = Vec::new();
            let recognized = self.recognized();
            let Some(s) = &mut self.session else { return out };
            match s.process_welcome(welcome, Some(&recognized)) {
                Ok(()) => {
                    let v = self.protocol_version();
                    self.prepare_transition(transition_id, v, &mut out);
                }
                Err(e) => {
                    warn!(error = %e, transition_id, "dave: welcome rejected, starting over");
                    self.recover(transition_id, &mut out);
                }
            }
            out
        }

        fn recover(&mut self, transition_id: u16, out: &mut Vec<DaveOut>) {
            out.push(DaveOut::InvalidCommitWelcome(transition_id));
            let v = self.protocol_version();
            self.protocol_init(v, out);
        }

        /// E2EE-encrypt one Opus frame into `buf`.
        pub fn encrypt_opus(&mut self, opus: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
            let Some(s) = &mut self.session else {
                return Err(Error::DaveNotReady);
            };
            s.encrypt_into(MediaType::AUDIO, Codec::OPUS, opus, buf)
                .map_err(|e| Error::DaveEncrypt(e.to_string()))
        }

        #[cfg(test)]
        pub fn session_mut(&mut self) -> Option<&mut DaveSession> {
            self.session.as_mut()
        }
    }
}

#[cfg(not(feature = "dave"))]
mod stub {
    use super::*;

    enum Never {}

    /// Uninhabited: without the `dave` feature no DAVE session can exist.
    pub(crate) struct Dave(Never);

    #[allow(clippy::unused_self)]
    impl Dave {
        pub fn new(_user_id: &str, _channel_id: &str) -> Result<Dave, Error> {
            Err(Error::NoDave)
        }
        pub fn max_protocol_version() -> u16 {
            0
        }
        pub fn ready(&self) -> bool {
            match self.0 {}
        }
        pub fn add_user(&mut self, _: &str) {
            match self.0 {}
        }
        pub fn remove_user(&mut self, _: &str) {
            match self.0 {}
        }
        pub fn on_select_protocol_ack(&mut self, _: u16) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn on_prepare_epoch(&mut self, _: u64, _: u16) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn on_prepare_transition(&mut self, _: u16, _: u16) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn on_execute_transition(&mut self, _: u16) {
            match self.0 {}
        }
        pub fn on_external_sender(&mut self, _: &[u8]) {
            match self.0 {}
        }
        pub fn on_proposals(&mut self, _: &[u8]) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn on_commit(&mut self, _: u16, _: &[u8]) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn on_welcome(&mut self, _: u16, _: &[u8]) -> Vec<DaveOut> {
            match self.0 {}
        }
        pub fn encrypt_opus(&mut self, _: &[u8], _: &mut Vec<u8>) -> Result<(), Error> {
            match self.0 {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_calls_are_not_held() {
        assert!(
            !should_hold(true, 0, false),
            "a call Discord runs without E2EE (protocol 0) must not hold audio"
        );
    }

    #[test]
    fn encrypted_calls_hold_until_ready() {
        assert!(
            should_hold(true, 1, false),
            "an E2EE call must hold audio until the epoch is ready"
        );
        assert!(!should_hold(true, 1, true), "a ready E2EE call must not hold audio");
    }

    #[test]
    fn no_dave_session_never_holds() {
        assert!(
            !should_hold(false, 1, false),
            "a connection without a DAVE session must not hold audio"
        );
    }

    #[cfg(not(feature = "dave"))]
    #[test]
    fn without_the_feature_dave_is_unavailable() {
        assert!(!dave_available());
        assert!(matches!(Dave::new("1", "2"), Err(Error::NoDave)));
    }

    #[cfg(feature = "dave")]
    mod mls {
        use std::num::NonZeroU16;

        use davey::{DaveSession, MediaType};
        use openmls::prelude::tls_codec::Serialize as _;
        use openmls::prelude::*;
        use openmls_basic_credential::SignatureKeyPair;
        use openmls_rust_crypto::OpenMlsRustCrypto;

        use super::*;

        const BOT: u64 = 1111;
        const HUMAN: u64 = 2222;
        const CHANNEL: u64 = 1234567890;

        /// Plays the voice gateway's MLS external sender (after davey's tests).
        struct ExternalSenderMock {
            provider: OpenMlsRustCrypto,
            signer: SignatureKeyPair,
            sender: ExternalSender,
        }

        impl ExternalSenderMock {
            fn new() -> Self {
                let cs = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
                let signer = SignatureKeyPair::new(cs.signature_algorithm()).unwrap();
                let sender = ExternalSender::new(signer.public().into(), BasicCredential::new(vec![0, 1, 1, 0]).into());
                Self {
                    provider: OpenMlsRustCrypto::default(),
                    signer,
                    sender,
                }
            }

            fn package(&self) -> Vec<u8> {
                self.sender.tls_serialize_detached().unwrap()
            }

            /// Payload of an append proposals (27) frame adding `key_package`'s owner.
            fn propose_add(&self, key_package: &[u8]) -> Vec<u8> {
                let kp = KeyPackageIn::tls_deserialize_exact_bytes(key_package)
                    .unwrap()
                    .validate(self.provider.crypto(), ProtocolVersion::Mls10)
                    .unwrap();
                let msg = ExternalProposal::new_add::<OpenMlsRustCrypto>(
                    kp,
                    GroupId::from_slice(&CHANNEL.to_be_bytes()),
                    GroupEpoch::from(0),
                    &self.signer,
                    SenderExtensionIndex::new(0),
                )
                .unwrap();
                let mut out = vec![0u8];
                out.extend(
                    VLBytes::new(msg.tls_serialize_detached().unwrap())
                        .tls_serialize_detached()
                        .unwrap(),
                );
                out
            }
        }

        fn key_package(out: &[DaveOut]) -> Vec<u8> {
            out.iter()
                .find_map(|o| match o {
                    DaveOut::Binary(op, kp) if *op == Opcode::DAVE_KEY_PACKAGE => Some(kp.clone()),
                    _ => None,
                })
                .expect("a key package is sent")
        }

        #[test]
        fn protocol_zero_is_passthrough() {
            let mut d = Dave::new("1111", "1234567890").unwrap();
            assert!(d.on_select_protocol_ack(0).is_empty());
            assert!(!d.ready());
        }

        #[test]
        fn bad_snowflakes_are_rejected() {
            assert!(Dave::new("me", "1").is_err());
            assert!(Dave::new("1", "").is_err());
        }

        #[test]
        fn full_group_join_then_encrypt_to_a_peer() {
            let ext = ExternalSenderMock::new();
            let mut bot = Dave::new(&BOT.to_string(), &CHANNEL.to_string()).unwrap();
            bot.add_user(&HUMAN.to_string());
            // External sender can arrive before the protocol version is known.
            bot.on_external_sender(&ext.package());
            let out = bot.on_select_protocol_ack(1);
            key_package(&out);
            assert!(!bot.ready());

            let mut human = DaveSession::new(NonZeroU16::new(1).unwrap(), HUMAN, CHANNEL, None).unwrap();
            human.set_external_sender(&ext.package()).unwrap();
            let human_kp = human.create_key_package().unwrap();

            let out = bot.on_proposals(&ext.propose_add(&human_kp));
            let [DaveOut::Binary(op, cw)] = out.as_slice() else {
                panic!("expected one commit-welcome, got {out:?}")
            };
            assert_eq!(*op, Opcode::DAVE_COMMIT_WELCOME);
            let (_, welcome) = MlsMessageIn::tls_deserialize_bytes(cw).unwrap();
            let commit = &cw[..cw.len() - welcome.len()];

            assert_eq!(bot.on_commit(7, commit), vec![DaveOut::TransitionReady(7)]);
            assert!(!bot.ready(), "keys switch only when the transition executes");
            bot.on_execute_transition(7);
            assert!(bot.ready());

            human.process_welcome(welcome, Some(&[BOT, HUMAN])).unwrap();
            human.prepare_transition(7, 1).unwrap();
            human.execute_transition(7).unwrap();

            let opus = [0x78u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            let mut enc = Vec::new();
            bot.encrypt_opus(&opus, &mut enc).unwrap();
            assert_ne!(enc, opus);
            assert_eq!(human.decrypt(BOT, MediaType::AUDIO, &enc).unwrap(), opus);

            // Opus silence passes through untouched.
            bot.encrypt_opus(&wavyzon_audio::codec::OPUS_SILENCE, &mut enc).unwrap();
            assert_eq!(enc, wavyzon_audio::codec::OPUS_SILENCE);

            // Downgrade to protocol 0 removes the key.
            assert_eq!(bot.on_prepare_transition(9, 0), vec![DaveOut::TransitionReady(9)]);
            bot.on_execute_transition(9);
            assert!(!bot.ready());
            assert!(bot.session_mut().is_some());
        }

        #[test]
        fn a_bad_commit_is_reported_and_restarts_the_group() {
            let ext = ExternalSenderMock::new();
            let mut bot = Dave::new(&BOT.to_string(), &CHANNEL.to_string()).unwrap();
            bot.on_select_protocol_ack(1);
            bot.on_external_sender(&ext.package());
            let mut human = DaveSession::new(NonZeroU16::new(1).unwrap(), HUMAN, CHANNEL, None).unwrap();
            human.set_external_sender(&ext.package()).unwrap();
            bot.add_user(&HUMAN.to_string());
            bot.on_proposals(&ext.propose_add(&human.create_key_package().unwrap()));
            let out = bot.on_commit(3, &[0xde, 0xad]);
            assert_eq!(out[0], DaveOut::InvalidCommitWelcome(3));
            key_package(&out[1..]);
            let out = bot.on_welcome(4, &[0xbe, 0xef]);
            assert_eq!(out[0], DaveOut::InvalidCommitWelcome(4));
            key_package(&out[1..]);
        }

        #[test]
        fn proposals_from_unknown_users_are_rejected() {
            let ext = ExternalSenderMock::new();
            let mut bot = Dave::new(&BOT.to_string(), &CHANNEL.to_string()).unwrap();
            bot.on_external_sender(&ext.package());
            bot.on_select_protocol_ack(1);
            let mut stranger = DaveSession::new(NonZeroU16::new(1).unwrap(), 9999, CHANNEL, None).unwrap();
            stranger.set_external_sender(&ext.package()).unwrap();
            assert!(
                bot.on_proposals(&ext.propose_add(&stranger.create_key_package().unwrap()))
                    .is_empty()
            );
        }

        #[test]
        fn epoch_one_reinitialises() {
            let mut bot = Dave::new("1111", "1234567890").unwrap();
            assert!(bot.on_prepare_epoch(2, 1).is_empty());
            key_package(&bot.on_prepare_epoch(1, 1));
            assert_eq!(bot.on_prepare_transition(0, 1), vec![]);
        }
    }
}
