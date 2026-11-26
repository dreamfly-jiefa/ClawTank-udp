//! End-to-end EGFX test: an IronRDP client that offers the Graphics Pipeline connects, the
//! core reports `VideoStart`, and H.264 submitted through `VideoHub` reaches the client as
//! AVC420 byte-for-byte. Also checks the keyframe rule (no P-frames before the first IDR).

#[path = "support/client.rs"]
mod client;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawtank_core::events::CoreEvent;
use clawtank_core::server::{self, ServerConfig};
use clawtank_core::tls::TlsIdentity;
use clawtank_core::video::SubmitResult;

const USER: &str = "tester";
const PASSWORD: &str = "video-test-password";

/// Not decodable H.264, just Annex-B-shaped bytes (SPS, PPS, IDR slice); the server treats the
/// payload as opaque, and the test client records rather than decodes.
const FAKE_IDR: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x2a, 0xab, //
    0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, //
    0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff,
];
const FAKE_P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02, 0x04];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_h264_reaches_an_egfx_client() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("video-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
        udp: false,
    };
    let server = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "no session yet"
    );

    let recorder = client::EgfxRecorder::default();
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        1512,
        982,
        Some(&recorder),
    )
    .await
    .expect("connect with EGFX");

    // Pump until the server reports the pipeline ready (capability exchange runs over DVC).
    let started = {
        let events = Arc::clone(&events);
        move || {
            events.lock().unwrap().iter().any(|e| {
                matches!(
                    e,
                    CoreEvent::VideoStart {
                        width: 1512,
                        height: 982,
                        scale_percent: 100
                    }
                )
            })
        }
    };
    // `pump_until` consumes the connection result, so build a stage once and keep pumping.
    let ok = client::pump_until(result, &mut session, Duration::from_secs(15), {
        let video = Arc::clone(&video);
        let recorder = recorder.clone();
        let mut submitted = false;
        move || {
            if !started() {
                return false;
            }
            if !submitted {
                // The decoder chain must start at an IDR.
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::NeedKeyframe
                );
                assert_eq!(
                    video.submit_h264(FAKE_IDR, true, 1512, 982),
                    SubmitResult::Sent
                );
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::Sent
                );
                submitted = true;
            }
            recorder.frame_count() >= 2
        }
    })
    .await
    .expect("session");
    assert!(ok, "timed out; events: {:?}", events.lock().unwrap());

    let streams = recorder.bitstreams.lock().unwrap().clone();
    assert_eq!(
        streams,
        vec![FAKE_IDR.to_vec(), FAKE_P.to_vec()],
        "bitstreams must arrive unchanged"
    );
    assert_eq!(*recorder.reset_size.lock().unwrap(), Some((1512, 982)));
    // Acks drive adaptive bitrate: the client acknowledges each frame and we time it.
    // The client sent its acks while pumping above; give the server a moment to process them.
    let mut ok = false;
    for _ in 0..100 {
        if video.stats().frames_acked >= 2 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stats = video.stats();
    assert!(ok, "client acknowledged frames: {stats:?}");
    assert_eq!((stats.frames_sent, stats.frames_dropped), (2, 1));
    assert!(stats.ack_latency_us > 0, "ack latency measured: {stats:?}");

    drop(session);
    for _ in 0..100 {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
    );
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "session is gone"
    );
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_scale_reaches_video_start() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("video-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
        udp: false,
    };
    let server = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();

    let mut config = client::client_config(USER, PASSWORD, 1512, 982);
    config.desktop_scale_factor = 200;
    config.device_scale_factor = Some(180);
    let recorder = client::EgfxRecorder::default();
    let (result, mut session, _) =
        client::connect_with(server.local_addr(), config, Some(&recorder))
            .await
            .unwrap();

    let mut stage = client::active_stage(result);
    let mut image = ironrdp_session::image::DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        1512,
        982,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !events.lock().unwrap().iter().any(|e| {
        matches!(
            e,
            CoreEvent::VideoStart {
                scale_percent: 200,
                ..
            }
        )
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "VideoStart never carried the client scale: {:?}",
            events.lock().unwrap()
        );
        let read = tokio::time::timeout(Duration::from_millis(200), session.read_pdu()).await;
        let Ok(read) = read else { continue };
        let (action, payload) = read.unwrap();
        for out in stage.process(&mut image, action, &payload, None).unwrap() {
            if let ironrdp_session::ActiveStageOutput::ResponseFrame(bytes) = out {
                ironrdp_async::FramedWrite::write_all(&mut session, &bytes)
                    .await
                    .unwrap();
            }
        }
    }
}
