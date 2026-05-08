use std::sync::{Arc, atomic::{AtomicU8, AtomicI64, AtomicU64, Ordering}, mpsc};
use std::thread;
#[cfg(windows)]
use std::time::{Instant, Duration};

use crate::hls::StreamBuffer;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use windows::Win32::System::Com::*;
#[cfg(windows)]
use windows::core::PROPVARIANT;
#[cfg(windows)]
use windows::Win32::Media::MediaFoundation::*;
#[cfg(windows)]
use windows::Win32::Media::Audio::*;
#[cfg(windows)]
use windows::core::{HSTRING, GUID, IUnknown, w, Interface};
#[cfg(windows)]
use crate::hls::{spawn_hls_downloader, prefetch_hls};
#[cfg(windows)]
use crate::mf_byte_stream::SnekByteStream;
#[cfg(windows)]
use windows::Win32::Graphics::Direct2D::*;
#[cfg(windows)]
use windows::Win32::Graphics::Direct2D::Common::*;
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::*;
#[cfg(windows)]
use windows::Win32::Foundation::HWND;

// ── Public Types ──

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u32>,
    pub timestamp_ms: u64,
}

pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub has_audio: bool,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerState {
    Idle = 0, Playing = 1, Paused = 2, Stopped = 3, EndOfStream = 4, Error = 5,
}
impl From<u8> for PlayerState {
    fn from(v: u8) -> Self {
        match v { 1=>Self::Playing, 2=>Self::Paused, 3=>Self::Stopped,
                   4=>Self::EndOfStream, 5=>Self::Error, _=>Self::Idle }
    }
}

pub enum Cmd { Play, Pause, Stop, Seek(u64), SetVolume(f32), SetMute(bool) }

// ── Player ──

pub struct Player {
    pub state: Arc<AtomicU8>,
    pub position_100ns: Arc<AtomicI64>,
    pub seek_id: Arc<AtomicI64>,
    pub seek_done_id: Arc<AtomicI64>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub frame_rx: Option<mpsc::Receiver<VideoFrame>>,
    pub cmd_tx: Option<mpsc::Sender<Cmd>>,
    pub thread: Option<thread::JoinHandle<()>>,
    pub last_frame: Option<VideoFrame>,
    pub current_url: String,
    pub is_hls: bool,
    pub current_hwnd: Option<isize>,
    pub stream_buffer_ref: Option<Arc<std::sync::Mutex<Option<Arc<StreamBuffer>>>>>,
    pub download_progress: Arc<AtomicU64>,
    pub download_total: Arc<AtomicU64>,
}

impl Player {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(0)),
            position_100ns: Arc::new(AtomicI64::new(0)),
            seek_id: Arc::new(AtomicI64::new(0)),
            seek_done_id: Arc::new(AtomicI64::new(0)),
            duration_ms: 0, width: 0, height: 0,
            frame_rx: None, cmd_tx: None, thread: None,
            last_frame: None,
            current_url: String::new(),
            is_hls: false,
            current_hwnd: None,
            stream_buffer_ref: None,
            download_progress: Arc::new(AtomicU64::new(0)),
            download_total: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn open(&mut self, source: &str, start_ms: u64, hwnd: Option<isize>) -> Result<MediaInfo, String> {
        self.stop();

        if let Some(buf_mutex) = &self.stream_buffer_ref {
            if let Ok(guard) = buf_mutex.lock() {
                if let Some(buf) = guard.as_ref() {
                    buf.cancelled.store(true, Ordering::SeqCst);
                }
            }
        }
        
        self.position_100ns.store((start_ms as i64) * 10_000, Ordering::SeqCst);
        self.current_url = source.to_string();
        self.is_hls = source.contains(".m3u8");
        self.current_hwnd = hwnd;
        
        let (info_tx, info_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::sync_channel(8); // Increased frame buffer
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let state = self.state.clone();
        let position = self.position_100ns.clone();
        #[cfg(windows)]
        let seek_id_shared = self.seek_id.clone();
        #[cfg(windows)]
        let seek_done_id_shared = self.seek_done_id.clone();
        let url = source.to_string();
        let buf_ref = Arc::new(std::sync::Mutex::new(None));
        self.stream_buffer_ref = Some(buf_ref.clone());
        state.store(PlayerState::Paused as u8, Ordering::SeqCst);

        #[cfg(windows)]
        let download_progress = self.download_progress.clone();
        #[cfg(windows)]
        let download_total = self.download_total.clone();

        self.thread = Some(thread::spawn(move || {
            #[cfg(windows)]
            decode_thread_win(url, info_tx, frame_tx, cmd_rx, state, position, seek_id_shared, seek_done_id_shared, start_ms, hwnd, buf_ref, download_progress, download_total);
            #[cfg(target_os = "linux")]
            decode_thread_linux(url, info_tx, frame_tx, cmd_rx, state, position, start_ms, hwnd, buf_ref);
            #[cfg(all(not(windows), not(target_os = "linux")))]
            { let _ = info_tx.send(Err("Platform not supported".into())); }
        }));

        let info = info_rx.recv().map_err(|e| format!("{}", e))??;
        self.width = info.width;
        self.height = info.height;
        self.duration_ms = info.duration_ms;
        self.frame_rx = Some(frame_rx);
        self.cmd_tx = Some(cmd_tx);
        Ok(info)
    }

    pub fn play(&self)  { if let Some(ref tx) = self.cmd_tx { let _ = tx.send(Cmd::Play); } }
    pub fn pause(&self) { if let Some(ref tx) = self.cmd_tx { let _ = tx.send(Cmd::Pause); } }
    pub fn seek_ms(&self, ms: u64) { 
        if let Some(ref tx) = self.cmd_tx { 
            let _ = tx.send(Cmd::Seek(ms)); 
            // Update position_100ns immediately so position_ms() returns the new target
            // before the decode thread even processes the command. This prevents
            // UI flicker/reset during the transition.
            self.position_100ns.store((ms as i64) * 10_000, Ordering::SeqCst);
        } 
    }
    pub fn set_volume(&self, volume: f32) { if let Some(ref tx) = self.cmd_tx { let _ = tx.send(Cmd::SetVolume(volume)); } }
    pub fn set_mute(&self, mute: bool) { if let Some(ref tx) = self.cmd_tx { let _ = tx.send(Cmd::SetMute(mute)); } }

    pub fn stop(&mut self) {
        // Signal stop immediately
        self.state.store(PlayerState::Stopped as u8, Ordering::SeqCst);
        
        // Cancel any streaming downloads
        if let Some(buf_mutex) = &self.stream_buffer_ref {
            if let Ok(guard) = buf_mutex.lock() {
                if let Some(buf) = guard.as_ref() {
                    buf.cancelled.store(true, Ordering::SeqCst);
                    buf.set_eos();
                }
            }
        }
        
        // Send stop command through channel (unblocks try_recv loops)
        if let Some(ref tx) = self.cmd_tx {
            let _ = tx.send(Cmd::Stop);
        }
        // Drop the sender so any blocking recv in the thread will return Err
        self.cmd_tx = None;
        
        // Wait for thread with a timeout - don't hang forever
        if let Some(t) = self.thread.take() {
            // Give it 500ms to exit gracefully, then abandon
            let start = std::time::Instant::now();
            loop {
                if t.is_finished() {
                    let _ = t.join();
                    break;
                }
                if start.elapsed() > std::time::Duration::from_millis(500) {
                    eprintln!("[snek_apollo] Stop: thread did not exit in 500ms, abandoning");
                    // Don't join - let the thread die on its own
                    std::mem::forget(t);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        
        // Clean up temp files
        crate::hls::cleanup_temp_files();
    }

    pub fn next_frame(&self) -> Option<VideoFrame> { self.frame_rx.as_ref()?.try_recv().ok() }
    pub fn state(&self) -> PlayerState { PlayerState::from(self.state.load(Ordering::SeqCst)) }
    pub fn get_download_progress(&self) -> f32 {
        let total = self.download_total.load(Ordering::SeqCst);
        if total == 0 { return 0.0; }
        let done = self.download_progress.load(Ordering::SeqCst);
        (done as f32 / total as f32).min(1.0)
    }
    pub fn position_ms(&self) -> u64 { 
        let raw = self.position_100ns.load(Ordering::SeqCst);
        let ms = (raw / 10_000).max(0) as u64;
        if ms == 0 && self.duration_ms > 0 {
            // eprintln!("[snek_apollo] position_ms returning 0, raw is {}", raw);
        }
        ms
    }
    pub fn duration_ms(&self) -> u64 { self.duration_ms }
    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
}
impl Drop for Player { fn drop(&mut self) { self.stop(); } }

// WASAPI audio thread

// COM objects are safe to send across threads in MTA mode
#[cfg(windows)]
struct SendCom<T>(T);
#[cfg(windows)]
unsafe impl<T> Send for SendCom<T> {}

#[cfg(windows)]
enum AudioCmd {
    Data(Vec<u8>),
    Flush,
}

#[cfg(windows)]
fn audio_thread_win(
    rx: mpsc::Receiver<AudioCmd>,
    client: SendCom<IAudioClient>,
    render: SendCom<IAudioRenderClient>,
    buf_frames: u32,
    bytes_per_frame: usize,
) {
    let client = client.0;
    let render = render.0;
    unsafe {
        let _ = client.Start();
        let mut pending: Vec<u8> = Vec::with_capacity(65536);

        loop {
            loop {
                match rx.try_recv() {
                    Ok(AudioCmd::Data(d)) => pending.extend(d),
                    Ok(AudioCmd::Flush) => {
                        pending.clear();
                        let _ = client.Stop();
                        let _ = client.Reset();
                        let _ = client.Start();
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let _ = client.Stop();
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }

            let padding = client.GetCurrentPadding().unwrap_or(0);
            let avail = (buf_frames - padding) as usize;
            let frames_ready = if bytes_per_frame > 0 { pending.len() / bytes_per_frame } else { 0 };
            let to_write = avail.min(frames_ready);

            if to_write > 0 {
                if let Ok(buf_ptr) = render.GetBuffer(to_write as u32) {
                    let bytes = to_write * bytes_per_frame;
                    std::ptr::copy_nonoverlapping(pending.as_ptr(), buf_ptr, bytes);
                    let _ = render.ReleaseBuffer(to_write as u32, 0);
                    pending.drain(..bytes);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

// Seek helper: raw PROPVARIANT with VT_I8

#[cfg(windows)]
#[repr(C)]
struct PropVarI8 {
    vt: u16,
    pad: [u16; 3],
    val: i64,
}

#[cfg(windows)]
fn seek_reader(reader: &IMFSourceReader, position_100ns: i64) -> bool {
    unsafe {
        let pv = PropVarI8 { vt: 20, pad: [0; 3], val: position_100ns }; // VT_I8 = 20
        let guid_null = GUID::zeroed();
        reader.SetCurrentPosition(
            &guid_null as *const _,
            &pv as *const _ as *const _,
        ).is_ok()
    }
}

// Windows MF decode thread

#[cfg(windows)]
fn decode_thread_win(
    url: String,
    info_tx: mpsc::Sender<Result<MediaInfo, String>>,
    frame_tx: mpsc::SyncSender<VideoFrame>,
    cmd_rx: mpsc::Receiver<Cmd>,
    state: Arc<AtomicU8>,
    position: Arc<AtomicI64>,
    seek_id: Arc<AtomicI64>,
    seek_done_id: Arc<AtomicI64>,
    start_ms: u64,
    hwnd: Option<isize>,
    buf_ref: Arc<std::sync::Mutex<Option<Arc<StreamBuffer>>>>,
    download_progress: Arc<AtomicU64>,
    download_total: Arc<AtomicU64>,
) {
    let _is_hls_url = url.contains(".m3u8");
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = info_tx.send(Err(format!("COM: {:?}", e))); return;
        }
        if let Err(e) = MFStartup(MF_VERSION, 0) {
            let _ = info_tx.send(Err(format!("MF: {:?}", e))); return;
        }

        let mut attrs_ptr: Option<IMFAttributes> = None;
        let _ = MFCreateAttributes(&mut attrs_ptr as *mut _, 1);
        if let Some(ref a) = attrs_ptr {
            let _ = a.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
        }

        //  create a source reader from a byte stream buffer
        fn resolve_buffer(
            buffer: &Arc<StreamBuffer>,
            attrs_ptr: Option<&IMFAttributes>,
        ) -> Result<IMFSourceReader, String> {
            unsafe {
                let byte_stream: IMFByteStream = SnekByteStream::new(buffer.clone()).into();

                // Set MIME type based on content
                if let Ok(bs_attrs) = byte_stream.cast::<IMFAttributes>() {
                    let mime = if buffer.is_ts() { w!("video/mp2t") } else { w!("video/mp4") };
                    let _ = bs_attrs.SetString(&MF_BYTESTREAM_CONTENT_TYPE, mime);
                }

                let resolver = MFCreateSourceResolver()
                    .map_err(|e| format!("MFCreateSourceResolver: {:?}", e))?;

                let mut obj_type = MF_OBJECT_TYPE::default();
                let mut source: Option<IUnknown> = None;

                // .ts for MPEG-TS, .mp4 for fMP4/MP4
                let hint_ts = [w!("dummy.ts"), windows::core::PCWSTR::null()];
                let hint_mp4 = [w!("dummy.mp4"), windows::core::PCWSTR::null()];
                let hints: &[windows::core::PCWSTR] = if buffer.is_ts() { &hint_ts } else { &hint_mp4 };

                let flags_list = [
                    MF_RESOLUTION_MEDIASOURCE.0 as u32,
                    MF_RESOLUTION_MEDIASOURCE.0 as u32 | 0x00000040,
                ];

                let mut last_error = None;
                'resolve: for flags in flags_list {
                    for h in hints {
                        match resolver.CreateObjectFromByteStream(
                            &byte_stream, *h, flags, None, &mut obj_type, &mut source,
                        ) {
                            Ok(_) => { last_error = None; break 'resolve; }
                            Err(e) => { last_error = Some(e); }
                        }
                    }
                }

                if let Some(e) = last_error {
                    return Err(format!("Source resolver failed: {:?}", e));
                }

                let source = source.ok_or("Source was None")?;
                MFCreateSourceReaderFromMediaSource(
                    &source.cast::<IMFMediaSource>().map_err(|e| format!("{:?}", e))?,
                    attrs_ptr,
                ).map_err(|e| format!("Open MediaSource: {:?}", e))
            }
        }

        // Audio reader for separate HLS audio rendition
        let mut audio_reader_ext: Option<IMFSourceReader> = None;
        let mut audio_idx_ext: Option<u32> = None;
        let mut hls_duration_ms: u64 = 0;
        let mut hls_video_buffer: Option<Arc<StreamBuffer>> = None;
        let mut hls_audio_buffer: Option<Arc<StreamBuffer>> = None;

        let reader = if url.starts_with("http") {
            if url.contains(".m3u8") {
                // HLS try prefetch (download-then-play)
                match prefetch_hls(&url, download_progress, download_total) {
                    Ok(result) if !result.is_live => {
                        // VOD HLS: all segments are downloaded to a local temp file
                        eprintln!("[snek_apollo] HLS prefetch complete, creating source reader");
                        hls_duration_ms = result.duration_ms;
                        
                        // Create video reader from local file
                        let path_w = HSTRING::from(result.video_file);
                        let vreader = match MFCreateSourceReaderFromURL(&path_w, attrs_ptr.as_ref()) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("[snek_apollo] Failed to open prefetched video: {:?}", e);
                                let _ = info_tx.send(Err(format!("{:?}", e))); return;
                            }
                        };
                        
                        // Create audio reader from local file if separate rendition
                        if let Some(ref audio_file) = result.audio_file {
                            let apath_w = HSTRING::from(audio_file);
                            match MFCreateSourceReaderFromURL(&apath_w, attrs_ptr.as_ref()) {
                                Ok(ar) => {
                                    for i in 0..16u32 {
                                        match ar.GetNativeMediaType(i, 0) {
                                            Ok(mt) => {
                                                if let Ok(major) = mt.GetGUID(&MF_MT_MAJOR_TYPE) {
                                                    if major == MFMediaType_Audio {
                                                        audio_idx_ext = Some(i);
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    if audio_idx_ext.is_some() {
                                        audio_reader_ext = Some(ar);
                                        eprintln!("[snek_apollo] Separate audio reader from prefetch: OK");
                                    }
                                }
                                Err(e) => eprintln!("[snek_apollo] Audio reader failed (non-fatal): {:?}", e),
                            }
                        }
                        
                        // NOTE: hls_video_buffer stays None!
                        // This means the decode loop will use the normal (non-HLS) path for
                        // seeking, position tracking, and timing. MF handles it all natively
                        // because it has the complete file.
                        
                        Some(vreader)
                    }
                    Ok(_) => {
                        // Live HLS or smth else that didn't provide a result
                        eprintln!("[snek_apollo] Using streaming mode (live stream detected)");
                        let buffer = StreamBuffer::new();
                        hls_video_buffer = Some(buffer.clone());
                        if let Ok(mut guard) = buf_ref.lock() {
                            *guard = Some(buffer.clone());
                        }
                        hls_audio_buffer = spawn_hls_downloader(url.clone(), buffer.clone(), start_ms);
                        
                        while buffer.total_written() < 256 * 1024 && !buffer.is_eos() {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        hls_duration_ms = buffer.get_duration_ms();
                        
                        if buffer.has_error() || buffer.total_written() == 0 {
                            let _ = info_tx.send(Err("HLS streaming failed".into())); return;
                        }
                        
                        let vr = match resolve_buffer(&buffer, attrs_ptr.as_ref()) {
                            Ok(r) => r,
                            Err(e) => { let _ = info_tx.send(Err(e)); return; }
                        };
                        
                        if let Some(ref abuf) = hls_audio_buffer {
                            while abuf.total_written() < 64 * 1024 && !abuf.is_eos() {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            if abuf.total_written() > 0 && !abuf.has_error() {
                                if let Ok(ar) = resolve_buffer(abuf, attrs_ptr.as_ref()) {
                                    for i in 0..16u32 {
                                        if let Ok(mt) = ar.GetNativeMediaType(i, 0) {
                                            if mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default() == MFMediaType_Audio {
                                                audio_idx_ext = Some(i);
                                                audio_reader_ext = Some(ar);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(vr)
                    }
                    Err(e) => {
                        // Prefetch failed > fall back to streaming mode
                        eprintln!("[snek_apollo] Prefetch failed: {}, falling back to streaming mode", e);
                        let buffer = StreamBuffer::new();
                        hls_video_buffer = Some(buffer.clone());
                        if let Ok(mut guard) = buf_ref.lock() {
                            *guard = Some(buffer.clone());
                        }
                        hls_audio_buffer = spawn_hls_downloader(url.clone(), buffer.clone(), start_ms);
                        
                        while buffer.total_written() < 256 * 1024 && !buffer.is_eos() {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        hls_duration_ms = buffer.get_duration_ms();
                        
                        if buffer.has_error() || buffer.total_written() == 0 {
                            let _ = info_tx.send(Err("HLS streaming failed".into())); return;
                        }
                        
                        let vr = match resolve_buffer(&buffer, attrs_ptr.as_ref()) {
                            Ok(r) => r,
                            Err(e) => { let _ = info_tx.send(Err(e)); return; }
                        };
                        
                        if let Some(ref abuf) = hls_audio_buffer {
                            while abuf.total_written() < 64 * 1024 && !abuf.is_eos() {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            if abuf.total_written() > 0 && !abuf.has_error() {
                                if let Ok(ar) = resolve_buffer(abuf, attrs_ptr.as_ref()) {
                                    for i in 0..16u32 {
                                        if let Ok(mt) = ar.GetNativeMediaType(i, 0) {
                                            if mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default() == MFMediaType_Audio {
                                                audio_idx_ext = Some(i);
                                                audio_reader_ext = Some(ar);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(vr)
                    }
                }
            } else {
                // Direct HTTP file (not HLS)
                let buffer = StreamBuffer::new();
                hls_video_buffer = Some(buffer.clone());
                if let Ok(mut guard) = buf_ref.lock() {
                    *guard = Some(buffer.clone());
                }
                spawn_hls_downloader(url.clone(), buffer.clone(), start_ms);
                
                while buffer.total_written() < 256 * 1024 && !buffer.is_eos() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                
                if buffer.has_error() {
                    // Fallback to native MF reader
                    let mut attrs: Option<IMFAttributes> = None;
                    let _ = MFCreateAttributes(&mut attrs, 2);
                    if let Some(ref a) = attrs {
                        let _ = a.SetString(&GUID::from_u128(0xad35d09b_640b_4903_bc82_01397444c12b), &HSTRING::from("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"));
                        if url.contains("gelbooru.com") {
                            let _ = a.SetString(&GUID::from_u128(0x521fb371_2ee8_4ee8_b205_318ce01bc89a), &HSTRING::from("https://gelbooru.com/"));
                        } else if url.contains("donmai.us") {
                            let _ = a.SetString(&GUID::from_u128(0x521fb371_2ee8_4ee8_b205_318ce01bc89a), &HSTRING::from("https://danbooru.donmai.us/"));
                        }
                    }
                    MFCreateSourceReaderFromURL(&HSTRING::from(&url), attrs.as_ref()).ok()
                } else if buffer.total_written() == 0 {
                    let _ = info_tx.send(Err("Download produced 0 bytes".into())); return;
                } else {
                    match resolve_buffer(&buffer, attrs_ptr.as_ref()) {
                        Ok(r) => Some(r),
                        Err(e) => { let _ = info_tx.send(Err(e)); return; }
                    }
                }
            }
        } else {
            // Local file
            match MFCreateSourceReaderFromURL(&HSTRING::from(&url), attrs_ptr.as_ref()) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("[snek_apollo] Failed to open local file reader: {:?}", e);
                    None
                }
            }
        };

        let reader = match reader {
            Some(r) => r,
            None => {
                let err = "Failed to open media source".to_string();
                eprintln!("[snek_apollo] {}", err);
                let _ = info_tx.send(Err(err)); return;
            }
        };

        // Discover streams
        let mut video_idx: Option<u32> = None;
        let mut audio_idx: Option<u32> = None;
        for i in 0..16u32 {
            match reader.GetNativeMediaType(i, 0) {
                Ok(mt) => {
                    if let Ok(major) = mt.GetGUID(&MF_MT_MAJOR_TYPE) {
                        if major == MFMediaType_Video && video_idx.is_none() { video_idx = Some(i); }
                        else if major == MFMediaType_Audio && audio_idx.is_none() { audio_idx = Some(i); }
                    }
                }
                Err(_) => break,
            }
        }

        let video_idx = match video_idx {
            Some(i) => i,
            None => { let _ = info_tx.send(Err("No video stream".into())); return; }
        };

        // Configure video
        let formats = [MFVideoFormat_RGB32, MFVideoFormat_NV12];
        let mut is_nv12 = false;
        let mut video_ok = false;
        for fmt in &formats {
            let mt = MFCreateMediaType().unwrap();
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            mt.SetGUID(&MF_MT_SUBTYPE, fmt).unwrap();
            if reader.SetCurrentMediaType(video_idx, None, &mt).is_ok() {
                is_nv12 = *fmt == MFVideoFormat_NV12;
                video_ok = true;
                break;
            }
        }
        if !video_ok { let _ = info_tx.send(Err("No supported video format".into())); return; }

        let actual = reader.GetCurrentMediaType(video_idx).unwrap();
        let size = actual.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let width = (size >> 32) as u32;
        let height = (size & 0xFFFFFFFF) as u32;
        if width == 0 || height == 0 { let _ = info_tx.send(Err("Bad dims".into())); return; }

        let mut media_stride = actual.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(0) as i32;
        
        let aperture = actual.GetBlobSize(&MF_MT_MINIMUM_DISPLAY_APERTURE).ok().and_then(|_| {
            let mut blob = vec![0u8; 16];
            let mut size = 0u32;
            if unsafe { actual.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, Some(&mut size)).is_ok() } {
                // MFVideoArea struct: OffsetX, OffsetY (both 16.16 fixed), Area (WidthxHeight)
                let area_w = u32::from_le_bytes(blob[8..12].try_into().unwrap());
                let area_h = u32::from_le_bytes(blob[12..16].try_into().unwrap());
                Some((area_w, area_h))
            } else { None }
        });

        eprintln!("[snek_apollo] Video: {}x{} | Stride: {} | NV12: {} | Aperture: {:?}", width, height, media_stride, is_nv12, aperture);

        if media_stride == 0 {
            // fallback for common alignments
            media_stride = if is_nv12 {
                (width as i32 + 15) & !15
            } else {
                (width as i32 * 4 + 15) & !15
            };
            eprintln!("[snek_apollo] Stride missing, using calculated: {}", media_stride);
        }

        let effective_stride = media_stride.abs() as usize;

        // Get duration (100ns units > ms)
        let mut duration_ms = reader.GetPresentationAttribute(
            MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION
        ).ok().and_then(|pv| {
            // PROPVARIANT with VT_UI8 or VT_I8, extract as i64
            let raw: &PropVarI8 = &*(&pv as *const _ as *const PropVarI8);
            Some((raw.val / 10_000) as u64)
        }).unwrap_or(0);

        // For VOD HLS streams, MF might incorrectly calculate duration based only on the first downloaded segment.
        if hls_duration_ms > 0 {
            duration_ms = hls_duration_ms;
            eprintln!("[snek_apollo] Overriding MF duration with HLS parsed duration: {} ms", duration_ms);
        }

        // Configure audio using WASAPI mix format
        let mut audio_vol: Option<ISimpleAudioVolume> = None;

        // Determine which reader and stream index to use for audio
        let (audio_source_reader, effective_audio_idx) = if audio_idx.is_some() {
            (None, audio_idx)
        } else if let (Some(ref ext_reader), Some(ext_idx)) = (&audio_reader_ext, audio_idx_ext) {
            // Separate audio rendition
            (Some(ext_reader.clone()), Some(ext_idx))
        } else {
            (None, None)
        };
        let use_external_audio = audio_idx.is_none() && audio_reader_ext.is_some();

        let audio_tx: Option<mpsc::Sender<AudioCmd>> = (|| -> Option<mpsc::Sender<AudioCmd>> {
            let aidx = effective_audio_idx?;
            let the_reader = if use_external_audio {
                audio_source_reader.as_ref()?
            } else {
                &reader
            };

            // Get WASAPI mix format
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            let mix_fmt_ptr = client.GetMixFormat().ok()?;
            let mix_fmt = &*mix_fmt_ptr;

            let sr = mix_fmt.nSamplesPerSec;
            let ch = mix_fmt.nChannels;
            let bits = mix_fmt.wBitsPerSample;
            let is_float = mix_fmt.wFormatTag == 3 || // WAVE_FORMAT_IEEE_FLOAT
                (mix_fmt.wFormatTag == 0xFFFE && bits == 32); // EXTENSIBLE, assume float

            // Configure MF audio to match WASAPI mix format
            let amt = MFCreateMediaType().ok()?;
            amt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
            if is_float {
                amt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).ok()?;
            } else {
                amt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).ok()?;
            }
            amt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits as u32).ok()?;
            amt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch as u32).ok()?;
            amt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sr).ok()?;
            let block_align = ch * bits / 8;
            amt.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align as u32).ok()?;
            amt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sr * block_align as u32).ok()?;

            if the_reader.SetCurrentMediaType(aidx, None, &amt).is_err() {
                eprintln!("[snek_apollo] Audio format negotiation failed");
                return None;
            }

            // Initialize WASAPI with mix format
            if client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, mix_fmt_ptr, None).is_err() {
                eprintln!("[snek_apollo] WASAPI init failed");
                return None;
            }

            let render_client: IAudioRenderClient = client.GetService().ok()?;
            if let Ok(vol) = client.GetService() { audio_vol = Some(vol); }

            let buf_frames = client.GetBufferSize().unwrap_or(4096);
            let bpf = block_align as usize;

            let (atx, arx) = mpsc::channel::<AudioCmd>();
            let sc = SendCom(client);
            let sr = SendCom(render_client);
            thread::spawn(move || audio_thread_win(arx, sc, sr, buf_frames, bpf));
            Some(atx)
        })();

        let has_audio = audio_tx.is_some();
        if has_audio { eprintln!("[snek_apollo] Audio: OK"); }
        else { eprintln!("[snek_apollo] Audio: disabled"); }

        // If using external audio reader, spawn a thread to pump audio samples
        let ext_audio_tx = if use_external_audio && audio_tx.is_some() {
            let atx = audio_tx.as_ref().unwrap().clone();
            let ext_reader_raw = audio_reader_ext.take().unwrap();
            // Transmute the COM pointer to a raw usize to send across threads
            let reader_ptr: usize = std::mem::transmute_copy(&ext_reader_raw);
            std::mem::forget(ext_reader_raw); // prevent Drop on this thread
            let ext_idx = audio_idx_ext.unwrap();
            let ext_state = state.clone();
            let ext_pos = position.clone();
            let ext_seek_id = seek_id.clone();
            let ext_seek_done_id = seek_done_id.clone();
            
            thread::spawn(move || {
                    // Reconstruct the IMFSourceReader from the raw pointer
                    let ext_reader: IMFSourceReader = std::mem::transmute_copy(&reader_ptr);
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    let mut last_seek_id: i64 = 0;
                    let mut audio_pts_base: i64 = 0; // first PTS in the audio stream
                    let mut base_set = false;
                    let mut was_playing = false;
                    
                    loop {
                        let st = PlayerState::from(ext_state.load(Ordering::SeqCst));
                        if st == PlayerState::Stopped || st == PlayerState::Error { break; }
                        
                        // Check for seeks
                        let current_seek_id = ext_seek_id.load(Ordering::SeqCst);
                        if current_seek_id != last_seek_id {
                            let pos = ext_pos.load(Ordering::SeqCst);
                            let var = PROPVARIANT::from(pos);
                            let _ = ext_reader.SetCurrentPosition(&GUID::default(), &var);
                            let _ = atx.send(AudioCmd::Flush);
                            last_seek_id = current_seek_id;
                            base_set = false; // Re-acquire base on next sample
                        }

                        if st != PlayerState::Playing {
                            // Flush WASAPI on the Playing -> Paused transition
                            // audio syncing
                            if was_playing {
                                let _ = atx.send(AudioCmd::Flush);
                                base_set = false;
                                was_playing = false;
                            }
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        was_playing = true;

                        let mut actual_stream: u32 = 0;
                        let mut flags: u32 = 0;
                        let mut timestamp: i64 = 0;
                        let mut sample: Option<IMFSample> = None;

                        if ext_reader.ReadSample(ext_idx, 0,
                            Some(&mut actual_stream), Some(&mut flags),
                            Some(&mut timestamp), Some(&mut sample),
                        ).is_err() { break; }

                        if flags & 1 != 0 { break; } // EOS

                        if let Some(s) = sample {
                            // Set the base PTS on the first sample after start/seek
                            if !base_set {
                                audio_pts_base = timestamp;
                                base_set = true;
                            }
                            
                            // Slave to video position
                            let video_pos = ext_pos.load(Ordering::SeqCst);
                            
                            let audio_ahead_100ns = timestamp - video_pos;
                            
                            if audio_ahead_100ns > 500_000 {
                                let mut waited = 0;
                                while waited < 2000 {
                                    let vp = ext_pos.load(Ordering::SeqCst);
                                    if timestamp - vp <= 500_000 { break; } // Video caught up
                                    let st2 = PlayerState::from(ext_state.load(Ordering::SeqCst));
                                    if st2 == PlayerState::Stopped || st2 == PlayerState::Error { break; }
                                    if ext_seek_id.load(Ordering::SeqCst) != last_seek_id { break; }
                                    thread::sleep(Duration::from_millis(5));
                                    waited += 5;
                                }
                            } else if audio_ahead_100ns < -5_000_000 {
                                // too far behind, need to catch up
                                continue;
                            }
                            if let Ok(buf) = s.ConvertToContiguousBuffer() {
                                let mut p: *mut u8 = std::ptr::null_mut();
                                let mut ml: u32 = 0; let mut cl: u32 = 0;
                                if buf.Lock(&mut p, Some(&mut ml as *mut _), Some(&mut cl as *mut _)).is_ok() {
                                    let data = std::slice::from_raw_parts(p, cl as usize).to_vec();
                                    let _ = buf.Unlock();
                                    if atx.send(AudioCmd::Data(data)).is_err() { break; }
                                }
                            }
                        }
                    }
                    CoUninitialize();
            });
            None::<()>
        } else {
            None
        };
        let _ = ext_audio_tx;

        let _ = info_tx.send(Ok(MediaInfo { width, height, duration_ms, has_audio }));

        let mut d2d_target: Option<ID2D1HwndRenderTarget> = None;
        let mut d2d_bitmap: Option<ID2D1Bitmap> = None;
        let mut pixels: Option<Vec<u32>> = None;

        if let Some(hwnd_ptr) = hwnd {
            if hwnd_ptr != 0 {
                let hwnd_handle = HWND(hwnd_ptr);
                let options = D2D1_FACTORY_OPTIONS { debugLevel: D2D1_DEBUG_LEVEL_NONE };
                if let Ok(f) = D2D1CreateFactory::<ID2D1Factory>(
                    D2D1_FACTORY_TYPE_MULTI_THREADED,
                    Some(&options as *const _),
                ) {
                    let props = D2D1_RENDER_TARGET_PROPERTIES {
                        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_IGNORE,
                        },
                        dpiX: 0.0,
                        dpiY: 0.0,
                        usage: D2D1_RENDER_TARGET_USAGE_NONE,
                        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                    };
                    let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd: hwnd_handle,
                        pixelSize: D2D_SIZE_U { width, height },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    };
                    if let Ok(target) = f.CreateHwndRenderTarget(&props, &hwnd_props) {
                        d2d_target = Some(target);
                        eprintln!("[snek_apollo] D2D1 Hardware Rendering initialized on HWND.");
                        let ram = crate::hls::get_available_ram();
                        eprintln!("[snek_apollo] System Memory: {} MB available", ram / (1024 * 1024));
                    }
                }
            }
        }

        // Decode loop
        let mut playing = false;
        let mut clock_start: Option<Instant> = None;
        let mut pts_start: Option<i64> = None;
        let mut elapsed_at_pause: Duration = Duration::ZERO; // media time consumed before pause
        let mut hls_pts_offset: i64 = (start_ms as i64) * 10_000; 
        let mut seeking = false;
        let mut seek_target_100ns = 0i64;
        let mut _last_stored_ms = 0u64;
        let mut latest_seek = None;

        'decode: loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Cmd::Stop => break 'decode,
                    Cmd::Play => {
                        playing = true;
                        state.store(PlayerState::Playing as u8, Ordering::SeqCst);
                        if clock_start.is_none() {
                            clock_start = Some(Instant::now() - elapsed_at_pause);
                        }
                    }
                    Cmd::Pause => {
                        playing = false;
                        state.store(PlayerState::Paused as u8, Ordering::SeqCst);
                        // Snapshot how much media time has been consumed so far
                        if let Some(cs) = clock_start {
                            elapsed_at_pause = cs.elapsed();
                        }
                        clock_start = None;
                        if let Some(ref atx) = audio_tx {
                            let _ = atx.send(AudioCmd::Flush);
                        }
                    }
                    Cmd::Seek(ms) => {
                        latest_seek = Some(ms);
                    }
                    Cmd::SetVolume(vol) => {
                        if let Some(ref v) = audio_vol { let _ = v.SetMasterVolume(vol.clamp(0.0, 1.0), std::ptr::null()); }
                    }
                    Cmd::SetMute(mute) => {
                        if let Some(ref v) = audio_vol { let _ = v.SetMute(mute, std::ptr::null()); }
                    }
                }
            }

            if let Some(ms) = latest_seek.take() {
                let pos_100ns = (ms as i64) * 10_000;
                let sid = seek_id.fetch_add(1, Ordering::SeqCst) + 1;
                eprintln!("[snek_apollo] PROCESSING Cmd::Seek: {} ms (seek_id: {})", ms, sid);
                
                let is_hls = url.contains(".m3u8");
                let seek_ok = if let Some(ref vbuf) = hls_video_buffer {
                    if is_hls {
                        vbuf.request_seek(ms);
                        if let Some(ref abuf) = hls_audio_buffer {
                            abuf.request_seek(ms);
                        }
                        hls_pts_offset = pos_100ns;
                        seek_target_100ns = pos_100ns;
                        pts_start = None;
                        clock_start = None;
                        elapsed_at_pause = Duration::ZERO;
                        seeking = true;
                        position.store(pos_100ns, Ordering::SeqCst);
                        _last_stored_ms = ms;
                        seek_reader(&reader, 0)
                    } else {
                        seeking = true;
                        seek_target_100ns = pos_100ns;
                        hls_pts_offset = 0;
                        clock_start = None;
                        elapsed_at_pause = Duration::ZERO;
                        pts_start = None;
                        position.store(pos_100ns, Ordering::SeqCst);
                        _last_stored_ms = ms;
                        seek_reader(&reader, pos_100ns)
                    }
                } else {
                    seeking = true;
                    seek_target_100ns = pos_100ns;
                    let ok = seek_reader(&reader, pos_100ns);
                    hls_pts_offset = 0;
                    clock_start = None;
                    elapsed_at_pause = Duration::ZERO;
                    pts_start = None;
                    position.store(pos_100ns, Ordering::SeqCst);
                    _last_stored_ms = ms;
                    ok
                };

                if seek_ok {
                    if let Some(atx) = audio_tx.as_ref() {
                        let _ = atx.send(AudioCmd::Flush);
                    }
                }
                seek_done_id.store(sid, Ordering::SeqCst);
            }

            if !playing {
                // Check if we should exit while paused
                if state.load(Ordering::SeqCst) == PlayerState::Stopped as u8 { break 'decode; }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            
            // Check if we should exit before blocking on ReadSample
            if state.load(Ordering::SeqCst) == PlayerState::Stopped as u8 { break 'decode; }

            let mut actual_stream: u32 = 0;
            let mut flags: u32 = 0;
            let mut timestamp: i64 = 0;
            let mut sample: Option<IMFSample> = None;

            if reader.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0,
                Some(&mut actual_stream), Some(&mut flags),
                Some(&mut timestamp), Some(&mut sample),
            ).is_err() {
                state.store(PlayerState::Error as u8, Ordering::SeqCst); break;
            }

            if flags & 1 != 0 {
                if seeking {
                    eprintln!("[snek_apollo] EOS during seek, ignoring");
                    seeking = false;
                    continue;
                }
                state.store(PlayerState::EndOfStream as u8, Ordering::SeqCst); break;
            }

            let sample = match sample { Some(s) => s, None => {
                if seeking { continue; }
                continue;
            }};
            
            if seeking {

                let is_early = if hls_video_buffer.is_none() {
                    timestamp < seek_target_100ns - 333_333 // 1 frame at 30fps
                } else {
                    false
                };

                if is_early {
                    // Discard this sample and keep seeking
                    continue;
                }

                seeking = false;
                pts_start = Some(timestamp);
                clock_start = Some(Instant::now());
                eprintln!("[snek_apollo] Exact seek landed at: ts={} (target={})", timestamp, seek_target_100ns);
                position.store(if hls_video_buffer.is_some() { hls_pts_offset } else { timestamp }, Ordering::SeqCst);
            }

            // Detect timestamp resets or backward jumps
            if let Some(ps) = pts_start {
                if timestamp < ps - 50_000_000 {
                    pts_start = Some(timestamp);
                }
            }

            if let (Some(cs), Some(ps)) = (clock_start, pts_start) {
                let target = Duration::from_nanos(((timestamp - ps).max(0) as u64) * 100);
                let elapsed = cs.elapsed();
                if target > elapsed {
                    let wait = target - elapsed;
                    if wait < Duration::from_secs(2) { thread::sleep(wait); }
                }
            }

            // Audio sample
            if Some(actual_stream) == audio_idx {
                if let Some(ref atx) = audio_tx {
                    if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                        let mut p: *mut u8 = std::ptr::null_mut();
                        let mut ml: u32 = 0; let mut cl: u32 = 0;
                        if buf.Lock(&mut p, Some(&mut ml as *mut _), Some(&mut cl as *mut _)).is_ok() {
                            let data = std::slice::from_raw_parts(p, cl as usize).to_vec();
                            let _ = buf.Unlock();
                            let _ = atx.send(AudioCmd::Data(data));
                        }
                    }
                }
                continue;
            }

            // Video sample
            if actual_stream != video_idx { continue; }

            if pts_start.is_none() { 
                pts_start = Some(timestamp);
                eprintln!("[snek_apollo] Initial Video PTS: {} (hls_pts_offset: {})", timestamp, hls_pts_offset);
            }
            
            let absolute_pos = if seeking {
                seek_target_100ns
            } else {
                timestamp
            };

            let current_stored = position.load(Ordering::SeqCst);
            if seeking || absolute_pos > current_stored {
                position.store(absolute_pos, Ordering::SeqCst);
            }

            // Ensure absolute_pos doesn't jump backwards significantly without reset
            let current_ms = (absolute_pos / 10_000) as u64;
            _last_stored_ms = current_ms;


            let buffer = match sample.ConvertToContiguousBuffer() { Ok(b) => b, Err(_) => continue };
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut ml: u32 = 0; let mut cl: u32 = 0;
            
            // Try to use 2D buffer interface for more accurate stride if available
            let mut current_stride = effective_stride;
            let mut is_2d = false;
            
            if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
                let mut stride_2d: i32 = 0;
                if unsafe { buffer2d.Lock2D(&mut ptr, &mut stride_2d).is_ok() } {
                    current_stride = stride_2d.abs() as usize;
                    is_2d = true;
                    // Log once per second to avoid spamming
                    static mut LAST_LOG: Option<Instant> = None;
                    unsafe {
                        let now = Instant::now();
                        if LAST_LOG.is_none() || now.duration_since(LAST_LOG.unwrap()) > Duration::from_secs(1) {
                            eprintln!("[snek_apollo] Frame Stride (Lock2D): {} | is_nv12: {}", current_stride, is_nv12);
                            LAST_LOG = Some(now);
                        }
                    }
                }
            }
            
            if !is_2d {
                if buffer.Lock(&mut ptr, Some(&mut ml as *mut _), Some(&mut cl as *mut _)).is_err() { continue; }
                
                // Detect stride mismatch from buffer size
                let bpp = if is_nv12 { 1 } else { 4 };
                let expected_min = width as usize * height as usize * bpp;
                let actual_len = cl as usize;
                
                if actual_len > expected_min {
                    // Rule 1: If current_stride divides the buffer perfectly, it's likely just height padding.
                    // Rule 2: If it doesn't, try to find a stride that does, starting from width aligned to 16.
                    if actual_len % current_stride != 0 {
                        let aligned_16 = ((width as usize + 15) & !15) * bpp;
                        let aligned_32 = ((width as usize + 31) & !31) * bpp;
                        
                        if actual_len % aligned_16 == 0 {
                            current_stride = aligned_16;
                        } else if actual_len % aligned_32 == 0 {
                            current_stride = aligned_32;
                        } else {
                            // desparation
                            let calc_stride = actual_len / height as usize;
                            if calc_stride >= width as usize * bpp && calc_stride < width as usize * bpp + 256 {
                                current_stride = calc_stride;
                            }
                        }
                        
                        static mut LAST_STRIDE_WARN: usize = 0;
                        unsafe {
                            if LAST_STRIDE_WARN != current_stride {
                                eprintln!("[snek_apollo] STRIDE ADJUSTED: {} (Buffer size: {})", current_stride, actual_len);
                                LAST_STRIDE_WARN = current_stride;
                            }
                        }
                    }
                }
            } else {
                // For 2D buffers, cl is not automatically provided by fuckass Lock2D
                cl = (current_stride * height as usize) as u32;
            }
            
            static mut FRAME_COUNT: u32 = 0;
            unsafe {
                if FRAME_COUNT < 5 {
                    eprintln!("[snek_apollo] Frame {}: Stride={}, Size={}, 2D={}", FRAME_COUNT, current_stride, cl, is_2d);
                    FRAME_COUNT += 1;
                }
            }

            let raw = unsafe { std::slice::from_raw_parts(ptr, cl as usize) };
            if pixels.is_none() || pixels.as_ref().unwrap().len() != (width * height) as usize {
                pixels = Some(vec![0u32; (width * height) as usize]);
            }
            let pixels_ref = pixels.as_mut().unwrap();

            if is_nv12 {
                let y_plane_size = current_stride * height as usize;
                let y_plane = &raw[..y_plane_size.min(raw.len())];
                let uv_plane = if y_plane_size < raw.len() { &raw[y_plane_size..] } else { &[] };
                for r in 0..height as usize {
                    let src_r = if media_stride < 0 { height as usize - 1 - r } else { r };
                    for c in 0..width as usize {
                        let yi = src_r * current_stride + c;
                        let uvi = (src_r / 2) * current_stride + (c & !1);
                        let yv = *y_plane.get(yi).unwrap_or(&16) as i32 - 16;
                        let u = *uv_plane.get(uvi).unwrap_or(&128) as i32 - 128;
                        let v = *uv_plane.get(uvi + 1).unwrap_or(&128) as i32 - 128;
                        let rv = ((298 * yv + 409 * v + 128) >> 8).clamp(0, 255) as u32;
                        let gv = ((298 * yv - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u32;
                        let bv = ((298 * yv + 516 * u + 128) >> 8).clamp(0, 255) as u32;
                        pixels_ref[r * width as usize + c] = (rv << 16) | (gv << 8) | bv;
                    }
                }
            } else {
                for y in 0..height as usize {
                    let src_y = if media_stride < 0 { height as usize - 1 - y } else { y };
                    let row = src_y * current_stride;
                    if row + (width as usize * 4) > raw.len() { continue; }
                    for x in 0..width as usize {
                        let o = row + x * 4;
                        pixels_ref[y * width as usize + x] =
                            ((raw[o+2] as u32) << 16) | ((raw[o+1] as u32) << 8) | (raw[o] as u32);
                    }
                }
            }

            if is_2d {
                if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
                    let _ = unsafe { buffer2d.Unlock2D() };
                }
            } else {
                let _ = buffer.Unlock();
            }
            
            // Render directly to HWND via Direct2D if available
            if let Some(ref target) = d2d_target {
                if d2d_bitmap.is_none() {
                    let props = D2D1_BITMAP_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_IGNORE,
                        },
                        dpiX: 0.0,
                        dpiY: 0.0,
                    };
                    if let Err(e) = target.CreateBitmap(
                        D2D_SIZE_U { width, height },
                        Some(pixels_ref.as_ptr() as *const _),
                        width * 4,
                        &props,
                    ) {
                        eprintln!("[snek_apollo] CreateBitmap failed: {:?}", e);
                        d2d_bitmap = None;
                    } else {
                        d2d_bitmap = target.CreateBitmap(
                            D2D_SIZE_U { width, height },
                            Some(pixels_ref.as_ptr() as *const _),
                            width * 4,
                            &props,
                        ).ok();
                    }
                } else {
                    if let Some(ref bmp) = d2d_bitmap {
                        let rect = D2D_RECT_U { left: 0, top: 0, right: width, bottom: height };
                        let _ = bmp.CopyFromMemory(Some(&rect), pixels_ref.as_ptr() as *const _, width * 4);
                    }
                }

                if let Some(ref bmp) = d2d_bitmap {
                    target.BeginDraw();
                    target.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
                    let size = target.GetSize();
                    if width > 0 && height > 0 {
                        // SCALIIIIINGGG!!!!!!!!!!!!!!!!!!!
                        let scale_x = size.width / width as f32;
                        let scale_y = size.height / height as f32;
                        let scale = scale_x.min(scale_y);
                        
                        if scale < 0.1 {
                             // Log if suspiciously small
                             eprintln!("[snek_apollo] Scaling anomaly: window {}x{}, video {}x{}, scale {}", size.width, size.height, width, height, scale);
                        }
                        let target_w = width as f32 * scale;
                        let target_h = height as f32 * scale;
                        let x_off = (size.width - target_w) / 2.0;
                        let y_off = (size.height - target_h) / 2.0;
                        
                        let dest_rect = D2D_RECT_F {
                            left: x_off,
                            top: y_off,
                            right: x_off + target_w,
                            bottom: y_off + target_h,
                        };
                        
                        // Temp log scaling details for troubleshooting
                        static mut LAST_SCALE_LOG: Option<Instant> = None;
                        unsafe {
                            let now = Instant::now();
                            if LAST_SCALE_LOG.is_none() || now.duration_since(LAST_SCALE_LOG.unwrap()) > Duration::from_secs(5) {
                                eprintln!("[snek_apollo] D2D Rendering: Window {}x{}, Video {}x{}, Scale {}, DestRect {:?}", 
                                    size.width, size.height, width, height, scale, dest_rect);
                                LAST_SCALE_LOG = Some(now);
                            }
                        }

                        target.DrawBitmap(
                            bmp,
                            Some(&dest_rect),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                        if let Err(e) = target.EndDraw(None, None) {
                            eprintln!("[snek_apollo] EndDraw failed: {:?}. Recreating target next frame.", e);
                            d2d_target = None;
                            d2d_bitmap = None;
                        }
                    }
                }
            } else {
                // Software fallback push to python
                let frame = VideoFrame { width, height, data: pixels_ref.clone(), timestamp_ms: (timestamp / 10_000) as u64 };
                if frame_tx.send(frame).is_err() { break; }
            }
        }

        drop(audio_tx);
        let _ = MFShutdown();
        CoUninitialize();
    }
}

#[cfg(target_os = "linux")]
struct LinuxMediaProbe {
    info: MediaInfo,
    fps_num: u64,
    fps_den: u64,
}

#[cfg(target_os = "linux")]
struct LinuxDecoderProcess {
    child: Child,
    thread: thread::JoinHandle<()>,
}

#[cfg(target_os = "linux")]
struct LinuxAudioProcess {
    child: Child,
}

#[cfg(target_os = "linux")]
fn parse_ratio(value: &str) -> Option<(u64, u64)> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "N/A" {
        return None;
    }

    if let Some((num, den)) = trimmed.split_once('/') {
        let num = num.trim().parse::<u64>().ok()?;
        let den = den.trim().parse::<u64>().ok()?;
        if num == 0 || den == 0 {
            return None;
        }
        return Some((num, den));
    }

    let fps = trimmed.parse::<f64>().ok()?;
    if fps <= 0.0 {
        return None;
    }
    Some(((fps * 1000.0).round() as u64, 1000))
}

#[cfg(target_os = "linux")]
fn parse_duration_ms(value: &str) -> u64 {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "N/A" {
        return 0;
    }

    trimmed
        .parse::<f64>()
        .ok()
        .map(|secs| (secs.max(0.0) * 1000.0) as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn probe_media_linux(url: &str) -> Result<LinuxMediaProbe, String> {
    let video_output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
            url,
        ])
        .output()
        .map_err(|e| format!("ffprobe not available: {e}"))?;

    if !video_output.status.success() {
        let stderr = String::from_utf8_lossy(&video_output.stderr);
        return Err(format!("ffprobe failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&video_output.stdout);
    let mut width = 0u32;
    let mut height = 0u32;
    let mut duration_ms = 0u64;
    let mut fps_num = 30u64;
    let mut fps_den = 1u64;

    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "width" => width = value.trim().parse::<u32>().unwrap_or(0),
                "height" => height = value.trim().parse::<u32>().unwrap_or(0),
                "duration" => duration_ms = parse_duration_ms(value),
                "r_frame_rate" => {
                    if let Some((num, den)) = parse_ratio(value) {
                        fps_num = num;
                        fps_den = den;
                    }
                }
                _ => {}
            }
        }
    }

    if width == 0 || height == 0 {
        return Err("ffprobe did not return video dimensions".into());
    }

    let audio_output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            url,
        ])
        .output()
        .map_err(|e| format!("ffprobe audio probe failed: {e}"))?;

    let has_audio = audio_output.status.success() && !audio_output.stdout.is_empty();

    Ok(LinuxMediaProbe {
        info: MediaInfo {
            width,
            height,
            duration_ms,
            has_audio,
        },
        fps_num,
        fps_den,
    })
}

#[cfg(target_os = "linux")]
fn spawn_linux_decoder(
    url: &str,
    width: u32,
    height: u32,
    start_ms: u64,
    fps_num: u64,
    fps_den: u64,
    frame_tx: mpsc::SyncSender<VideoFrame>,
    position: Arc<AtomicI64>,
) -> Result<LinuxDecoderProcess, String> {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-nostats", "-re"]);
    if start_ms > 0 {
        command.args(["-ss", &format!("{:.3}", start_ms as f64 / 1000.0)]);
    }
    command.args([
        "-i",
        url,
        "-an",
        "-sn",
        "-dn",
        "-pix_fmt",
        "bgra",
        "-f",
        "rawvideo",
        "-",
    ]);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("ffmpeg stdout was not piped")?;
    let frame_size = width as usize * height as usize * 4;
    let frame_interval_100ns = ((10_000_000u128 * fps_den.max(1) as u128) / fps_num.max(1) as u128)
        .max(1) as i64;

    let thread = thread::spawn(move || {
        let mut bytes = vec![0u8; frame_size];
        let mut frame_index = 0u64;

        while stdout.read_exact(&mut bytes).is_ok() {
            let mut data = Vec::with_capacity(width as usize * height as usize);
            for pixel in bytes.chunks_exact(4) {
                let b = pixel[0] as u32;
                let g = pixel[1] as u32;
                let r = pixel[2] as u32;
                data.push(b | (g << 8) | (r << 16));
            }

            let timestamp_100ns = (start_ms as i64) * 10_000 + frame_index as i64 * frame_interval_100ns;
            position.store(timestamp_100ns, Ordering::SeqCst);

            if frame_tx.send(VideoFrame {
                width,
                height,
                data,
                timestamp_ms: (timestamp_100ns / 10_000).max(0) as u64,
            }).is_err() {
                break;
            }

            frame_index += 1;
        }
    });

    Ok(LinuxDecoderProcess { child, thread })
}

#[cfg(target_os = "linux")]
fn stop_linux_decoder(process: &mut Option<LinuxDecoderProcess>) {
    if let Some(mut process) = process.take() {
        let _ = process.child.kill();
        let _ = process.child.wait();
        let _ = process.thread.join();
    }
}

#[cfg(target_os = "linux")]
fn spawn_linux_audio(url: &str, start_ms: u64, volume: f32, muted: bool) -> Result<LinuxAudioProcess, String> {
    let volume = if muted {
        0
    } else {
        (volume.clamp(0.0, 1.0) * 100.0).round() as i32
    };

    let mut command = Command::new("ffplay");
    command.args(["-nodisp", "-autoexit", "-hide_banner", "-loglevel", "error", "-volume", &volume.to_string()]);
    if start_ms > 0 {
        command.args(["-ss", &format!("{:.3}", start_ms as f64 / 1000.0)]);
    }
    command.args(["-i", url]);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let child = command.spawn().map_err(|e| format!("failed to spawn ffplay: {e}"))?;
    Ok(LinuxAudioProcess { child })
}

#[cfg(target_os = "linux")]
fn stop_linux_audio(process: &mut Option<LinuxAudioProcess>) {
    if let Some(mut process) = process.take() {
        let _ = process.child.kill();
        let _ = process.child.wait();
    }
}

#[cfg(target_os = "linux")]
fn decode_thread_linux(
    url: String,
    info_tx: mpsc::Sender<Result<MediaInfo, String>>,
    frame_tx: mpsc::SyncSender<VideoFrame>,
    cmd_rx: mpsc::Receiver<Cmd>,
    state: Arc<AtomicU8>,
    position: Arc<AtomicI64>,
    start_ms: u64,
    _hwnd: Option<isize>,
    _buf_ref: Arc<std::sync::Mutex<Option<Arc<StreamBuffer>>>>,
) {
    let probe = match probe_media_linux(&url) {
        Ok(probe) => probe,
        Err(err) => {
            state.store(PlayerState::Error as u8, Ordering::SeqCst);
            let _ = info_tx.send(Err(err));
            return;
        }
    };

    position.store((start_ms as i64) * 10_000, Ordering::SeqCst);

    let _ = info_tx.send(Ok(MediaInfo {
        width: probe.info.width,
        height: probe.info.height,
        duration_ms: probe.info.duration_ms,
        has_audio: probe.info.has_audio,
    }));

    let mut current_start_ms = start_ms;
    let mut playing = false;
    let mut decoder: Option<LinuxDecoderProcess> = None;
    let mut audio: Option<LinuxAudioProcess> = None;
    let mut volume = 1.0f32;
    let mut muted = false;

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Play => {
                    if !playing {
                        match spawn_linux_decoder(
                            &url,
                            probe.info.width,
                            probe.info.height,
                            current_start_ms,
                            probe.fps_num,
                            probe.fps_den,
                            frame_tx.clone(),
                            position.clone(),
                        ) {
                            Ok(process) => {
                                decoder = Some(process);
                                if probe.info.has_audio {
                                    match spawn_linux_audio(&url, current_start_ms, volume, muted) {
                                        Ok(process) => audio = Some(process),
                                        Err(err) => eprintln!("[snek_apollo] {err}"),
                                    }
                                }
                                playing = true;
                                state.store(PlayerState::Playing as u8, Ordering::SeqCst);
                            }
                            Err(err) => {
                                state.store(PlayerState::Error as u8, Ordering::SeqCst);
                                eprintln!("[snek_apollo] {err}");
                            }
                        }
                    }
                }
                Cmd::Pause => {
                    current_start_ms = (position.load(Ordering::SeqCst) / 10_000).max(0) as u64;
                    stop_linux_decoder(&mut decoder);
                    stop_linux_audio(&mut audio);
                    playing = false;
                    state.store(PlayerState::Paused as u8, Ordering::SeqCst);
                }
                Cmd::Stop => {
                    stop_linux_decoder(&mut decoder);
                    stop_linux_audio(&mut audio);
                    state.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                    return;
                }
                Cmd::Seek(ms) => {
                    current_start_ms = ms;
                    position.store((ms as i64) * 10_000, Ordering::SeqCst);
                    stop_linux_decoder(&mut decoder);
                    stop_linux_audio(&mut audio);
                    if playing {
                        match spawn_linux_decoder(
                            &url,
                            probe.info.width,
                            probe.info.height,
                            current_start_ms,
                            probe.fps_num,
                            probe.fps_den,
                            frame_tx.clone(),
                            position.clone(),
                        ) {
                            Ok(process) => {
                                decoder = Some(process);
                                if probe.info.has_audio {
                                    match spawn_linux_audio(&url, current_start_ms, volume, muted) {
                                        Ok(process) => audio = Some(process),
                                        Err(err) => eprintln!("[snek_apollo] {err}"),
                                    }
                                }
                                state.store(PlayerState::Playing as u8, Ordering::SeqCst);
                            }
                            Err(err) => {
                                playing = false;
                                state.store(PlayerState::Error as u8, Ordering::SeqCst);
                                eprintln!("[snek_apollo] {err}");
                            }
                        }
                    } else {
                        state.store(PlayerState::Paused as u8, Ordering::SeqCst);
                    }
                }
                Cmd::SetVolume(v) => {
                    volume = v;
                    if playing && probe.info.has_audio {
                        current_start_ms = (position.load(Ordering::SeqCst) / 10_000).max(0) as u64;
                        stop_linux_audio(&mut audio);
                        match spawn_linux_audio(&url, current_start_ms, volume, muted) {
                            Ok(process) => audio = Some(process),
                            Err(err) => eprintln!("[snek_apollo] {err}"),
                        }
                    }
                }
                Cmd::SetMute(m) => {
                    muted = m;
                    if playing && probe.info.has_audio {
                        current_start_ms = (position.load(Ordering::SeqCst) / 10_000).max(0) as u64;
                        stop_linux_audio(&mut audio);
                        match spawn_linux_audio(&url, current_start_ms, volume, muted) {
                            Ok(process) => audio = Some(process),
                            Err(err) => eprintln!("[snek_apollo] {err}"),
                        }
                    }
                }
            }
        }

        if playing {
            if let Some(process) = decoder.as_mut() {
                match process.child.try_wait() {
                    Ok(Some(_status)) => {
                        stop_linux_decoder(&mut decoder);
                        stop_linux_audio(&mut audio);
                        playing = false;
                        state.store(PlayerState::EndOfStream as u8, Ordering::SeqCst);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        stop_linux_decoder(&mut decoder);
                        stop_linux_audio(&mut audio);
                        playing = false;
                        state.store(PlayerState::Error as u8, Ordering::SeqCst);
                        eprintln!("[snek_apollo] ffmpeg process error: {err}");
                    }
                }
            }
        }

        thread::sleep(std::time::Duration::from_millis(10));
    }
}
