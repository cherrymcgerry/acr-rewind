//! Prints Assetto Corsa Rally shared memory at 10 Hz to verify the field layout.
//!
//! Usage: `acr-shm-dump [--once] [--static]`

use std::thread::sleep;
use std::time::{Duration, Instant};

use acr_shm::{SharedMemory, ShmError, Snapshot};

const INTERVAL: Duration = Duration::from_millis(100);
const RETRY: Duration = Duration::from_secs(2);
/// Physics packet id unchanged for this long while not paused means the game has gone away.
const STALE_AFTER: Duration = Duration::from_secs(3);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("acr-shm-dump [--once] [--static]\n  --once    print one sample and exit\n  --static  print the static page on connect");
        return;
    }
    let once = args.iter().any(|a| a == "--once");
    let show_static = once || args.iter().any(|a| a == "--static");

    loop {
        let shm = match SharedMemory::open() {
            Ok(shm) => shm,
            Err(ShmError::NotRunning(page)) => {
                if once {
                    eprintln!("{page} not found: Assetto Corsa Rally is not running.");
                    std::process::exit(2);
                }
                eprintln!("Waiting for Assetto Corsa Rally ({page} not found)...");
                sleep(RETRY);
                continue;
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("Connected to shared memory.");
        if show_static {
            print_static(&shm.snapshot());
        }
        if once {
            print_line(&shm.snapshot());
            return;
        }
        run(&shm);
        eprintln!("Shared memory went stale (game closed?). Reconnecting...");
    }
}

fn run(shm: &SharedMemory) {
    let mut last_packet = None;
    let mut last_change = Instant::now();
    loop {
        let snap = shm.snapshot();
        let packet = snap.physics.packet_id;
        if last_packet != Some(packet) {
            last_packet = Some(packet);
            last_change = Instant::now();
        } else {
            let idle = last_change.elapsed();
            let stale = (snap.is_live() && idle > STALE_AFTER)
                || (snap.status() == acr_shm::Status::Off && idle > STALE_AFTER * 4);
            if stale {
                return;
            }
        }
        print_line(&snap);
        sleep(INTERVAL);
    }
}

fn print_line(s: &Snapshot) {
    let p = &s.physics;
    let g = &s.graphics;
    let w = &p.wheels;
    let pos = g.player_position.map_or_else(|| "-".to_string(), |v| format!("{:8.1} {:8.1} {:8.1}", v[0], v[1], v[2]));
    println!(
        "{:<7} pkt {:>8} | {:6.1} km/h {:>5} rpm g{:>2} | thr {:.2} brk {:.2} clu {:.2} str {:+.3} | \
         vel {:+6.1} {:+6.1} {:+6.1} | hpr {:+.2} {:+.2} {:+.2} | whl {:6.1} {:6.1} {:6.1} {:6.1} rad/s | \
         tyre {:5.1} {:5.1} {:5.1} {:5.1} C | pos {}",
        s.status().to_string(),
        p.packet_id,
        p.speed_kmh,
        p.rpm,
        p.gear,
        p.gas,
        p.brake,
        p.clutch,
        p.steer_angle,
        p.velocity[0],
        p.velocity[1],
        p.velocity[2],
        p.heading,
        p.pitch,
        p.roll,
        w[0].angular_speed,
        w[1].angular_speed,
        w[2].angular_speed,
        w[3].angular_speed,
        w[0].core_temp_c,
        w[1].core_temp_c,
        w[2].core_temp_c,
        w[3].core_temp_c,
        pos,
    );
}

fn print_static(s: &Snapshot) {
    let st = &s.static_info;
    if !st.is_populated() {
        println!("static: (not populated yet; load a stage)");
        return;
    }
    println!(
        "static: sm {} ac {} | car {} | track {} {} | max rpm {} fuel {:.1} | online {}",
        st.sm_version,
        st.ac_version,
        st.car_model,
        st.track,
        st.track_configuration,
        st.max_rpm,
        st.max_fuel,
        st.is_online
    );
}
