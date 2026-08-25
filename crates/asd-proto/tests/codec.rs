//! Protocol codec contract: roundtrip for every frame kind plus
//! truncated/oversized frame error paths.

use asd_proto::{
    AgentState, ClientKind, Frame, FrameReader, FrameWriter, MAX_FRAME_LEN, ProtoError, Scrollback,
    SessionExit, SessionInfo, TerminalAppearance, TerminalColor, decode_frame, encode_frame,
};

/// Covers every frame kind in the current protocol. Add new frames here in
/// lockstep with the protocol version bump and explicit assertion below.
fn all_frames() -> Vec<Frame> {
    vec![
        Frame::Hello {
            proto_version: 0,
            kind: ClientKind::Tui,
        },
        Frame::HelloAck {
            proto_version: 0,
            daemon_version: "0.1.0".into(),
        },
        Frame::ListSessions,
        Frame::SessionList {
            sessions: vec![
                SessionInfo {
                    name: "s0".into(),
                    command: "/bin/bash".into(),
                    title: "user@host: ~".into(),
                    created_ms: 1_752_450_000_000,
                    idle_ms: 1500,
                    running: true,
                    state: AgentState::Blocked,
                    attached_clients: 2,
                    pid: 4242,
                    cols: 120,
                    rows: 40,
                },
                SessionInfo {
                    name: "work".into(),
                    command: "htop".into(),
                    title: String::new(),
                    created_ms: 0,
                    idle_ms: 0,
                    running: false,
                    state: AgentState::Idle,
                    attached_clients: 0,
                    pid: 4242,
                    cols: 80,
                    rows: 24,
                },
            ],
        },
        Frame::Create {
            name: Some("work".into()),
            cmd: Some("htop".into()),
            cwd: Some("/srv/app".into()),
        },
        Frame::Create {
            name: None,
            cmd: None,
            cwd: None,
        },
        Frame::Created { name: "s0".into() },
        Frame::Kill { name: "s0".into() },
        Frame::Rename {
            name: "s0".into(),
            new_name: "build".into(),
        },
        Frame::Attach {
            name: "s0".into(),
            cols: 120,
            rows: 40,
            view_id: 7,
            appearance: TerminalAppearance {
                foreground: Some(TerminalColor {
                    r: 0xe7,
                    g: 0xe2,
                    b: 0xd6,
                }),
                background: Some(TerminalColor {
                    r: 0x0b,
                    g: 0x0d,
                    b: 0x11,
                }),
            },
            read_only: false,
        },
        Frame::Attach {
            name: "unknown-theme".into(),
            cols: 80,
            rows: 24,
            view_id: 0,
            appearance: TerminalAppearance::default(),
            read_only: false,
        },
        Frame::Attach {
            name: "watched".into(),
            cols: 80,
            rows: 24,
            view_id: 0,
            appearance: TerminalAppearance::default(),
            read_only: true,
        },
        Frame::Snapshot {
            vt: b"\x1b[2J\x1b[Hhello".to_vec(),
        },
        Frame::Output {
            bytes: vec![0u8, 255, 27, 91],
        },
        Frame::Input {
            bytes: b"ls -la\r".to_vec(),
        },
        Frame::Resize { cols: 80, rows: 24 },
        Frame::Detach,
        Frame::FetchHistory {
            start: 100,
            count: 40,
        },
        Frame::History {
            total_rows: 512,
            start: 100,
            rows: vec![b"line one".to_vec(), b"line two".to_vec(), Vec::new()],
        },
        Frame::Refresh,
        Frame::SendInput {
            name: "build".into(),
            bytes: b"make test".to_vec(),
            enter: true,
        },
        Frame::Ack,
        Frame::Peek {
            name: "build".into(),
            scrollback: Scrollback::All,
        },
        Frame::Peek {
            name: "build".into(),
            scrollback: Scrollback::None,
        },
        Frame::Peek {
            name: "build".into(),
            scrollback: Scrollback::Lines(500),
        },
        Frame::PeekReply {
            cols: 80,
            rows: 24,
            cursor_col: 5,
            cursor_row: 12,
            title: "user@host: ~".into(),
            screen: b"line one\nline two".to_vec(),
        },
        Frame::Inspect {
            name: "work".into(),
        },
        Frame::InspectReply {
            info: SessionInfo {
                name: "work".into(),
                command: "vim file".into(),
                title: "vim".into(),
                created_ms: 1_752_450_000_000,
                idle_ms: 42,
                running: true,
                state: AgentState::Working,
                attached_clients: 1,
                pid: 4242,
                cols: 100,
                rows: 30,
            },
            child_pid: 4242,
            alt_screen: true,
            scrollback_rows: 512,
            mouse_tracking: true,
            mouse_modes: vec![1002, 1006],
            cursor_col: 7,
            cursor_row: 3,
            cursor_visible: true,
        },
        Frame::Follow { name: "s0".into() },
        Frame::Unfollow { name: "s0".into() },
        Frame::FollowStatus {
            running: true,
            state: AgentState::Working,
            idle_ms: 12,
            exit: None,
        },
        Frame::FollowStatus {
            running: false,
            state: AgentState::Idle,
            idle_ms: 2500,
            exit: None,
        },
        // The last status a session sends: how its child ended, both ways.
        Frame::FollowStatus {
            running: false,
            state: AgentState::Unknown,
            idle_ms: 40,
            exit: Some(SessionExit {
                code: 3,
                signal: None,
            }),
        },
        Frame::FollowStatus {
            running: false,
            state: AgentState::Unknown,
            idle_ms: 40,
            exit: Some(SessionExit {
                code: 1,
                signal: Some("SIGHUP".into()),
            }),
        },
        Frame::ViewRevoked {
            name: "build".into(),
            view_id: 7,
        },
        Frame::ViewRenamed {
            old_name: "build".into(),
            new_name: "review".into(),
            view_id: 7,
        },
        Frame::Error {
            code: asd_proto::code::VERSION_MISMATCH,
            msg: "proto version mismatch".into(),
        },
        Frame::HostMetrics,
        Frame::HostMetricsReply {
            sample: Some(asd_proto::HostSample {
                cpu_pct: 12,
                mem_used_bytes: 6_500_000_000,
                mem_total_bytes: 33_000_000_000,
                net_rx_bps: 1_258_291,
                net_tx_bps: 348_160,
                sampled_age_ms: 740,
            }),
        },
        Frame::HostMetricsReply { sample: None },
    ]
}

#[test]
fn protocol_version_covers_host_metrics() {
    assert_eq!(asd_proto::PROTO_VERSION, 17);
}

#[test]
fn every_frame_roundtrips_through_encode_decode() {
    for frame in all_frames() {
        let buf = encode_frame(&frame).unwrap();
        let (len_prefix, payload) = buf.split_at(4);
        assert_eq!(
            u32::from_le_bytes(len_prefix.try_into().unwrap()) as usize,
            payload.len(),
            "length prefix must match payload length for {frame:?}"
        );
        let decoded = decode_frame(payload).unwrap();
        assert_eq!(decoded, frame);
    }
}

#[tokio::test]
async fn every_frame_roundtrips_through_reader_writer() {
    let mut wire = Vec::new();
    {
        let mut writer = FrameWriter::new(&mut wire);
        for frame in all_frames() {
            writer.write_frame(&frame).await.unwrap();
        }
    }
    let mut reader = FrameReader::new(wire.as_slice());
    for expected in all_frames() {
        let got = reader.read_frame().await.unwrap().unwrap();
        assert_eq!(got, expected);
    }
    // After all frames are read, EOF lands cleanly on a frame boundary.
    assert!(reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn eof_at_frame_boundary_is_clean_close() {
    let mut reader = FrameReader::new(&[][..]);
    assert!(reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn truncated_length_prefix_is_error() {
    // Stream cut after only 2 length bytes: EOF mid-frame, not a clean close.
    let mut reader = FrameReader::new(&[0x05, 0x00][..]);
    match reader.read_frame().await {
        Err(ProtoError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[tokio::test]
async fn truncated_payload_is_error() {
    let mut wire = encode_frame(&Frame::Created { name: "s0".into() }).unwrap();
    wire.truncate(wire.len() - 1);
    let mut reader = FrameReader::new(wire.as_slice());
    match reader.read_frame().await {
        Err(ProtoError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_length_prefix_is_protocol_error() {
    let len = (MAX_FRAME_LEN as u32) + 1;
    let mut wire = len.to_le_bytes().to_vec();
    wire.extend_from_slice(&[0u8; 16]);
    let mut reader = FrameReader::new(wire.as_slice());
    match reader.read_frame().await {
        Err(ProtoError::FrameTooLarge(n)) => assert_eq!(n, len as usize),
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn oversized_frame_is_rejected_on_encode() {
    let frame = Frame::Output {
        bytes: vec![0u8; MAX_FRAME_LEN + 1],
    };
    match encode_frame(&frame) {
        Err(ProtoError::FrameTooLarge(_)) => {}
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn garbage_payload_is_codec_error() {
    // Length is valid but postcard cannot decode the frame enum (255 is not a
    // valid discriminant).
    let mut wire = 4u32.to_le_bytes().to_vec();
    wire.extend_from_slice(&[255, 255, 255, 255]);
    let mut reader = FrameReader::new(wire.as_slice());
    match reader.read_frame().await {
        Err(ProtoError::Codec(_)) => {}
        other => panic!("expected Codec error, got {other:?}"),
    }
}

/// A read cancelled mid-frame must not eat the bytes it already took.
///
/// Every client drives its connection with `tokio::select!`, which drops the
/// losing branch's future — the frame read — on every heartbeat tick and every
/// keystroke. If the partially read frame lived in that future, those bytes
/// would be gone and the stream would resume at a payload byte, reading it as a
/// length prefix: the connection dies with a bogus length or a codec error,
/// which is what a user sees as the daemon "going down" mid-session.
#[tokio::test]
async fn a_cancelled_read_resumes_the_same_frame() {
    use tokio::io::AsyncWriteExt;

    let frame = Frame::Output {
        bytes: vec![7u8; 64],
    };
    let wire = encode_frame(&frame).unwrap();
    let (mut peer, stream) = tokio::io::duplex(1024);
    let mut reader = FrameReader::new(stream);

    // Enough for the length prefix and a slice of the payload — not the frame.
    peer.write_all(&wire[..6]).await.unwrap();

    // Poll once (which consumes those bytes), then drop the future, exactly as
    // `select!` does to the branch that loses the race.
    {
        let mut read = std::pin::pin!(reader.read_frame());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(read.as_mut(), &mut cx).is_pending(),
            "the frame is incomplete, so the read must be pending"
        );
    }

    peer.write_all(&wire[6..]).await.unwrap();
    assert_eq!(reader.read_frame().await.unwrap(), Some(frame));
}
