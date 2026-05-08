use snek_apollo::{Player, PlayerState};
use std::env;
use std::time::Duration;
use std::thread;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: snek_apollo <URL>");
        return;
    }
    let url = &args[1];

    println!("[snek_apollo] Opening: {}", url);
    let mut player = Player::new();
    
    match player.open(url, 0, None) {
        Ok(info) => {
            let dur_s = info.duration_ms / 1000;
            let h = dur_s / 3600;
            let m = (dur_s % 3600) / 60;
            let s = dur_s % 60;
            let dur_str = if h > 0 {
                format!("{:02}:{:02}:{:02}", h, m, s)
            } else {
                format!("{:02}:{:02}", m, s)
            };

            println!("[snek_apollo] Video: {}x{} | Duration: {} | Audio: {}", 
                info.width, info.height, dur_str, if info.has_audio { "yes" } else { "no" });
            
            player.play();

            while player.state() != PlayerState::EndOfStream && player.state() != PlayerState::Error {
                // Consume frames to prevent the decoder from blocking
                while let Some(_frame) = player.next_frame() {
                    // We don't have a window in this CLI tool, but consuming the frame allows the decode thread to continue and print stride logs
                }
                thread::sleep(Duration::from_millis(10));
            }
            println!("[snek_apollo] Done.");
        }
        Err(e) => {
            println!("[snek_apollo] Error: {}", e);
        }
    }
}
