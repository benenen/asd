//! Protocol test contract (spec §8): roundtrip for every frame kind +
//! truncated/oversized frame error paths.

use asd_proto::{
    ClientKind, Frame, FrameReader, FrameWriter, MAX_FRAME_LEN, ProtoError, SessionInfo,
    decode_frame, encode_frame,
};

/// Covers all frame kinds of protocol v1. New frames must be added here in
/// lockstep.
fn all_frames() -> Vec<Frame> {
    vec![
        Frame::Hello {
            proto_version: 0,
            kind: ClientKind::Gui,
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
                    attached_clients: 2,
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
                    attached_clients: 0,
                    cols: 80,
                    rows: 24,
                },
            ],
        },
        Frame::Create {
            name: Some("work".into()),
            cmd: Some("htop".into()),
        },
        Frame::Create {
            name: None,
            cmd: None,
        },
        Frame::Created { name: "s0".into() },
        Frame::Kill { name: "s0".into() },
        Frame::Attach {
            name: "s0".into(),
            cols: 120,
            rows: 40,
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
            bytes: b"make test\r".to_vec(),
        },
        Frame::Ack,
        Frame::Peek {
            name: "build".into(),
            scrollback: true,
        },
        Frame::PeekReply {
            cols: 80,
            rows: 24,
            cursor_col: 5,
            cursor_row: 12,
            title: "user@host: ~".into(),
            screen: b"line one\nline two".to_vec(),
        },
        Frame::Error {
            code: asd_proto::code::VERSION_MISMATCH,
            msg: "proto version mismatch".into(),
        },
    ]
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
