use std::sync::{Arc, Mutex, Condvar};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::io::Read;
use std::collections::{HashSet, HashMap};

#[cfg(windows)]
use windows::Win32::System::SystemInformation::*;

// A random access streaming buffer.
//
// All data pushed in is retained in a contiguous `Vec<u8>`, enabling
// backward seeks required by Media Foundation's MP4 demuxer.
static CLEANUP_PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn register_for_cleanup(path: String) {
    if let Ok(mut paths) = CLEANUP_PATHS.lock() {
        paths.push(path);
    }
}

pub fn cleanup_temp_files() {
    if let Ok(mut paths) = CLEANUP_PATHS.lock() {
        for path in paths.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn get_available_ram() -> u64 {
    #[cfg(windows)]
    unsafe {
        let mut status = MEMORYSTATUSEX::default();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status).is_ok() {
            return status.ullAvailPhys;
        }
    }
    1024 * 1024 * 1024 // Fallback to 1GB
}

pub struct StreamBuffer {
    inner: Mutex<StreamBufferInner>,
    cond: Condvar,
    eos: AtomicBool,
    error: AtomicBool,
    is_ts: AtomicBool,          // true = MPEG-TS, false = fMP4 or raw MP4
    total_len: AtomicU64,       // Content-Length (known file size), 0 = unknown
    duration_ms: AtomicU64,     // Calculated duration for HLS streams
    seek_ms: AtomicU64,         // Requested seek position in ms, u64::MAX = none
    pub total_segments: AtomicU64,      // Total number of segments to download
    pub downloaded_segments: AtomicU64,  // Number of segments downloaded so far
    pub prefetched: AtomicBool,          // true = all data is loaded, no streaming needed
    pub cancelled: AtomicBool,           // true = downloader should stop
}

struct StreamBufferInner {
    data: Vec<u8>,
    error_msg: String,
    segment_cache: HashMap<String, Vec<u8>>,
}

impl StreamBuffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(StreamBufferInner {
                data: Vec::with_capacity(256 * 1024),
                error_msg: String::new(),
                segment_cache: HashMap::new(),
            }),
            cond: Condvar::new(),
            eos: AtomicBool::new(false),
            error: AtomicBool::new(false),
            is_ts: AtomicBool::new(false),
            total_len: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            seek_ms: AtomicU64::new(u64::MAX),
            total_segments: AtomicU64::new(0),
            downloaded_segments: AtomicU64::new(0),
            prefetched: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn download_progress(&self) -> f32 {
        let total = self.total_segments.load(Ordering::SeqCst);
        if total == 0 { return 0.0; }
        let done = self.downloaded_segments.load(Ordering::SeqCst);
        (done as f32 / total as f32).min(1.0)
    }

    pub fn is_prefetched(&self) -> bool {
        self.prefetched.load(Ordering::SeqCst)
    }

    pub fn set_duration_ms(&self, dur: u64) {
        self.duration_ms.store(dur, Ordering::SeqCst);
    }

    pub fn get_duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::SeqCst)
    }

    pub fn set_total_len(&self, len: u64) {
        self.total_len.store(len, Ordering::SeqCst);
    }

    pub fn get_total_len(&self) -> u64 {
        self.total_len.load(Ordering::SeqCst)
    }

    pub fn set_is_ts(&self, val: bool) {
        self.is_ts.store(val, Ordering::SeqCst);
    }

    pub fn is_ts(&self) -> bool {
        self.is_ts.load(Ordering::SeqCst)
    }

    pub fn request_seek(&self, ms: u64) {
        self.seek_ms.store(ms, Ordering::SeqCst);
        let mut inner = self.inner.lock().unwrap();
        inner.data.clear();
        self.eos.store(false, Ordering::SeqCst);
        self.cond.notify_all();
    }

    pub fn take_seek_request(&self) -> Option<u64> {
        let val = self.seek_ms.swap(u64::MAX, Ordering::SeqCst);
        if val == u64::MAX { None } else { Some(val) }
    }

    pub fn push(&self, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        inner.data.extend_from_slice(bytes);
        self.cond.notify_all();
    }

    pub fn cache_get(&self, key: &str) -> Option<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        inner.segment_cache.get(key).cloned()
    }

    pub fn cache_put(&self, key: String, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner.segment_cache.insert(key, data);
    }

    pub fn set_eos(&self) {
        self.eos.store(true, Ordering::SeqCst);
        self.cond.notify_all();
    }

    pub fn is_eos(&self) -> bool {
        self.eos.load(Ordering::SeqCst)
    }

    pub fn set_error(&self, msg: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.error_msg = msg;
        self.error.store(true, Ordering::SeqCst);
        self.eos.store(true, Ordering::SeqCst);
        self.cond.notify_all();
    }

    pub fn has_error(&self) -> bool {
        self.error.load(Ordering::SeqCst)
    }

    pub fn error_msg(&self) -> String {
        self.inner.lock().unwrap().error_msg.clone()
    }

    /// Total bytes written into the buffer so far.
    pub fn total_written(&self) -> usize {
        self.inner.lock().unwrap().data.len()
    }

    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();

        let timeout = std::time::Duration::from_secs(30);
        let deadline = std::time::Instant::now() + timeout;

        while inner.data.len() <= offset && !self.eos.load(Ordering::SeqCst) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return 0;
            }
            let (guard, wait_result) = self.cond.wait_timeout(inner, remaining).unwrap();
            inner = guard;
            if wait_result.timed_out() && inner.data.len() <= offset {
                return 0;
            }
        }

        if inner.data.len() <= offset {
            return 0;
        }

        let avail = inner.data.len() - offset;
        let to_copy = buf.len().min(avail);
        buf[..to_copy].copy_from_slice(&inner.data[offset..offset + to_copy]);
        to_copy
    }
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

fn resolve_url(base: &str, relative: &str) -> String {
    if relative.starts_with("http") {
        return relative.to_string();
    }
    if let Some(pos) = base.rfind('/') {
        return format!("{}/{}", &base[..pos], relative);
    }
    relative.to_string()
}

// Result from spawning HLS downloader: video buffer + optional audio buffer.
pub struct HlsDownloadResult {
    pub video: Arc<StreamBuffer>,
    pub audio: Option<Arc<StreamBuffer>>,
}

// Build the correct Referer header for known CDNs
fn build_request(url: &str) -> ureq::Request {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();
    
    let mut req = agent.get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "*/*")
        .set("Accept-Language", "en-US,en;q=0.9")
        .set("Sec-Fetch-Dest", "video")
        .set("Sec-Fetch-Mode", "no-cors")
        .set("Sec-Fetch-Site", "cross-site")
        .set("Sec-Ch-Ua", "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\"")
        .set("Sec-Ch-Ua-Mobile", "?0")
        .set("Sec-Ch-Ua-Platform", "\"Windows\"");
        
    if url.contains("gelbooru.com") {
        req = req.set("Referer", "https://gelbooru.com/");
    } else if url.contains("donmai.us") {
        req = req.set("Referer", "https://danbooru.donmai.us/");
    } else if url.contains("rule34.xxx") {
        req = req.set("Referer", "https://rule34.xxx/");
    } else if url.contains("uptobox-ggcdn.top") || url.contains("lh3-ggcontent.top") {
        req = req.set("Referer", "https://play2.cdn-xvideos-xnxx.xyz/");
        req = req.set("Origin", "https://play2.cdn-xvideos-xnxx.xyz");
    }
    req
}

pub fn spawn_hls_downloader(url: String, buffer: Arc<StreamBuffer>, start_ms: u64) -> Option<Arc<StreamBuffer>> {
    // Check if it's HLS or direct file
    if !url.contains(".m3u8") {
        // Don't spawn downloader for local files
        if !url.starts_with("http") && !url.contains("://") {
            return None;
        }

        // Direct HTTP download - no audio buffer needed
        let buf = buffer.clone();
        let u = url.clone();
        thread::spawn(move || download_direct(u, buf));
        return None;
    }

    // HLS - need to check for separate audio rendition first (synchronously parse master)
    let master_text = match build_request(&url).call() {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(e) => {
            buffer.set_error(format!("HLS playlist fetch failed: {}", e));
            return None;
        }
    };

    if !master_text.contains("#EXTM3U") {
        buffer.set_error("Invalid M3U8 file".to_string());
        return None;
    }

    // Parse master playlist
    let mut video_url = url.clone(); // the URL itself is the media playlist
    let mut audio_url: Option<String> = None;

    if master_text.contains("#EXT-X-STREAM-INF") {
        // Pick highest bandwidth variant
        let mut best_url = String::new();
        let mut max_bw = 0;
        let lines: Vec<&str> = master_text.lines().collect();
        for i in 0..lines.len() {
            if lines[i].starts_with("#EXT-X-STREAM-INF") {
                let bw = if let Some(p) = lines[i].find("BANDWIDTH=") {
                    let sub = &lines[i][p+10..];
                    let end = sub.find(',').or_else(|| sub.find(' ')).unwrap_or(sub.len());
                    sub[..end].parse::<u64>().unwrap_or(0)
                } else { 0 };

                if i + 1 < lines.len() {
                    let path = lines[i+1].trim();
                    if bw >= max_bw {
                        max_bw = bw;
                        best_url = resolve_url(&url, path);
                    }
                }
            }
            // Parse separate audio rendition
            if lines[i].starts_with("#EXT-X-MEDIA:") && lines[i].contains("TYPE=AUDIO") {
                if let Some(uri_pos) = lines[i].find("URI=\"") {
                    let sub = &lines[i][uri_pos+5..];
                    if let Some(end) = sub.find('"') {
                        let audio_path = &sub[..end];
                        audio_url = Some(resolve_url(&url, audio_path));
                    }
                }
            }
        }
        if !best_url.is_empty() {
            video_url = best_url;
            eprintln!("[hls] Selected variant (BW={}): {}", max_bw, video_url);
        }
    }

    // Spawn audio downloader if separate audio rendition exists
    let audio_buffer = if let Some(ref aurl) = audio_url {
        eprintln!("[hls] Separate audio rendition: {}", aurl);
        let abuf = StreamBuffer::new();
        let abuf_clone = abuf.clone();
        let aurl_clone = aurl.clone();
        thread::spawn(move || download_hls_rendition(aurl_clone, abuf_clone, "audio", start_ms));
        Some(abuf)
    } else {
        None
    };

    // Spawn video downloader
    let vbuf = buffer.clone();
    let vurl = video_url.clone();
    thread::spawn(move || download_hls_rendition(vurl, vbuf, "video", start_ms));

    audio_buffer
}

fn download_direct(url: String, buffer: Arc<StreamBuffer>) {
    eprintln!("[downloader] Caching direct file: {}", url);
    
    match build_request(&url).call() {
        Ok(resp) => {
            if let Some(cl) = resp.header("Content-Length").and_then(|h| h.parse::<u64>().ok()) {
                eprintln!("[downloader] Content-Length: {}", cl);
                buffer.set_total_len(cl);
            }
            
            let mut reader = resp.into_reader();
            let mut chunk = [0u8; 65536];
            let mut total = 0;
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 || buffer.cancelled.load(Ordering::SeqCst) { break; }
                buffer.push(&chunk[..n]);
                total += n;
            }
            eprintln!("[downloader] Finished download: {} bytes", total);
            if buffer.get_total_len() == 0 {
                buffer.set_total_len(total as u64);
            }
        }
        Err(e) => {
            let msg = format!("{}: {}", url, e);
            // cloudflare is a bitch.
            buffer.set_error(msg);
            return;
        }
    }
    buffer.set_eos();
}

fn download_hls_rendition(media_url: String, buffer: Arc<StreamBuffer>, label: &str, mut start_ms: u64) {
    let mut downloaded_segments = HashSet::new();
    let mut init_downloaded = false;
    let mut accumulated_ms: f64 = 0.0;
    let mut skipped_up_to_start = false;
    let mut init_data: Option<Vec<u8>> = None;

    loop {
        // Check for seek request
        if let Some(new_start) = buffer.take_seek_request() {
            eprintln!("[hls-{}] Seek requested to {} ms", label, new_start);
            start_ms = new_start;
            accumulated_ms = 0.0;
            skipped_up_to_start = false;
            downloaded_segments.clear();
            // Re-push init segment if we have it
            if let Some(ref init) = init_data {
                buffer.push(init);
            }
        }

        let resp = match build_request(&media_url).call() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[hls-{}] Media playlist fetch error: {}", label, e);
                thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
        };

        let text = resp.into_string().unwrap_or_default();
        let is_live = !text.contains("#EXT-X-ENDLIST");
        let has_map = text.contains("#EXT-X-MAP:");
        
        // Detect TS vs fMP4 on first iteration
        if !init_downloaded {
            if !has_map {
                buffer.set_is_ts(true);
                eprintln!("[hls-{}] Segment type: MPEG-TS", label);
            } else {
                eprintln!("[hls-{}] Segment type: fMP4", label);
            }
        }

        // Handle Init Segment (fMP4 only)
        if !init_downloaded {
            if let Some(pos) = text.find("#EXT-X-MAP:URI=\"") {
                let sub = &text[pos+16..];
                if let Some(end) = sub.find('"') {
                    let init_path = &sub[..end];
                    let init_url = resolve_url(&media_url, init_path);
                    eprintln!("[hls-{}] Downloading init segment: {}", label, init_url);
                    if let Ok(resp) = ureq::get(&init_url).set("User-Agent", USER_AGENT).call() {
                        let mut chunk = Vec::new();
                        if resp.into_reader().read_to_end(&mut chunk).is_ok() {
                            buffer.push(&chunk);
                            init_data = Some(chunk);
                            init_downloaded = true;
                        }
                    }
                }
            } else {
                // MPEG-TS doesn't have init segments
                init_downloaded = true;
            }
        }

        let lines: Vec<&str> = text.lines().collect();
        
        // Calculate total duration for VOD on first pass
        if downloaded_segments.is_empty() && !is_live && buffer.get_duration_ms() == 0 {
            let mut total_duration_ms: f64 = 0.0;
            for line in &lines {
                if line.starts_with("#EXTINF:") {
                    let duration_str = line[8..].split(',').next().unwrap_or("0");
                    if let Ok(dur) = duration_str.parse::<f64>() {
                        total_duration_ms += dur * 1000.0;
                    }
                }
            }
            if total_duration_ms > 0.0 {
                eprintln!("[hls-{}] Calculated VOD duration: {} ms", label, total_duration_ms as u64);
                buffer.set_duration_ms(total_duration_ms as u64);
            }
        }

        for i in 0..lines.len() {
            if lines[i].starts_with("#EXTINF") {
                let duration_str = lines[i][8..].split(',').next().unwrap_or("0");
                let dur_ms = duration_str.parse::<f64>().unwrap_or(0.0) * 1000.0;

                if i + 1 < lines.len() {
                    let segment = lines[i+1].trim();

                    // Skip segments before start_ms
                    if !skipped_up_to_start {
                        if accumulated_ms + dur_ms < start_ms as f64 {
                            accumulated_ms += dur_ms;
                            continue;
                        } else {
                            skipped_up_to_start = true;
                        }
                    }

                    if !downloaded_segments.contains(segment) {
                        downloaded_segments.insert(segment.to_string());
                        
                        let seg_url = resolve_url(&media_url, segment);
                        
                        // Check cache first
                        if let Some(cached_data) = buffer.cache_get(&seg_url) {
                            eprintln!("[hls-{}] Using cached segment: {}", label, segment);
                            buffer.push(&cached_data);
                        } else {
                            if let Ok(seg_resp) = build_request(&seg_url).call() {
                                let mut chunk = Vec::new();
                                if seg_resp.into_reader().read_to_end(&mut chunk).is_ok() {
                                    eprintln!("[hls-{}] Buffered segment: {} ({} bytes)", label, segment, chunk.len());
                                    buffer.cache_put(seg_url, chunk.clone());
                                    buffer.push(&chunk);
                                }
                            }
                        }
                    }
                    accumulated_ms += dur_ms;
                }
            }
        }

        if !is_live {
            // For VOD, we stay in the loop to handle future seeks
            thread::sleep(std::time::Duration::from_millis(500));
            if buffer.cancelled.load(Ordering::SeqCst) { break; }
            continue;
        }

        thread::sleep(std::time::Duration::from_secs(2));
        if buffer.cancelled.load(Ordering::SeqCst) { break; }
    }
}

// Parsed segment info from an HLS media playlist
struct HlsSegment {
    url: String,
    index: usize,
}

pub struct HlsPrefetchResult {
    pub video_file: String,
    pub audio_file: Option<String>,
    pub duration_ms: u64,
    pub is_live: bool,
}
pub fn prefetch_hls(url: &str, progress: Arc<AtomicU64>, total: Arc<AtomicU64>) -> Result<HlsPrefetchResult, String> {
    let master_text = build_request(url).call()
        .map_err(|e| format!("HLS playlist fetch failed: {}", e))?
        .into_string().map_err(|e| format!("Read error: {}", e))?;

    if !master_text.contains("#EXTM3U") {
        return Err("Invalid M3U8 file".into());
    }

    // Parse master playlist to find video + audio rendition URLs
    let mut video_url = url.to_string();
    let mut audio_url: Option<String> = None;

    if master_text.contains("#EXT-X-STREAM-INF") {
        let mut best_url = String::new();
        let mut max_bw = 0u64;
        let lines: Vec<&str> = master_text.lines().collect();
        for i in 0..lines.len() {
            if lines[i].starts_with("#EXT-X-STREAM-INF") {
                let bw = if let Some(p) = lines[i].find("BANDWIDTH=") {
                    let sub = &lines[i][p+10..];
                    let end = sub.find(',').or_else(|| sub.find(' ')).unwrap_or(sub.len());
                    sub[..end].parse::<u64>().unwrap_or(0)
                } else { 0 };
                if i + 1 < lines.len() && bw >= max_bw {
                    max_bw = bw;
                    best_url = resolve_url(url, lines[i+1].trim());
                }
            }
            if lines[i].starts_with("#EXT-X-MEDIA:") && lines[i].contains("TYPE=AUDIO") {
                if let Some(uri_pos) = lines[i].find("URI=\"") {
                    let sub = &lines[i][uri_pos+5..];
                    if let Some(end) = sub.find('"') {
                        audio_url = Some(resolve_url(url, &sub[..end]));
                    }
                }
            }
        }
        if !best_url.is_empty() {
            video_url = best_url;
            eprintln!("[hls-prefetch] Selected variant (BW={}): {}", max_bw, video_url);
        }
    }

    // Fetch the media playlist
    let media_text = if video_url != url {
        build_request(&video_url).call()
            .map_err(|e| format!("Media playlist fetch failed: {}", e))?
            .into_string().map_err(|e| format!("Read error: {}", e))?
    } else {
        master_text.clone()
    };

    let is_live = !media_text.contains("#EXT-X-ENDLIST");
    if is_live {
        return Ok(HlsPrefetchResult {
            video_file: String::new(),
            audio_file: None,
            duration_ms: 0,
            is_live: true,
        });
    }

    // Parse segments and compute duration
    let lines: Vec<&str> = media_text.lines().collect();
    let has_map = media_text.contains("#EXT-X-MAP:");
    let is_ts = !has_map;

    let mut segments: Vec<HlsSegment> = Vec::new();
    let mut total_duration_ms: f64 = 0.0;
    let mut seg_idx = 0;

    // Find init segment URL if fMP4
    let init_url = if has_map {
        if let Some(pos) = media_text.find("#EXT-X-MAP:URI=\"") {
            let sub = &media_text[pos+16..];
            sub.find('"').map(|end| resolve_url(&video_url, &sub[..end]))
        } else { None }
    } else { None };

    for i in 0..lines.len() {
        if lines[i].starts_with("#EXTINF") {
            let dur_str = lines[i][8..].split(',').next().unwrap_or("0");
            let dur_ms = dur_str.parse::<f64>().unwrap_or(0.0) * 1000.0;
            total_duration_ms += dur_ms;
            if i + 1 < lines.len() {
                let path = lines[i+1].trim();
                if !path.starts_with('#') {
                    segments.push(HlsSegment {
                        url: resolve_url(&video_url, path),
                        index: seg_idx,
                    });
                    seg_idx += 1;
                }
            }
        }
    }

    let duration_ms = total_duration_ms as u64;
    let num_segments = segments.len();
    eprintln!("[hls-prefetch] VOD: {} segments, {} ms total", num_segments, duration_ms);

    // Download video and audio segments in parallel
    let video_progress = progress.clone();
    let video_total = total.clone();
    
    let v_handle = thread::spawn(move || {
        prefetch_segments(segments, init_url, is_ts, "video", video_progress, video_total)
    });

    let a_handle = if let Some(ref aurl) = audio_url {
        let aurl_clone = aurl.clone();
        let a_progress = Arc::new(AtomicU64::new(0)); // Separate trackers for audio
        let a_total = Arc::new(AtomicU64::new(0));
        
        Some(thread::spawn(move || {
            let audio_text = build_request(&aurl_clone).call()
                .map_err(|e| format!("Audio playlist: {}", e))?
                .into_string().map_err(|e| format!("Read: {}", e))?;

            let alines: Vec<&str> = audio_text.lines().collect();
            let a_has_map = audio_text.contains("#EXT-X-MAP:");
            let a_is_ts = !a_has_map;

            let a_init_url = if a_has_map {
                if let Some(pos) = audio_text.find("#EXT-X-MAP:URI=\"") {
                    let sub = &audio_text[pos+16..];
                    sub.find('"').map(|end| resolve_url(&aurl_clone, &sub[..end]))
                } else { None }
            } else { None };

            let mut a_segments = Vec::new();
            let mut a_idx = 0;
            for i in 0..alines.len() {
                if alines[i].starts_with("#EXTINF") && i + 1 < alines.len() {
                    let path = alines[i+1].trim();
                    if !path.starts_with('#') {
                        a_segments.push(HlsSegment { url: resolve_url(&aurl_clone, path), index: a_idx });
                        a_idx += 1;
                    }
                }
            }
            prefetch_segments(a_segments, a_init_url, a_is_ts, "audio", a_progress, a_total)
        }))
    } else {
        None
    };

    let video_file = v_handle.join().unwrap()?;
    let audio_file = if let Some(ah) = a_handle {
        Some(ah.join().unwrap()?)
    } else {
        None
    };

    Ok(HlsPrefetchResult {
        video_file,
        audio_file,
        duration_ms,
        is_live: false,
    })
}


/// Download all segments in parallel using a thread pool, then concatenate in-order to a local temp file
fn prefetch_segments(
    segments: Vec<HlsSegment>,
    init_url: Option<String>,
    is_ts: bool,
    label: &str,
    progress: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
) -> Result<String, String> {
    let num_segments = segments.len();
    total.store(num_segments as u64, Ordering::SeqCst);
    progress.store(0, Ordering::SeqCst);

    // Download init segment first (if fMP4)
    let init_data = if let Some(ref iurl) = init_url {
        eprintln!("[hls-prefetch-{}] Downloading init segment", label);
        match build_request(iurl).call() {
            Ok(resp) => {
                let mut data = Vec::new();
                resp.into_reader().read_to_end(&mut data).ok();
                Some(data)
            }
            Err(e) => return Err(format!("Init segment failed: {}", e)),
        }
    } else { None };

    // Download all segments in parallel with a thread pool
    let num_workers = 32.min(num_segments);
    let results: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::new(Mutex::new(vec![None; num_segments]));
    let work_queue: Arc<Mutex<Vec<HlsSegment>>> = Arc::new(Mutex::new(segments));
    let total_segs = num_segments;
    
    let mut handles = Vec::new();
    let label_str = label.to_string();
    
    for _worker_id in 0..num_workers {
        let q = work_queue.clone();
        let r = results.clone();
        let c = progress.clone();
        let l = label_str.clone();
        
        handles.push(thread::spawn(move || {
            loop {
                let seg = {
                    let mut queue = q.lock().unwrap();
                    queue.pop()
                };
                let seg = match seg {
                    Some(s) => s,
                    None => break, // No more work
                };
                
                let mut data = Vec::new();
                let mut retries = 0;
                loop {
                    match build_request(&seg.url).call() {
                        Ok(resp) => {
                            data.clear();
                            if resp.into_reader().read_to_end(&mut data).is_ok() {
                                break;
                            }
                        }
                        Err(e) => {
                            retries += 1;
                            if retries >= 3 {
                                eprintln!("[hls-prefetch-{}] Segment {} failed after 3 retries: {}", l, seg.index, e);
                                break;
                            }
                            thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }
                
                if !data.is_empty() {
                    let mut results = r.lock().unwrap();
                    results[seg.index] = Some(data);
                }
                let done = c.fetch_add(1, Ordering::SeqCst) + 1;
                if done % 10 == 0 || done == total_segs as u64 {
                    eprintln!("[hls-prefetch-{}] Progress: {}/{}", l, done, total_segs);
                }
            }
        }));
    }

    // Wait for all workers to finish
    for (i, h) in handles.into_iter().enumerate() {
        if let Err(e) = h.join() {
            eprintln!("[hls-prefetch-{}] Worker {} panicked: {:?}", label, i, e);
        }
    }

    // Create unique temp file in system Temp directory to avoid conflicts and clutter
    let temp_dir = std::env::temp_dir();
    let ext = if is_ts { "ts" } else { "mp4" };
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let cache_filename = format!("snek_hls_cache_{}_{}.{}", label, timestamp, ext);
    let cache_path = temp_dir.join(&cache_filename).to_string_lossy().into_owned();
    
    register_for_cleanup(cache_path.clone());
    eprintln!("[hls-prefetch-{}] Writing segments to {}", label, cache_path);
    
    let mut file = std::fs::File::create(&cache_path).map_err(|e| format!("Failed to create cache file {}: {}", cache_path, e))?;
    use std::io::Write;

    if let Some(init) = init_data {
        file.write_all(&init).map_err(|e| format!("Init write error: {}", e))?;
    }
    
    let results = results.lock().unwrap();
    let mut total_bytes = 0;
    for (i, seg_data) in results.iter().enumerate() {
        match seg_data {
            Some(data) => {
                file.write_all(data).map_err(|e| format!("Segment {} write error: {}", i, e))?;
                total_bytes += data.len();
            }
            None => {
                eprintln!("[hls-prefetch-{}] WARNING: segment {} is missing, stream will have a gap", label, i);
            }
        }
    }
    
    eprintln!("[hls-prefetch-{}] Download complete, concatenated {} bytes to {}", label, total_bytes, cache_path);
    drop(file);

    Ok(cache_path)
}
