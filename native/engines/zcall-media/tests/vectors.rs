//! Replays tests/vectors.json (produced by the JS engine, gen-vectors.js)
//! through the Rust port: every byte and every log line must match.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zcall_media::json::num;
use zcall_media::rtcp::{self, Kind, NackTracker, RtcpFeedback};
use zcall_media::rtp::{self, GroupAudio, GroupVideo, SfuExt};
use zcall_media::send_rate::{Options, SendRate};
use zcall_media::srtp;
use zcall_media::zrtp::{self, GroupInit, LayerEntry, Packet};

fn vectors() -> Value {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn h(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn hx(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| v.as_f64().unwrap() as u64)
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

/// Numbers as f64, so 300000 (JS) and 300000.0 (Rust f64) compare equal.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), norm(x))).collect()),
        other => other.clone(),
    }
}

#[test]
fn srtp_matches_js() {
    let v = vectors();
    for k in v["srtp"].as_array().unwrap() {
        let master = h(&k["master"]);
        let keys = srtp::session_keys(srtp::master_from_sess_id(&master).unwrap()).unwrap();
        assert_eq!(hx(&keys.enc), k["enc"]);
        assert_eq!(hx(&keys.auth), k["auth"]);
        assert_eq!(hx(&keys.salt), k["salt"]);
        for p in k["packets"].as_array().unwrap() {
            let (seq, ssrc, roc) = (u(&p["seq"]) as u16, u(&p["ssrc"]) as u32, u(&p["roc"]) as u32);
            assert_eq!(hx(&srtp::packet_iv(&keys.salt, ssrc, seq, roc)), p["iv"]);
            let prot = srtp::protect_rtp(&h(&p["header"]), &h(&p["payload"]), &keys, seq, ssrc, roc);
            assert_eq!(hx(&prot), p["protected"]);
            let un = srtp::unprotect_rtp(&prot, &keys, roc).map(|x| hx(&x.payload));
            assert_eq!(un.map(Value::from).unwrap_or(Value::Null), p["unprotected"]);
            assert_eq!(srtp::unprotect_rtp(&h(&p["tampered"]), &keys, roc).is_some(), p["tamperedOk"].as_bool().unwrap());
            assert_eq!(srtp::unprotect_rtp(&prot, &keys, roc + 1).is_some(), p["wrongRocOk"].as_bool().unwrap());
        }
    }
    for c in v["parseRtp"].as_array().unwrap() {
        let b = h(&c["buf"]);
        let got = srtp::parse_rtp(&b).map(|p| p.hdr.len() as u64);
        assert_eq!(got.map(Value::from).unwrap_or(Value::Null), c["hdrLen"], "{}", c["buf"]);
    }
    assert!(srtp::session_keys(&[0; 29]).is_err());
}

#[test]
fn rtp_headers_match_js() {
    for c in vectors()["rtp"].as_array().unwrap() {
        let (seq, ts, ssrc, tw) = (u(&c["seq"]) as u16, u(&c["ts"]) as u32, u(&c["ssrc"]) as u32, u(&c["twSeq"]) as u16);
        let (marker, pt) = (c["marker"].as_bool().unwrap(), u(&c["pt"]) as u8);
        assert_eq!(hx(&rtp::audio_header(seq, ts, ssrc, tw, marker, pt)), c["audio"]);
        let ga = GroupAudio { seq, ts, ssrc, tw_seq: tw, marker, pt, capture_ms: f(&c["captureMs"]), level: u(&c["level"]).min(255) as u8 };
        assert_eq!(hx(&rtp::group_audio_header(&ga)), c["groupAudio"]);
        let sfu = c["sfu"].as_object().map(|o| SfuExt { id: o.get("id").map_or(0, |v| u(v) as u8), long: o.get("long").and_then(|v| v.as_bool()).unwrap_or(false) });
        let gv = GroupVideo { seq, ts, ssrc, tw_seq: tw, marker, pt, sfu, layer_seq: u(&c["layerSeq"]) as u16, capture_ms: f(&c["captureMs"]), send_ms: f(&c["sendMs"]) };
        assert_eq!(hx(&rtp::group_video_header(&gv)), c["groupVideo"]);
    }
}

#[test]
fn zrtp_builders_match_js() {
    for c in vectors()["zrtp"].as_array().unwrap() {
        let g = |k: &str| u(&c[k]) as u32;
        let (uid, token, call_id, peer, seq) = (g("uid"), g("token"), g("callId"), g("peerUid"), g("seq"));
        let sess = h(&c["sess"]);
        let payload = h(&c["payload"]);
        let callee = c["role"] == "callee";
        assert_eq!(hx(&zrtp::wrap_media(g("msgType") as u8, token, &payload)), c["media"]);
        assert_eq!(hx(&zrtp::init_zrtp(!callee, uid, peer, call_id, &sess, &h(&c["extra"]), 2)), c["init"]);
        let gi = GroupInit { host: c["host"].as_bool().unwrap(), uid, sess_id: &sess, video_on: c["videoOn"].as_bool().unwrap(), call_id, host_call: g("hostCall"), layout: c["layout"].as_str().unwrap() };
        assert_eq!(hx(&zrtp::group_init_zrtp(&gi)), c["group"]);
        assert_eq!(hx(&zrtp::share_init_zrtp(uid, &sess)), c["shareInit"]);
        assert_eq!(hx(&zrtp::share_zavi_ping(uid, token, &sess)), c["sharePing"]);
        assert_eq!(hx(&zrtp::zavi_ping(uid, token, &sess, gi.video_on)), c["zavi"]);
        assert_eq!(hx(&zrtp::ping(uid, token, &sess)), c["ping"]);
        assert_eq!(hx(&zrtp::end_call(uid, token, call_id)), c["end"]);
        assert_eq!(hx(&zrtp::req_forward(uid, token, seq, g("subCmd") as u8, &payload)), c["fwd"]);
        assert_eq!(hx(&zrtp::req_forward_status(g("rxs"), g("a"), g("b5"))), c["fwdStatus"]);
        let entries: Vec<(u32, u8)> = c["entries"].as_array().unwrap().iter().map(|e| (u(&e["uid"]) as u32, u(&e["qualityId"]) as u8)).collect();
        assert_eq!(hx(&zrtp::group_vid_qual_req(uid, token, seq, &entries)), c["vqReq"]);
        let layers: Vec<LayerEntry> = c["layers"].as_array().unwrap().iter().map(|l| LayerEntry { id: u(&l["id"]) as u32, a: u(&l["a"]) as u32, b: u(&l["b"]) as u32, c: u(&l["c"]) as u32 }).collect();
        assert_eq!(hx(&zrtp::group_vid_qual_config(uid, token, seq, &layers)), c["vqCfg"]);
        assert_eq!(hx(&zrtp::group_sync(uid, token, seq, g("subCmd") as u8)), c["sync"]);
        assert_eq!(hx(&zrtp::p2p_bind(callee, call_id, u(&c["p2pSeq"]) as u16, g("index"))), c["p2pBind"]);
        let req = h(&c["req"]);
        assert_eq!(hx(&zrtp::p2p_bind_answer(callee, call_id, &req, c["peerAddr"].as_str().unwrap())), c["p2pAnswer"]);
        assert_eq!(hx(&zrtp::p2p_pong(callee, &req)), c["p2pPong"]);
        assert_eq!(hx(&zrtp::p2p_wrap(callee, call_id, zrtp::p2p::AUDIO, &payload)), c["p2pAudio"]);
        assert_eq!(hx(&zrtp::p2p_wrap(callee, call_id, zrtp::p2p::VIDEO, &payload)), c["p2pVideo"]);
        assert_eq!(hx(&zrtp::p2p_wrap(callee, call_id, zrtp::p2p::AUDIO_RTCP, &payload)), c["p2pRtcpA"]);
        assert_eq!(hx(&zrtp::p2p_wrap(callee, call_id, zrtp::p2p::VIDEO_RTCP, &payload)), c["p2pRtcpV"]);
    }
}

/// The JS unwrapPacket object for a Rust Packet (Buffers as hex, `raw` dropped).
fn packet_json(p: &Packet) -> Value {
    match p {
        Packet::Media { msg_type, token: None, payload } => json!({"isMedia": true, "msgType": msg_type, "payload": hx(payload)}),
        Packet::Media { msg_type, token: Some(t), payload } => json!({"isMedia": true, "msgType": msg_type, "token": t, "payload": hx(payload)}),
        Packet::MediaTruncated { msg_type } => json!({"isMedia": true, "msgType": msg_type, "truncated": true}),
        Packet::P2p { b1, b2, field38, cmd, data } => json!({"isMedia": false, "msgType": 0x7f, "b1": b1, "b2": b2, "field38": field38, "cmd": cmd, "data": hx(data)}),
        Packet::Other { msg_type } => json!({"isMedia": false, "msgType": msg_type}),
        Packet::Control(c) => {
            let mut o = json!({"isMedia": false, "msgType": c.msg_type, "magic": c.magic, "field02": c.field02, "seq": c.seq, "uid": c.uid,
                "token": c.token, "cmd": c.cmd, "subCmd": c.sub_cmd, "data": hx(c.data)});
            let m = o.as_object_mut().unwrap();
            if let Some(v) = c.res { m.insert("res".into(), json!(v)); }
            if let Some(v) = c.call_id { m.insert("callId".into(), json!(v)); }
            if let Some(v) = c.server_token { m.insert("serverToken".into(), json!(v)); }
            if let Some(v) = &c.public_addr { m.insert("publicAddr".into(), json!(v)); }
            if let Some(v) = c.peer_uid { m.insert("peerUid".into(), json!(v)); }
            if let Some(v) = c.sess_id { m.insert("sessId".into(), json!(hx(v))); }
            if let Some(v) = c.extra { m.insert("extra".into(), json!(hx(v))); }
            o
        }
    }
}

#[test]
fn unwrap_matches_js() {
    let v = vectors();
    for c in v["unwrap"].as_array().unwrap() {
        let b = h(&c["buf"]);
        let got = zrtp::unwrap(&b).map(|p| packet_json(&p)).unwrap_or(Value::Null);
        assert_eq!(got, c["parsed"], "buf {}", c["buf"]);
    }
    for c in v["roomUpdated"].as_array().unwrap() {
        let got = zrtp::parse_room_updated(&h(&c["buf"]));
        assert_eq!(got.map(|x| json!(x)).unwrap_or(Value::Null), c["uids"]);
    }
}

#[test]
fn group_video_plan_matches_js() {
    for c in vectors()["plans"].as_array().unwrap() {
        let cam = c["cam"].as_str().map(|s| num(Some(&Value::from(s))));
        let got = zrtp::group_video_plan(&c["config"], cam).map(|p| {
            let le = |l: &[LayerEntry]| l.iter().map(|x| json!({"id": x.id, "a": x.a, "b": x.b, "c": x.c})).collect::<Vec<_>>();
            json!({
                "layers": le(&p.layers),
                "encode": {"width": p.encode.width, "height": p.encode.height, "fps": p.encode.fps, "bitrate": p.encode.bitrate, "keyMs": p.encode.key_ms},
                "send": le(&p.send),
                "sfu": p.sfu.map(|s| json!({"id": s.id, "long": s.long})),
                "rows": p.rows.iter().map(|r| json!({"width": r.width, "height": r.height, "fps": r.fps, "kbps": r.kbps})).collect::<Vec<_>>(),
                "start": p.start,
            })
        });
        assert_eq!(got.unwrap_or(Value::Null), c["plan"], "config {} cam {}", c["config"], c["cam"]);
    }
}

fn twcc_json(p: &rtcp::TwccFeedback) -> Value {
    json!({"senderSsrc": p.sender_ssrc, "mediaSsrc": p.media_ssrc, "base": p.base, "count": p.count, "refUnits": p.ref_units,
        "fbCount": p.fb_count, "status": p.status, "deltaQ": p.delta_q})
}

#[test]
fn rtcp_feedback_matches_js() {
    for call in vectors()["rtcpCalls"].as_array().unwrap() {
        let mut fb = RtcpFeedback::new(u(&call["local"]) as u32);
        for (i, op) in call["ops"].as_array().unwrap().iter().enumerate() {
            let t = f(&op["t"]);
            let kind = if op["kind"] == "video" { Kind::Video } else { Kind::Audio };
            match op["op"].as_str().unwrap() {
                "rtp" => fb.on_rtp(kind, &h(&op["pkt"]), t),
                "rtcp" => fb.on_rtcp(kind, &h(&op["buf"]), t),
                "twcc" => {
                    let pk = fb.twcc_packets();
                    assert_eq!(json!(pk.iter().map(|p| hx(p)).collect::<Vec<_>>()), op["out"], "op {i}");
                    let parsed: Vec<Value> = pk.iter().map(|p| twcc_json(&rtcp::parse_twcc(p).unwrap())).collect();
                    assert_eq!(json!(parsed), op["parsed"], "op {i}");
                }
                "areport" => {
                    let rep = fb.audio_report(t, u(&op["rtpTs"]) as u32, u(&op["packets"]) as u32, u(&op["octets"]) as u32);
                    assert_eq!(hx(&rep), op["out"], "op {i}");
                }
                "vreport" => assert_eq!(fb.video_report(t).map(|b| json!(hx(&b))).unwrap_or(Value::Null), op["out"], "op {i}"),
                "pli" => assert_eq!(fb.pli().map(|b| json!(hx(&b))).unwrap_or(Value::Null), op["out"], "op {i}"),
                other => panic!("{other}"),
            }
        }
        let c = &call["counters"];
        assert_eq!((fb.counters.twcc, fb.counters.reports, fb.counters.pli, fb.counters.nack), (u(&c["twcc"]), u(&c["reports"]), u(&c["pli"]), u(&c["nack"])));
        assert_eq!(fb.twcc.sent, u(&call["sent"]));
    }
}

#[test]
fn nack_matches_js() {
    let v = vectors();
    let mut tr = NackTracker::default();
    for (i, c) in v["nack"].as_array().unwrap().iter().enumerate() {
        let res = tr.note(u(&c["ssrc"]) as u32, u(&c["seq"]) as u16, f(&c["t"]));
        let got: Vec<Value> = res.iter().map(|(s, q)| json!({"ssrc": s, "seqs": q})).collect();
        assert_eq!(json!(got), c["out"], "step {i}");
        let pk: Vec<String> = res.iter().map(|(s, q)| hx(&rtcp::build_nack(7, *s, q))).collect();
        assert_eq!(json!(pk), c["pkts"], "step {i}");
    }
    for c in v["buildNack"].as_array().unwrap() {
        let seqs: Vec<u16> = c["seqs"].as_array().unwrap().iter().map(|s| u(s) as u16).collect();
        assert_eq!(hx(&rtcp::build_nack(1, 2, &seqs)), c["out"]);
    }
    for c in v["readTwSeq"].as_array().unwrap() {
        assert_eq!(rtcp::read_tw_seq(&h(&c["buf"])).map(|x| json!(x)).unwrap_or(Value::Null), c["tw"], "{}", c["buf"]);
    }
}

#[test]
fn send_rate_matches_js() {
    let v = vectors();
    let plan_cfg = &v["sendRate"].as_array().unwrap().last().unwrap()["config"];
    let plan = zrtp::group_video_plan(plan_cfg, None).unwrap();
    for (k, c) in v["sendRate"].as_array().unwrap().iter().enumerate() {
        let now = Arc::new(Mutex::new(f(&c["start"])));
        let logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let profiles = Arc::new(Mutex::new(Vec::<Value>::new()));
        let group = c["group"].as_bool().unwrap();
        let (n2, l2, p2) = (now.clone(), logs.clone(), profiles.clone());
        let mut sr = SendRate::new(&c["config"], Options {
            local_ssrc: u(&c["local"]) as u32,
            log: Box::new(move |l| l2.lock().unwrap().push(l)),
            on_profile: Box::new(move |p| {
                p2.lock().unwrap().push(json!({"width": p.width, "height": p.height, "fps": p.fps, "bitrate": p.bitrate, "keyMs": p.key_ms, "captureBig": p.capture_big, "rung": p.rung}))
            }),
            now: Box::new(move || *n2.lock().unwrap()),
            layers: group.then(|| plan.rows.clone()),
            start_index: plan.start,
            env_rung: c["env"].as_str().map(str::to_string),
            env_adapt_off: false,
        })
        .unwrap();
        for op in c["ops"].as_array().unwrap() {
            match op["op"].as_str().unwrap() {
                "sent" => {
                    *now.lock().unwrap() = f(&op["ms"]);
                    sr.on_sent(u(&op["tw"]) as u16, Some(f(&op["ms"])));
                }
                "rtcp" => sr.on_rtcp(if op["kind"] == "video" { 'v' } else { 'a' }, &h(&op["buf"])),
                "tick" => {
                    *now.lock().unwrap() = f(&op["now"]);
                    sr.tick(op["sending"].as_bool().unwrap(), op["downLoss"].as_f64());
                }
                other => panic!("{other}"),
            }
        }
        let logs = logs.lock().unwrap();
        let want: Vec<&str> = c["logs"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        for (i, (a, b)) in logs.iter().zip(&want).enumerate() {
            assert_eq!(a, b, "send-rate {k} log line {i}");
        }
        assert_eq!(logs.len(), want.len(), "send-rate {k} log count");
        assert_eq!(norm(&json!(*profiles.lock().unwrap())), norm(&c["profiles"]), "send-rate {k} profiles");
        let t = &c["totals"];
        assert_eq!(
            (sr.totals.twcc_pkts, sr.totals.twcc_lost, sr.totals.rr_blocks, sr.totals.changes, sr.totals.downs),
            (u(&t["twccPkts"]), u(&t["twccLost"]), u(&t["rrBlocks"]), u(&t["changes"]), u(&t["downs"])),
            "send-rate {k} totals"
        );
        assert_eq!(sr.level as u64, u(&c["level"]));
    }
}
