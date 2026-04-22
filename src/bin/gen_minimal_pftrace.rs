// tools/servoperf/src/bin/gen_minimal_pftrace.rs
//! Writes tests/fixtures/minimal.pftrace: a tiny, deterministic pftrace
//! containing one complete span ("A" 100ns..200ns) and one instant event
//! ("FirstContentfulPaint" at 300ns) on a thread called "main".
//!
//! Run with:  cargo run --bin gen_minimal_pftrace

use prost::Message;

#[allow(unused_imports)]
#[path = "../proto.rs"]
mod proto;

use proto::perfetto_protos::{
    ThreadDescriptor, TrackDescriptor, TracePacket, TrackEvent,
    track_event::{NameField, Type as TE},
    trace_packet::{Data, OptionalTrustedPacketSequenceId},
    Trace,
};

fn main() {
    let seq: u32 = 1;
    let main_track: u64 = 42;

    let packets = vec![
        // Thread descriptor for track 42 → "main" thread.
        {
            let mut p = TracePacket::default();
            p.optional_trusted_packet_sequence_id =
                Some(OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(seq));
            let mut td = TrackDescriptor::default();
            td.uuid = Some(main_track);
            let mut thread = ThreadDescriptor::default();
            thread.pid = Some(1);
            thread.tid = Some(1);
            thread.thread_name = Some("main".to_string());
            td.thread = Some(thread);
            p.data = Some(Data::TrackDescriptor(td));
            p
        },
        // BEGIN "A" at 100ns on track 42.
        event(seq, main_track, 100, "A", TE::SliceBegin),
        // END "A" at 200ns.
        event(seq, main_track, 200, "A", TE::SliceEnd),
        // Instant "FirstContentfulPaint" at 300ns.
        event(seq, main_track, 300, "FirstContentfulPaint", TE::Instant),
    ];

    let trace = Trace { packet: packets };
    let out = trace.encode_to_vec();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("minimal.pftrace");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, out).unwrap();
    println!("wrote {}", path.display());
}

fn event(seq: u32, track: u64, ts: u64, name: &str, ty: TE) -> TracePacket {
    let mut p = TracePacket::default();
    p.optional_trusted_packet_sequence_id =
        Some(OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(seq));
    p.timestamp = Some(ts);
    let mut e = TrackEvent::default();
    e.track_uuid = Some(track);
    e.name_field = Some(NameField::Name(name.to_string()));
    e.r#type = Some(ty as i32);
    p.data = Some(Data::TrackEvent(e));
    p
}
