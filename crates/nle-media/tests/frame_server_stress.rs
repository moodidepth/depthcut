//! Rapid-scrubbing and teardown regression tests for the frame server.
//! Each scenario runs on its own thread with a watchdog, so a deadlock
//! fails the test instead of hanging the suite.

use nle_core::{MediaAsset, Time};
use nle_media::probe::synthetic_asset;
use nle_media::FrameServer;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn with_watchdog<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs)).expect("frame server deadlocked or hung")
}

#[test]
fn rapid_scrub_never_deadlocks_and_queue_stays_bounded() {
    with_watchdog(60, || {
        const CAPACITY: usize = 8;
        let assets: Vec<MediaAsset> = (0..3).map(|i| synthetic_asset(&format!("scrub-{i}"), 30, i as f32 * 0.3)).collect();
        let server = FrameServer::new(CAPACITY, Some(180));
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut delivered = 0usize;
        for i in 0..6_000u32 {
            // xorshift: jump around the whole clip like a frantic scrub
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let asset = &assets[(seed % 3) as usize];
            let t = Time::from_millis((seed >> 8) as i64 % 30_000);
            server.request(asset, t, false, i % 5 == 0);
            if i % 16 == 0 {
                let frames = server.drain();
                assert!(frames.len() <= CAPACITY, "queue grew to {}", frames.len());
                delivered += frames.len();
            }
            if i == 3_000 {
                // removing an asset mid-scrub joins its worker cleanly
                server.forget(assets[2].id);
            }
        }
        // the last request per asset is always honoured ("latest wins")
        let deadline = Instant::now() + Duration::from_secs(20);
        while delivered == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            delivered += server.drain().len();
        }
        assert!(delivered > 0, "no frame was ever delivered");
        drop(server); // joins every worker thread
    });
}

#[test]
fn drop_joins_workers_that_are_idle_prefetching_or_failing() {
    with_watchdog(60, || {
        let good = synthetic_asset("teardown", 10, 0.5);
        // A real-file asset whose media is missing: the decoder can never open.
        let mut broken = good.clone();
        broken.id = nle_core::AssetId::new();
        broken.kind = nle_core::AssetKind::Video;
        broken.path = std::env::temp_dir().join("rcp-missing-media-does-not-exist.mp4");

        for _ in 0..25 {
            let server = FrameServer::new(4, None);
            server.request(&good, Time::from_secs(1), false, true);
            server.request(&broken, Time::from_secs(1), false, false);
            // no frame for the broken asset, and no panic either
            assert!(server.frame_sync(&broken, Time::ZERO, false).is_err());
            drop(server);
        }
        // teardown while the failing worker sits in its retry back-off
        let server = FrameServer::new(4, None);
        server.request(&broken, Time::ZERO, false, false);
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(5), "teardown waited on a failing decoder");
    });
}
