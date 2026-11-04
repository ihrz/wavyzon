//! End-to-end tests against an in-process mock voice gateway: a
//! tokio-tungstenite server plus a UDP socket on localhost.
#![cfg_attr(not(feature = "dave"), allow(dead_code, unused_imports))]

use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use wavyzon_audio::codec::OPUS_SILENCE;
use wavyzon_audio::{FRAME_SAMPLES_PER_CHANNEL, Frame, FrameInfo, FrameSource};
use wavyzon_voice::transport::{RTP_HEADER_SIZE, Sealer};
use wavyzon_voice::{CloseEvent, Conn, EncryptionMode, Server, dial};

const SSRC: u32 = 77;
const KEY: [u8; 32] = [9; 32];
const WAIT: Duration = Duration::from_secs(5);

type Ws = WebSocketStream<TcpStream>;

/// Plays `frames` frames of a tone, then nothing.
struct Tone {
    left: AtomicU32,
    pulls: AtomicU32,
    sent: AtomicU32,
}

impl Tone {
    fn new(frames: u32) -> Arc<Tone> {
        Arc::new(Tone {
            left: AtomicU32::new(frames),
            pulls: AtomicU32::new(0),
            sent: AtomicU32::new(0),
        })
    }
}

impl FrameSource for Tone {
    fn next_frame(&self, out: &mut Frame) -> Option<FrameInfo> {
        let n = self.pulls.fetch_add(1, Ordering::SeqCst);
        if self.left.load(Ordering::SeqCst) == 0 {
            return None;
        }
        self.left.fetch_sub(1, Ordering::SeqCst);
        for (i, s) in out.iter_mut().enumerate() {
            *s = ((i as f64 / 2.0 / 48.0 * 440.0 / 1000.0 * TAU).sin() * 8000.0) as i16;
        }
        Some(FrameInfo {
            instance: 1,
            position_ms: f64::from(n) * 20.0,
            mixing: false,
        })
    }
    fn sent(&self, _: FrameInfo, _: Instant) {
        self.sent.fetch_add(1, Ordering::SeqCst);
    }
    fn stall(&self, _: Duration) {}
}

struct Mock {
    listener: TcpListener,
    udp: UdpSocket,
}

impl Mock {
    async fn new() -> Mock {
        Mock {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            udp: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    fn server(&self) -> Server {
        Server {
            guild_id: "10".into(),
            user_id: "1111".into(),
            channel_id: "1234567890".into(),
            session_id: "sess".into(),
            token: "tok".into(),
            endpoint: format!("ws://{}", self.listener.local_addr().unwrap()),
            bitrate: 64_000,
        }
    }

    #[allow(clippy::result_large_err)]
    async fn accept(&self) -> Ws {
        let (tcp, _) = timeout(WAIT, self.listener.accept())
            .await
            .expect("client dials")
            .unwrap();
        let mut path = String::new();
        let ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
                path = req.uri().to_string();
                Ok(res)
            },
        )
        .await
        .unwrap();
        assert_eq!(path, "/?v=8");
        ws
    }

    /// Hello → identify → ready → IP discovery → select protocol → session description.
    async fn handshake(&self, dave_version: u16) -> Ws {
        let mut ws = self.accept().await;
        send(&mut ws, json!({"op": 8, "d": {"heartbeat_interval": 1000.0}})).await;
        let id = expect_op(&mut ws, 0).await;
        assert_eq!(id["d"]["server_id"], "10");
        assert_eq!(id["d"]["user_id"], "1111");
        assert_eq!(id["d"]["session_id"], "sess");
        assert_eq!(id["d"]["token"], "tok");
        assert_eq!(id["d"]["max_dave_protocol_version"], 1);
        let port = self.udp.local_addr().unwrap().port();
        let modes = [
            "xsalsa20_poly1305",
            "aead_xchacha20_poly1305_rtpsize",
            "aead_aes256_gcm_rtpsize",
        ];
        send(
            &mut ws,
            json!({"op": 2, "d": {"ssrc": SSRC, "ip": "127.0.0.1", "port": port, "modes": modes}}),
        )
        .await;

        let mut buf = [0u8; 128];
        let (n, from) = timeout(WAIT, self.udp.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(n, 74);
        assert_eq!(&buf[4..8], &SSRC.to_be_bytes());
        let mut reply = [0u8; 74];
        reply[..2].copy_from_slice(&2u16.to_be_bytes());
        reply[2..4].copy_from_slice(&70u16.to_be_bytes());
        reply[4..8].copy_from_slice(&SSRC.to_be_bytes());
        reply[8..19].copy_from_slice(b"203.0.113.5");
        reply[72..].copy_from_slice(&5555u16.to_be_bytes());
        self.udp.send_to(&reply, from).await.unwrap();

        let sp = expect_op(&mut ws, 1).await;
        assert_eq!(sp["d"]["protocol"], "udp");
        assert_eq!(sp["d"]["data"]["address"], "203.0.113.5");
        assert_eq!(sp["d"]["data"]["port"], 5555);
        assert_eq!(sp["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
        send(
            &mut ws,
            json!({"op": 4, "d": {"mode": "aead_aes256_gcm_rtpsize", "secret_key": KEY, "dave_protocol_version": dave_version}}),
        )
        .await;
        ws
    }

    /// Receive and open one RTP packet: (sequence, payload).
    async fn rtp(&self, within: Duration) -> Option<(u16, Vec<u8>)> {
        let mut buf = [0u8; 2048];
        let (n, _) = timeout(within, self.udp.recv_from(&mut buf)).await.ok()?.unwrap();
        let packet = &buf[..n];
        assert_eq!((packet[0], packet[1]), (0x80, 0x78));
        assert_eq!(&packet[8..12], &SSRC.to_be_bytes());
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let opener = Sealer::new(EncryptionMode::Aes256Gcm, &KEY).unwrap();
        let payload = opener.open(packet).expect("packet opens with the session key");
        assert!(n > RTP_HEADER_SIZE);
        Some((seq, payload))
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn close(ws: &mut Ws, code: u16, reason: &str) {
    let _ = ws
        .close(Some(CloseFrame {
            code: code.into(),
            reason: reason.to_string().into(),
        }))
        .await;
}

enum In {
    Json(Value),
    Binary(Vec<u8>),
    Closed(Option<u16>),
}

async fn recv(ws: &mut Ws) -> In {
    loop {
        match timeout(WAIT, ws.next()).await.expect("client message") {
            None | Some(Err(_)) => return In::Closed(None),
            Some(Ok(Message::Text(t))) => return In::Json(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Binary(b))) => return In::Binary(b.to_vec()),
            Some(Ok(Message::Close(f))) => return In::Closed(f.map(|f| u16::from(f.code))),
            Some(Ok(_)) => {}
        }
    }
}

/// Wait for JSON op `op`, acking heartbeats (unless waiting for one) and
/// skipping anything else.
async fn expect_op(ws: &mut Ws, op: u64) -> Value {
    loop {
        match recv(ws).await {
            In::Json(v) if v["op"] == op => return v,
            In::Json(v) if v["op"] == 3 => send(ws, json!({"op": 6, "d": {"t": v["d"]["t"]}})).await,
            In::Json(_) | In::Binary(_) => {}
            In::Closed(code) => panic!("closed with {code:?} while waiting for op {op}"),
        }
    }
}

async fn expect_binary(ws: &mut Ws, op: u8) -> Vec<u8> {
    loop {
        match recv(ws).await {
            In::Binary(b) if b[0] == op => return b[1..].to_vec(),
            In::Json(v) if v["op"] == 3 => send(ws, json!({"op": 6, "d": {"t": v["d"]["t"]}})).await,
            In::Json(_) | In::Binary(_) => {}
            In::Closed(code) => panic!("closed with {code:?} while waiting for binary op {op}"),
        }
    }
}

fn recorder() -> (
    Arc<Mutex<Vec<CloseEvent>>>,
    impl Fn(&Conn, CloseEvent) + Send + Sync + 'static,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    (events, move |_: &Conn, ev: CloseEvent| e.lock().push(ev))
}

#[cfg(feature = "dave")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_audio_heartbeat_resume_and_terminal_close() {
    let mock = Mock::new().await;
    let tone = Tone::new(5);
    let (events, on_close) = recorder();
    let conn = dial(mock.server(), tone.clone(), on_close).unwrap();
    let mut ws = mock.handshake(0).await;

    // Speaking on, 5 audio frames, a 5-frame silence trailer, speaking off.
    let on = expect_op(&mut ws, 5).await;
    assert_eq!(
        (on["d"]["speaking"].as_u64(), on["d"]["ssrc"].as_u64()),
        (Some(1), Some(u64::from(SSRC)))
    );
    let mut seqs = Vec::new();
    let mut silences = 0;
    for _ in 0..10 {
        let (seq, payload) = mock.rtp(WAIT).await.expect("an RTP packet");
        seqs.push(seq);
        if payload == OPUS_SILENCE {
            silences += 1;
        }
    }
    assert_eq!(seqs, (0..10).collect::<Vec<u16>>());
    assert_eq!(silences, 5);
    let off = expect_op(&mut ws, 5).await;
    assert_eq!(off["d"]["speaking"], 0);
    assert_eq!(tone.sent.load(Ordering::SeqCst), 5);
    assert!(
        mock.rtp(Duration::from_millis(200)).await.is_none(),
        "nothing is sent after speaking goes off"
    );

    // Heartbeats carry the last sequence we sent.
    send(&mut ws, json!({"op": 11, "d": {"user_ids": ["2222"]}, "seq": 42})).await;
    loop {
        let hb = expect_op(&mut ws, 3).await;
        send(&mut ws, json!({"op": 6, "d": {"t": hb["d"]["t"]}})).await;
        if hb["d"]["seq_ack"] == 42 {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(conn.ping() > Duration::ZERO);

    // A non-terminal close is followed by a resume.
    close(&mut ws, 4015, "voice server crashed").await;
    let mut ws = mock.accept().await;
    send(&mut ws, json!({"op": 8, "d": {"heartbeat_interval": 1000.0}})).await;
    let resume = expect_op(&mut ws, 7).await;
    assert_eq!(
        resume["d"],
        json!({"server_id": "10", "session_id": "sess", "token": "tok", "seq_ack": 42})
    );
    send(&mut ws, json!({"op": 9, "d": null})).await;
    assert_eq!(
        events.lock().clone(),
        vec![CloseEvent {
            code: 4015,
            reason: "voice server crashed".into(),
            by_remote: true,
            terminal: false
        }]
    );
    assert!(!conn.dead());

    // A terminal close kills the connection.
    close(&mut ws, 4014, "disconnected").await;
    let deadline = Instant::now() + WAIT;
    while !conn.dead() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(conn.dead());
    assert_eq!(
        events.lock().last().unwrap(),
        &CloseEvent {
            code: 4014,
            reason: "disconnected".into(),
            by_remote: true,
            terminal: true
        }
    );
    conn.close().await;
}

#[cfg(feature = "dave")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_sends_a_normal_closure_and_stops_the_sender() {
    let mock = Mock::new().await;
    let tone = Tone::new(u32::MAX);
    let (events, on_close) = recorder();
    let conn = dial(mock.server(), tone.clone(), on_close).unwrap();
    let mut ws = mock.handshake(0).await;
    mock.rtp(WAIT).await.expect("audio flows");
    let server = tokio::spawn(async move {
        loop {
            if let In::Closed(code) = recv(&mut ws).await {
                return code;
            }
        }
    });
    conn.close().await;
    let pulls = tone.pulls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        tone.pulls.load(Ordering::SeqCst),
        pulls,
        "the sender thread exited before close returned"
    );
    assert_eq!(server.await.unwrap(), Some(1000));
    assert!(events.lock().is_empty(), "a local close is not reported");
}

#[cfg(feature = "dave")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtp_timestamps_advance_by_one_frame() {
    let mock = Mock::new().await;
    let conn = dial(mock.server(), Tone::new(3), |_, _| {}).unwrap();
    let _ws = mock.handshake(0).await;
    let mut buf = [0u8; 2048];
    let mut stamps = Vec::new();
    let mut at = Vec::new();
    for _ in 0..3 {
        let (n, _) = timeout(WAIT, mock.udp.recv_from(&mut buf)).await.unwrap().unwrap();
        assert!(n > 12);
        stamps.push(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]));
        at.push(Instant::now());
    }
    assert_eq!(
        stamps,
        vec![
            0,
            FRAME_SAMPLES_PER_CHANNEL as u32,
            2 * FRAME_SAMPLES_PER_CHANNEL as u32
        ]
    );
    let gap = at[2] - at[0];
    assert!(
        gap >= Duration::from_millis(30) && gap <= Duration::from_millis(80),
        "paced at 20 ms, got {gap:?} for 2 frames"
    );
    conn.close().await;
}

#[cfg(feature = "dave")]
mod dave {
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

    /// The gateway's MLS external sender.
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

    fn frame(seq: u16, op: u8, transition: Option<u16>, payload: &[u8]) -> Message {
        let mut b = seq.to_be_bytes().to_vec();
        b.push(op);
        if let Some(t) = transition {
            b.extend(t.to_be_bytes());
        }
        b.extend_from_slice(payload);
        Message::Binary(b.into())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2ee_call_holds_audio_until_the_mls_transition_executes() {
        let mock = Mock::new().await;
        let tone = Tone::new(u32::MAX);
        let conn = dial(mock.server(), tone.clone(), |_, _| {}).unwrap();
        let mut ws = mock.handshake(1).await;
        let _bot_kp = expect_binary(&mut ws, 26).await;

        // Held: nothing pulled from the player, nothing on the wire.
        assert!(
            mock.rtp(Duration::from_millis(300)).await.is_none(),
            "E2EE audio must not go out before the epoch"
        );
        assert_eq!(tone.pulls.load(Ordering::SeqCst), 0);
        assert_eq!(conn.dave_protocol_version(), 1);

        let ext = ExternalSenderMock::new();
        send(&mut ws, json!({"op": 11, "d": {"user_ids": [HUMAN.to_string()]}})).await;
        ws.send(frame(1, 25, None, &ext.package())).await.unwrap();
        let mut human = DaveSession::new(NonZeroU16::new(1).unwrap(), HUMAN, CHANNEL, None).unwrap();
        human.set_external_sender(&ext.package()).unwrap();
        ws.send(frame(
            2,
            27,
            None,
            &ext.propose_add(&human.create_key_package().unwrap()),
        ))
        .await
        .unwrap();

        let cw = expect_binary(&mut ws, 28).await;
        let (_, welcome) = MlsMessageIn::tls_deserialize_bytes(&cw[..]).unwrap();
        let commit = &cw[..cw.len() - welcome.len()];
        ws.send(frame(3, 29, Some(5), commit)).await.unwrap();
        let ready = expect_op(&mut ws, 23).await;
        assert_eq!(ready["d"]["transition_id"], 5);
        assert!(
            mock.rtp(Duration::from_millis(100)).await.is_none(),
            "still held until execute"
        );

        human.process_welcome(welcome, Some(&[BOT, HUMAN])).unwrap();
        human.prepare_transition(5, 1).unwrap();
        human.execute_transition(5).unwrap();
        send(&mut ws, json!({"op": 22, "d": {"transition_id": 5}})).await;

        let (_, payload) = mock.rtp(WAIT).await.expect("audio flows once the transition executes");
        let opus = human
            .decrypt(BOT, MediaType::AUDIO, &payload)
            .expect("the peer decrypts our frame");
        assert!(!opus.is_empty() && opus != payload);
        conn.close().await;
    }
}

#[cfg(not(feature = "dave"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_dave_the_session_setup_fails() {
    let mock = Mock::new().await;
    let conn = dial(mock.server(), Tone::new(0), |_, _| {}).unwrap();
    let mut ws = mock.accept().await;
    send(&mut ws, json!({"op": 8, "d": {"heartbeat_interval": 1000.0}})).await;
    loop {
        if let In::Closed(code) = recv(&mut ws).await {
            assert_eq!(code, Some(1011));
            break;
        }
    }
    assert!(!wavyzon_voice::dave_available());
    conn.close().await;
}
