//! Round-trip and wire-format tests for the generated protobuf messages.
//!
//! Wire-compat assertions are hand-computed from the upstream `.proto` field numbers
//! and protobuf wire types, independent of prost's encoder.

use prost::Message;
// prost renders `CNETMsg_Tick` as `CnetMsgTick` (underscores dropped, casing normalized).
use s2sdk::proto::{
    c_msg_source1_legacy_game_event::KeyT, CMsgSource1LegacyGameEvent, CMsgVector, CnetMsgTick,
    NetMessages, SvcMessages,
};

#[test]
fn cmsg_vector_round_trip() {
    let msg = CMsgVector {
        x: Some(1.5),
        y: Some(-2.25),
        z: Some(64.0),
        w: None,
    };
    let bytes = msg.encode_to_vec();
    let decoded = CMsgVector::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded, msg);
}

/// Field 1 (`x`), wire type 5 (fixed 32-bit) -> tag byte (1 << 3) | 5 = 0x0d,
/// followed by the little-endian f32 bit pattern of 1.0 (0x3f800000).
#[test]
fn cmsg_vector_wire_format() {
    let expected: &[u8] = &[0x0d, 0x00, 0x00, 0x80, 0x3f];

    let mut encoded = Vec::new();
    CMsgVector {
        x: Some(1.0),
        ..Default::default()
    }
    .encode(&mut encoded)
    .unwrap();
    assert_eq!(encoded, expected);

    let decoded = CMsgVector::decode(expected).unwrap();
    assert_eq!(decoded.x, Some(1.0));
    assert_eq!(decoded.y, None);
    assert_eq!(decoded.z, None);
    assert_eq!(decoded.w, None);
}

#[test]
fn cnetmsg_tick_round_trip() {
    let msg = CnetMsgTick {
        tick: Some(123456),
        host_computationtime: Some(4400),
        host_computationtime_std_deviation: Some(120),
        legacy_host_loss: None,
        host_unfiltered_frametime: Some(4390),
        hltv_replay_flags: None,
        expected_long_tick: None,
        expected_long_tick_reason: None,
        host_frame_dropped_pct_x10: Some(0),
        host_frame_irregular_arrival_pct_x10: Some(0),
    };
    let bytes = msg.encode_to_vec();
    let decoded = CnetMsgTick::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn cmsg_source1_legacy_game_event_round_trip() {
    let msg = CMsgSource1LegacyGameEvent {
        event_name: Some("player_death".to_string()),
        eventid: Some(42),
        keys: vec![
            KeyT {
                r#type: Some(1),
                val_string: Some("CT".to_string()),
                ..Default::default()
            },
            KeyT {
                r#type: Some(3),
                val_long: Some(7),
                ..Default::default()
            },
            KeyT {
                r#type: Some(5),
                val_bool: Some(true),
                ..Default::default()
            },
        ],
        server_tick: Some(98765),
        passthrough: None,
    };
    let bytes = msg.encode_to_vec();
    let decoded = CMsgSource1LegacyGameEvent::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded, msg);
}

/// Enum discriminators must match the upstream `.proto` values verbatim: the generated
/// constants feed network message dispatch, so a renumbered enum would silently
/// misroute messages. `net_Tick = 4` and `svc_UserMessage = 72` per
/// networkbasetypes.proto / netmessages.proto.
#[test]
fn enum_values_match_upstream() {
    assert_eq!(i32::from(NetMessages::NetTick), 4);
    assert_eq!(i32::from(NetMessages::NetNop), 0);
    assert_eq!(i32::from(SvcMessages::SvcUserMessage), 72);
}
