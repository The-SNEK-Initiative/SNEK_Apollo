use crate::player::{Player, PlayerState};
use std::ffi::CStr;
use std::os::raw::c_char;

#[repr(C)]
pub struct SnekMediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub has_audio: bool,
}

#[repr(C)]
pub struct SnekVideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: *const u32,
    pub data_len: usize,
    pub timestamp_ms: u64,
}

#[no_mangle]
pub extern "C" fn snek_create() -> *mut Player {
    let player = Box::new(Player::new());
    Box::into_raw(player)
}

#[no_mangle]
pub extern "C" fn snek_destroy(ptr: *mut Player) {
    if !ptr.is_null() {
        unsafe { let _ = Box::from_raw(ptr); }
    }
}

#[no_mangle]
pub extern "C" fn snek_open(ptr: *mut Player, url: *const c_char, out_info: *mut SnekMediaInfo, hwnd: *mut std::ffi::c_void) -> bool {
    if ptr.is_null() || url.is_null() || out_info.is_null() { return false; }
    let player = unsafe { &mut *ptr };
    let c_str = unsafe { CStr::from_ptr(url) };
    let url_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let hwnd_opt = if hwnd.is_null() { None } else { Some(hwnd as isize) };

    match player.open(url_str, 0, hwnd_opt) {
        Ok(info) => {
            unsafe {
                (*out_info).width = info.width;
                (*out_info).height = info.height;
                (*out_info).duration_ms = info.duration_ms;
                (*out_info).has_audio = info.has_audio;
            }
            true
        }
        Err(_) => false,
    }
}

#[no_mangle]
pub extern "C" fn snek_play(ptr: *mut Player) {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.play();
    }
}

#[no_mangle]
pub extern "C" fn snek_pause(ptr: *mut Player) {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.pause();
    }
}

#[no_mangle]
pub extern "C" fn snek_stop(ptr: *mut Player) {
    if let Some(player) = unsafe { ptr.as_mut() } {
        player.stop();
    }
}

#[no_mangle]
pub extern "C" fn snek_terminate() {
    std::process::exit(0);
}

#[no_mangle]
pub extern "C" fn snek_seek(ptr: *mut Player, ms: u64) {
    if let Some(player) = unsafe { ptr.as_mut() } {
        player.seek_ms(ms);
    }
}

#[no_mangle]
pub extern "C" fn snek_seek_hls(ptr: *mut Player, ms: u64) -> bool {
    if let Some(player) = unsafe { ptr.as_mut() } {
        let start_sid = player.seek_id.load(std::sync::atomic::Ordering::SeqCst);
        player.seek_ms(ms);
        
        // Wait for the decode thread to pick up and process the seek command, this ensures the next snek_position_ms call doesn't see a "stale" 0 or old pos
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < std::time::Duration::from_millis(500) {
            let done_sid = player.seek_done_id.load(std::sync::atomic::Ordering::SeqCst);
            if done_sid > start_sid {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    } else {
        false
    }
}

#[no_mangle]
pub extern "C" fn snek_set_volume(ptr: *mut Player, volume: f32) {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.set_volume(volume);
    }
}

#[no_mangle]
pub extern "C" fn snek_set_mute(ptr: *mut Player, mute: bool) {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.set_mute(mute);
    }
}

#[no_mangle]
pub extern "C" fn snek_position_ms(ptr: *mut Player) -> u64 {
    if let Some(player) = unsafe { ptr.as_ref() } {
        let pos = player.position_ms();
        if pos == 0 && player.duration_ms() > 0 {
            // eprintln!("[ffi] snek_position_ms returned 0 for stream with duration {}", player.duration_ms());
        }
        pos
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn snek_state(ptr: *mut Player) -> u8 {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.state() as u8
    } else {
        PlayerState::Error as u8
    }
}

#[no_mangle]
pub extern "C" fn snek_next_frame(ptr: *mut Player, out_frame: *mut SnekVideoFrame) -> bool {
    let player = match unsafe { ptr.as_mut() } {
        Some(p) => p,
        None => return false,
    };

    if let Some(frame) = player.next_frame() {
        player.last_frame = Some(frame);
    }

    if let Some(ref frame) = player.last_frame {
        unsafe {
            (*out_frame).width = frame.width;
            (*out_frame).height = frame.height;
            (*out_frame).data = frame.data.as_ptr();
            (*out_frame).data_len = frame.data.len();
            (*out_frame).timestamp_ms = frame.timestamp_ms;
        }
        true
    } else {
        false
    }
}

#[no_mangle]
pub extern "C" fn snek_get_download_progress(ptr: *mut Player) -> f32 {
    if let Some(player) = unsafe { ptr.as_ref() } {
        player.get_download_progress()
    } else {
        0.0
    }
}

#[no_mangle]
pub extern "C" fn snek_cleanup() {
    crate::hls::cleanup_temp_files();
}
