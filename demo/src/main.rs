//! Every prow hook, in the shape of a serving loop: a prompt in steps of 512, then decode
//! steps, with zones on this thread, card work on a lane of its own, a plot and messages.
//!
//! `PROW_SERVER=host:port cargo run -p prow-demo [steps]`. Build with
//! `--no-default-features` for the baseline with no tracking compiled in.

use std::time::Duration;

use prow::Timestamp;

fn main() {
    let steps: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    prow::Run::new("demo").attrs(prow::build_attrs!()).attr("workload", "demo-serving").attr("steps", steps as u64).start();

    prow::message!("track-store", "starting");
    let prompt = 1100u32;
    let mut pos = 0u32;
    while pos < prompt {
        let width = (prompt - pos).min(512);
        step("prefill", width, pos);
        pos += width;
    }
    for _ in 0..steps {
        step("decode", 1, pos);
        pos += 1;
    }
    prow::plot!("track-store", "store.positions", pos);
    prow::message!("track-store", format!("done at position {pos}"));
}

fn step(kind: &'static str, width: u32, pos: u32) {
    prow::zone!("track-zones", "step");
    let t0 = Timestamp::now();
    {
        prow::zone!("track-zones", "attention");
        std::thread::sleep(Duration::from_micros(200 + width as u64));
    }
    // the card's share, as a span timed elsewhere: here, a slice of the step on lane "card0"
    let card_start = Timestamp::now();
    std::thread::sleep(Duration::from_micros(150 + 2 * width as u64));
    prow::zone_at!("track-zones", "experts", "card0", card_start, Timestamp::now());
    let ms = (Timestamp::now().unix_nanos() - t0.unix_nanos()) as f64 / 1e6;
    prow::row!("track-step", "step", {
        "kind" => kind,
        "width" => width,
        "pos" => pos,
        "ms" => ms,
        "resident" => 0.42f32,
        "verified" => kind == "decode",
    });
    prow::plot!("track-store", "store.bytes", 19_000.0 * pos as f64);
}
