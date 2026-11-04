//! Drives the mock voice gateway with a tiny scripted Discord voice client.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpStream, UdpSocket};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use wavyzon_testkit::gateway::wire::{self, PacketSealer, TransportMode};
use wavyzon_testkit::gateway::{GatewayConfig, GatewayMessage, MockVoiceGateway, close_code, op};

const T: Duration = Duration::from_secs(3);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What a scripted client observes.
enum Recv {
    Json(Value),
    Binary(Vec<u8>),
    Closed(Option<u16>),
}

async fn recv(ws: &mut Ws) -> Recv {
    loop {
        match tokio::time::timeout(T, ws.next())
            .await
            .expect("gateway answered in time")
        {
            Some(Ok(Message::Text(t))) => return Recv::Json(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Binary(b))) => return Recv::Binary(b.to_vec()),
            Some(Ok(Message::Close(f))) => return Recv::Closed(f.map(|f| u16::from(f.code))),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return Recv::Closed(None),
        }
    }
}

async fn recv_json(ws: &mut Ws) -> Value {
    match recv(ws).await {
        Recv::Json(v) => v,
        Recv::Binary(b) => panic!("expected JSON, got binary {b:?}"),
        Recv::Closed(c) => panic!("expected JSON, got close {c:?}"),
    }
}

async fn recv_close(ws: &mut Ws) -> Option<u16> {
    loop {
        if let Recv::Closed(c) = recv(ws).await {
            return c;
        }
    }
}

async fn send(ws: &mut Ws, op: u8, d: Value) {
    ws.send(Message::Text(json!({ "op": op, "d": d }).to_string().into()))
        .await
        .unwrap();
}

fn identify(token: &str) -> Value {
    json!({ "server_id": "42", "user_id": "7", "session_id": "sess-1", "token": token, "max_dave_protocol_version": 0 })
}

struct Session {
    ws: Ws,
    udp: UdpSocket,
    ssrc: u32,
    sealer: PacketSealer,
}

/// Full happy-path handshake; returns the live session.
async fn handshake(gw: &MockVoiceGateway, mode: TransportMode) -> Session {
    let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["op"], op::HELLO);
    assert!(hello["d"]["heartbeat_interval"].as_f64().unwrap() > 0.0);
    assert!(hello.get("seq").is_none());

    send(&mut ws, op::IDENTIFY, identify("tok")).await;
    let ready = recv_json(&mut ws).await;
    assert_eq!(ready["op"], op::READY);
    assert_eq!(ready["seq"], 1);
    let ssrc = ready["d"]["ssrc"].as_u64().unwrap() as u32;
    let modes: Vec<&str> = ready["d"]["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(modes.contains(&mode.as_str()));
    let addr = format!("{}:{}", ready["d"]["ip"].as_str().unwrap(), ready["d"]["port"]);

    // UDP IP discovery, resending like a real client.
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(&addr).await.unwrap();
    let mut buf = [0u8; 128];
    let (ip, port) = loop {
        udp.send(&wire::discovery_request(ssrc)).await.unwrap();
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(100), udp.recv(&mut buf)).await {
            let (kind, got_ssrc, ip, port) = wire::parse_discovery(&buf[..n]).unwrap();
            assert_eq!((kind, got_ssrc), (2, ssrc));
            break (ip, port);
        }
    };
    assert_eq!(ip, "127.0.0.1");
    assert_eq!(port, udp.local_addr().unwrap().port());

    send(
        &mut ws,
        op::SELECT_PROTOCOL,
        json!({ "protocol": "udp", "data": { "address": ip, "port": port, "mode": mode.as_str() } }),
    )
    .await;
    let sd = recv_json(&mut ws).await;
    assert_eq!(sd["op"], op::SESSION_DESCRIPTION);
    assert_eq!(sd["d"]["mode"], mode.as_str());
    assert_eq!(sd["d"]["dave_protocol_version"], 0);
    let key: Vec<u8> = sd["d"]["secret_key"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_u64().unwrap() as u8)
        .collect();
    let key: [u8; 32] = key.try_into().unwrap();
    assert_eq!(key, gw.secret_key());
    Session {
        ws,
        udp,
        ssrc,
        sealer: PacketSealer::new(mode, &key),
    }
}

#[tokio::test]
async fn handshake_and_rtp_both_modes() {
    for mode in TransportMode::ALL {
        let gw = MockVoiceGateway::start().await.unwrap();
        let mut s = handshake(&gw, mode).await;
        assert_eq!(gw.connect_uris(), vec!["/?v=8".to_string()]);
        assert_eq!(gw.identify_payload().unwrap()["token"], "tok");
        assert_eq!(gw.negotiated_mode(), Some(mode));
        assert_eq!(gw.select_protocol_payload().unwrap()["data"]["mode"], mode.as_str());
        assert_eq!(gw.sessions_established(), 1);

        send(
            &mut s.ws,
            op::SPEAKING,
            json!({ "speaking": 1, "delay": 0, "ssrc": s.ssrc }),
        )
        .await;
        for i in 0..10u16 {
            let h = wire::rtp_header(i, i as u32 * 960, s.ssrc);
            s.udp.send(&s.sealer.seal(&h, &[0xFC, i as u8, 0xAA])).await.unwrap();
        }
        // An undecryptable packet is counted, not recorded.
        let mut bad = PacketSealer::new(mode, &[0; 32]);
        s.udp
            .send(&bad.seal(&wire::rtp_header(99, 0, s.ssrc), b"x"))
            .await
            .unwrap();
        s.udp
            .send(&s.sealer.seal(&wire::rtp_header(10, 9600, s.ssrc), &[0xF8, 0xFF, 0xFE]))
            .await
            .unwrap();

        let packets = gw.wait_for_packets(11, T).await.unwrap();
        assert_eq!(packets.len(), 11);
        for (i, p) in packets.iter().take(10).enumerate() {
            assert_eq!(p.sequence, i as u16);
            assert_eq!(p.timestamp, i as u32 * 960);
            assert_eq!(p.ssrc, s.ssrc);
            assert_eq!(p.payload_type, 0x78);
            assert_eq!(p.nonce, i as u32);
            assert_eq!(p.opus, vec![0xFC, i as u8, 0xAA]);
        }
        assert!(packets[10].is_opus_silence());
        assert_eq!(gw.undecryptable_packets(), 1);
        let speaking = gw.wait_for_speaking(1, T).await.unwrap();
        assert_eq!((speaking[0].speaking, speaking[0].ssrc), (1, s.ssrc));
    }
}

#[tokio::test]
async fn heartbeat_ack_echoes_nonce_and_records_seq_ack() {
    let gw = MockVoiceGateway::start().await.unwrap();
    let mut s = handshake(&gw, TransportMode::Aes256Gcm).await;
    send(&mut s.ws, op::HEARTBEAT, json!({ "t": 1234567, "seq_ack": 2 })).await;
    let ack = recv_json(&mut s.ws).await;
    assert_eq!(ack, json!({ "op": op::HEARTBEAT_ACK, "d": { "t": 1234567 } }));
    let hb = gw.heartbeats();
    assert_eq!((hb[0].nonce.clone(), hb[0].seq_ack), (json!(1234567), Some(2)));

    gw.update_faults(|f| f.withhold_heartbeat_acks = true);
    send(&mut s.ws, op::HEARTBEAT, json!({ "t": 1, "seq_ack": 2 })).await;
    assert!(gw.send_op(op::CLIENTS_CONNECT, json!({ "user_ids": ["9"] })).await);
    // The next message is the scripted one, not an ACK.
    let next = recv_json(&mut s.ws).await;
    assert_eq!(next["op"], op::CLIENTS_CONNECT);
    assert_eq!(next["seq"], 3);
}

#[tokio::test]
async fn forced_close_then_resume() {
    let gw = MockVoiceGateway::start().await.unwrap();
    let mut s = handshake(&gw, TransportMode::XChaCha20Poly1305).await;
    assert!(gw.close(close_code::VOICE_SERVER_CRASHED, "crashed").await);
    assert_eq!(recv_close(&mut s.ws).await, Some(4015));
    gw.wait_until(T, "disconnect", |g| !g.is_connected()).await.unwrap();
    assert!(gw.closes()[0].by_server);

    let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
    assert_eq!(recv_json(&mut ws).await["op"], op::HELLO);
    send(
        &mut ws,
        op::RESUME,
        json!({ "server_id": "42", "session_id": "sess-1", "token": "tok", "seq_ack": 2 }),
    )
    .await;
    let resumed = recv_json(&mut ws).await;
    assert_eq!(resumed["op"], op::RESUMED);
    assert_eq!(gw.resumes(), 1);
    assert_eq!(gw.connection_count(), 2);
    assert_eq!(gw.resume_payload().unwrap()["seq_ack"], 2);

    // The old UDP session keeps working after a resume.
    s.udp
        .send(&s.sealer.seal(&wire::rtp_header(0, 0, s.ssrc), b"opus"))
        .await
        .unwrap();
    gw.wait_for_packets(1, T).await.unwrap();

    // A resume for an unknown session is refused with 4006.
    let (mut ws2, _) = connect_async(gw.ws_url()).await.unwrap();
    recv_json(&mut ws2).await;
    send(
        &mut ws2,
        op::RESUME,
        json!({ "server_id": "42", "session_id": "other", "token": "tok", "seq_ack": 0 }),
    )
    .await;
    assert_eq!(recv_close(&mut ws2).await, Some(close_code::SESSION_INVALID));

    // And so is any resume when the fault is set.
    gw.update_faults(|f| f.reject_resume = true);
    let (mut ws3, _) = connect_async(gw.ws_url()).await.unwrap();
    recv_json(&mut ws3).await;
    send(
        &mut ws3,
        op::RESUME,
        json!({ "server_id": "42", "session_id": "sess-1", "token": "tok", "seq_ack": 0 }),
    )
    .await;
    assert_eq!(recv_close(&mut ws3).await, Some(4006));
}

#[tokio::test]
async fn protocol_errors_close_with_discord_codes() {
    let gw = MockVoiceGateway::start_with(GatewayConfig {
        expected_token: Some("good".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let cases: Vec<(Vec<(u8, Value)>, u16)> = vec![
        (vec![(op::IDENTIFY, identify("bad"))], close_code::AUTHENTICATION_FAILED),
        (
            vec![(op::SELECT_PROTOCOL, json!({ "protocol": "udp", "data": {} }))],
            close_code::NOT_AUTHENTICATED,
        ),
        (
            vec![(op::IDENTIFY, identify("good")), (op::IDENTIFY, identify("good"))],
            close_code::ALREADY_AUTHENTICATED,
        ),
        (
            vec![
                (op::IDENTIFY, identify("good")),
                (
                    op::SELECT_PROTOCOL,
                    json!({ "protocol": "udp", "data": { "mode": "xsalsa20_poly1305" } }),
                ),
            ],
            close_code::UNKNOWN_ENCRYPTION_MODE,
        ),
        (
            vec![
                (op::IDENTIFY, identify("good")),
                (op::SELECT_PROTOCOL, json!({ "protocol": "webrtc", "data": {} })),
            ],
            close_code::UNKNOWN_PROTOCOL,
        ),
        (
            vec![(op::IDENTIFY, json!({ "token": "good" }))],
            close_code::DECODE_ERROR,
        ),
    ];
    for (script, code) in cases {
        let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
        recv_json(&mut ws).await;
        for (o, d) in script {
            send(&mut ws, o, d).await;
        }
        assert_eq!(recv_close(&mut ws).await, Some(code), "expected close {code}");
    }
    let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
    recv_json(&mut ws).await;
    ws.send(Message::Text("not json".into())).await.unwrap();
    assert_eq!(recv_close(&mut ws).await, Some(close_code::DECODE_ERROR));
}

#[tokio::test]
async fn faults_discovery_drops_and_stalls() {
    let gw = MockVoiceGateway::start().await.unwrap();
    gw.update_faults(|f| {
        f.ignore_discovery = 2;
        f.drop_every_nth_rtp = 2;
    });
    let mut s = handshake(&gw, TransportMode::Aes256Gcm).await;
    assert!(gw.discovery_requests() >= 3, "client had to resend discovery");
    for i in 0..10u16 {
        s.udp
            .send(&s.sealer.seal(&wire::rtp_header(i, 0, s.ssrc), b"o"))
            .await
            .unwrap();
    }
    gw.wait_until(T, "10 rtp", |g| g.packet_count() + g.dropped_packets() as usize >= 10)
        .await
        .unwrap();
    assert_eq!(gw.dropped_packets(), 5);
    let seqs: Vec<u16> = gw.received_packets().iter().map(|p| p.sequence).collect();
    assert_eq!(seqs, vec![0, 2, 4, 6, 8]);

    // Stall after Identify: no Ready ever comes.
    gw.update_faults(|f| f.stall_after_identify = true);
    let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
    recv_json(&mut ws).await;
    send(&mut ws, op::IDENTIFY, identify("tok")).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), ws.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reply_delay_postpones_handshake() {
    let gw = MockVoiceGateway::start().await.unwrap();
    gw.update_faults(|f| f.reply_delay = Duration::from_millis(150));
    let (mut ws, _) = connect_async(gw.ws_url()).await.unwrap();
    recv_json(&mut ws).await;
    let t0 = std::time::Instant::now();
    send(&mut ws, op::IDENTIFY, identify("tok")).await;
    assert_eq!(recv_json(&mut ws).await["op"], op::READY);
    assert!(t0.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn hooks_and_binary_frames_for_dave_scripting() {
    let gw = MockVoiceGateway::start().await.unwrap();
    // Reply to a (binary) key package with a binary announce-commit.
    gw.set_hook(|m| {
        if m.op == op::DAVE_KEY_PACKAGE {
            vec![GatewayMessage::Binary {
                op: op::DAVE_ANNOUNCE_COMMIT,
                payload: vec![0x00, 0x01, 0xAB],
            }]
        } else {
            vec![]
        }
    });
    let mut s = handshake(&gw, TransportMode::Aes256Gcm).await;
    s.ws.send(Message::Binary(
        wire::encode_client_binary(op::DAVE_KEY_PACKAGE, &[1, 2, 3]).into(),
    ))
    .await
    .unwrap();
    match recv(&mut s.ws).await {
        Recv::Binary(b) => {
            // [seq u16][op][transition id u16][payload]; seq 3 follows Ready (1) and SessionDescription (2).
            assert_eq!(b, vec![0x00, 0x03, op::DAVE_ANNOUNCE_COMMIT, 0x00, 0x01, 0xAB]);
        }
        _ => panic!("expected a binary frame"),
    }
    let kp = gw.wait_for_op(op::DAVE_KEY_PACKAGE, T).await.unwrap();
    assert_eq!(kp.binary, Some(vec![1, 2, 3]));

    assert!(
        gw.send_op(op::DAVE_PREPARE_EPOCH, json!({ "epoch": 1, "protocol_version": 1 }))
            .await
    );
    assert_eq!(recv_json(&mut s.ws).await["op"], op::DAVE_PREPARE_EPOCH);
    assert!(gw.send(GatewayMessage::Text("{\"op\":99,\"d\":null}".into())).await);
    assert_eq!(recv_json(&mut s.ws).await["op"], 99);

    assert!(gw.abort_connection().await);
    assert_eq!(recv_close(&mut s.ws).await, None);
}

#[tokio::test]
async fn client_close_is_recorded() {
    let gw = MockVoiceGateway::start().await.unwrap();
    let mut s = handshake(&gw, TransportMode::Aes256Gcm).await;
    s.ws.close(None).await.unwrap();
    gw.wait_until(T, "client close", |g| !g.closes().is_empty())
        .await
        .unwrap();
    let c = &gw.closes()[0];
    assert!(!c.by_server);
    assert!(!gw.close(1000, "").await || !gw.is_connected());
    assert!(gw.endpoint().starts_with("ws://127.0.0.1:"));
    assert_eq!(gw.host_port(), gw.ws_addr().to_string());
}
