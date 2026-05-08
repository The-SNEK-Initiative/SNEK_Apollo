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
) {
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

        // Helper: create a source reader from a byte stream buffer
        fn resolve_buffer(
            buffer: &Arc<StreamBuffer>,
            attrs_ptr: Option<&IMFAttributes>,
        ) -> Result<IMFSourceReader, String> {
            unsafe {
                let byte_stream: IMFByteStream = HlsByteStream::new(buffer.clone()).into();

                // Set MIME type based on content
                if let Ok(bs_attrs) = byte_stream.cast::<IMFAttributes>() {
                    let mime = if buffer.is_ts() { w!("video/mp2t") } else { w!("video/mp4") };
                    let _ = bs_attrs.SetString(&MF_BYTESTREAM_CONTENT_TYPE, mime);
                }

                let resolver = MFCreateSourceResolver()
                    .map_err(|e| format!("MFCreateSourceResolver: {:?}", e))?;

                let mut obj_type = MF_OBJECT_TYPE::default();
                let mut source: Option<IUnknown> = None;

                let hint = if buffer.is_ts() { w!("dummy.ts") } else { w!("dummy.mp4") };

                let flags = MF_RESOLUTION_MEDIASOURCE.0 as u32 | 0x00000040; // MF_RESOLUTION_CONTENT_DOES_NOT_HAVE_ANY_QUERY_PARAMETERS

                resolver.CreateObjectFromByteStream(
                    &byte_stream, hint, flags, None, &mut obj_type, &mut source,
                ).map_err(|e| format!("Source resolver failed: {:?}", e))?;

                let source = source.ok_or("Source was None")?;
                MFCreateSourceReaderFromMediaSource(
                    &source.cast::<IMFMediaSource>().map_err(|e| format!("{:?}", e))?,
                    attrs_ptr,
                ).map_err(|e| format!("Open MediaSource: {:?}", e))
            }
        }

        let mut audio_reader_ext: Option<IMFSourceReader> = None;
        let mut audio_idx_ext: Option<u32> = None;
        let mut hls_duration_ms: u64 = 0;
        let mut hls_video_buffer: Option<Arc<StreamBuffer>> = None;
        let mut hls_audio_buffer: Option<Arc<StreamBuffer>> = None;

        let reader = if url.starts_with("http") {
            let buffer = StreamBuffer::new();
            hls_video_buffer = Some(buffer.clone());
            if let Ok(mut guard) = buf_ref.lock() {
                *guard = Some(buffer.clone());
            }
            eprintln!("[player] Waiting for buffer...");
            hls_audio_buffer = spawn_hls_downloader(url.clone(), buffer.clone(), start_ms);

            while buffer.total_written() < 256 * 1024 && !buffer.is_eos() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            hls_duration_ms = buffer.get_duration_ms();
            eprintln!("[player] Buffer ready: {} bytes", buffer.total_written());

            if buffer.has_error() {
                let mut a: Option<IMFAttributes> = None;
                let _ = MFCreateAttributes(&mut a, 2);
                MFCreateSourceReaderFromURL(&HSTRING::from(&url), a.as_ref()).ok()
            } else {
                let video_reader = match resolve_buffer(&buffer, attrs_ptr.as_ref()) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[player] {}", e);
                        let _ = info_tx.send(Err(e)); return;
                    }
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
                Some(video_reader)
            }
        } else {
            MFCreateSourceReaderFromURL(&HSTRING::from(&url), attrs_ptr.as_ref()).ok()
        };

        let reader = match reader {
            Some(r) => r,
            None => { let _ = info_tx.send(Err("Failed to open media source".into())); return; }
        };

        let mut video_idx: Option<u32> = None;
        let mut audio_idx: Option<u32> = None;
        for i in 0..16u32 {
            if let Ok(mt) = reader.GetNativeMediaType(i, 0) {
                let major = mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default();
                if major == MFMediaType_Video && video_idx.is_none() { video_idx = Some(i); }
                else if major == MFMediaType_Audio && audio_idx.is_none() { audio_idx = Some(i); }
            }
        }
        let video_idx = match video_idx { Some(i) => i, None => { let _ = info_tx.send(Err("No video".into())); return; } };

        let mt = MFCreateMediaType().unwrap();
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).unwrap();
        let _ = reader.SetCurrentMediaType(video_idx, None, &mt);

        let actual = reader.GetCurrentMediaType(video_idx).unwrap();
        let size = actual.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let width = (size >> 32) as u32;
        let height = (size & 0xFFFFFFFF) as u32;
        let duration_ms = if hls_duration_ms > 0 { hls_duration_ms } else {
            let var = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION).unwrap_or_default();
            (var.Anonymous.Anonymous.uhVal / 10_000) as u64
        };

        let mut audio_vol: Option<ISimpleAudioVolume> = None;
        let audio_tx = (|| {
            let aidx = audio_idx.or(audio_idx_ext)?;
            let the_reader = if audio_idx.is_some() { &reader } else { audio_reader_ext.as_ref()? };
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            let mix_fmt = &*client.GetMixFormat().ok()?;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, mix_fmt as *const _ as *const _, None).ok()?;
            let render: IAudioRenderClient = client.GetService().ok()?;
            audio_vol = client.GetService().ok();
            let (atx, arx) = mpsc::channel();
            let bpf = mix_fmt.nBlockAlign as usize;
            let bf = client.GetBufferSize().unwrap_or(4096);
            let sc = SendCom(client); let sr = SendCom(render);
            thread::spawn(move || audio_thread_win(arx, sc, sr, bf, bpf));
            Some(atx)
        })();

        let has_audio = audio_tx.is_some();
        let _ = info_tx.send(Ok(MediaInfo { width, height, duration_ms, has_audio }));

        let mut d2d_target: Option<ID2D1HwndRenderTarget> = None;
        let mut d2d_bitmap: Option<ID2D1Bitmap> = None;
        if let Some(hwnd_ptr) = hwnd {
            if hwnd_ptr != 0 {
                let hwnd_handle = HWND(hwnd_ptr);
                if let Ok(f) = D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_MULTI_THREADED, None) {
                    let mut rect = RECT::default(); let _ = GetClientRect(hwnd_handle, &mut rect);
                    let props = D2D1_RENDER_TARGET_PROPERTIES::default();
                    let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES { hwnd: hwnd_handle, pixelSize: D2D_SIZE_U { width: (rect.right-rect.left) as u32, height: (rect.bottom-rect.top) as u32 }, presentOptions: D2D1_PRESENT_OPTIONS_NONE };
                    if let Ok(target) = f.CreateHwndRenderTarget(&props, &hprops) { d2d_target = Some(target); }
                }
            }
        }

        let mut audio_pump_handle = None;
        if let (Some(ref ar), Some(idx), Some(atx)) = (&audio_reader_ext, audio_idx_ext, &audio_tx) {
            let r_ptr: usize = std::mem::transmute_copy(ar);
            let atx_c = atx.clone(); let st_c = state.clone();
            audio_pump_handle = Some(thread::spawn(move || {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let reader: IMFSourceReader = unsafe { std::mem::transmute_copy(&r_ptr) };
                loop {
                    if st_c.load(Ordering::SeqCst) == PlayerState::Stopped as u8 { break; }
                    let mut s: Option<IMFSample> = None;
                    if unsafe { reader.ReadSample(idx, 0, None, None, None, Some(&mut s)) }.is_err() { break; }
                    if let Some(sample) = s {
                        if let Ok(buf) = unsafe { sample.ConvertToContiguousBuffer() } {
                            let mut p = std::ptr::null_mut(); let mut cl = 0;
                            if unsafe { buf.Lock(&mut p, None, Some(&mut cl)) }.is_ok() {
                                let data = unsafe { std::slice::from_raw_parts(p, cl as usize).to_vec() };
                                let _ = unsafe { buf.Unlock() };
                                if atx_c.send(AudioCmd::Data(data)).is_err() { break; }
                            }
                        }
                    }
                }
                unsafe { CoUninitialize(); }
            }));
        }

        let mut playing = false;
        let mut clock_start: Option<Instant> = None;
        let mut pts_start: Option<i64> = None;
        let mut hls_pts_offset: i64 = (start_ms as i64) * 10_000;
        let mut seeking = false;
        let mut last_stored_ms = 0u64;

        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Cmd::Play => { playing = true; state.store(PlayerState::Playing as u8, Ordering::SeqCst); clock_start = Some(Instant::now()); }
                    Cmd::Pause => { playing = false; state.store(PlayerState::Paused as u8, Ordering::SeqCst); clock_start = None; pts_start = None; }
                    Cmd::Stop => {
                        state.store(PlayerState::Stopped as u8, Ordering::SeqCst);
                        if let Some(h) = audio_pump_handle.take() { let _ = h.join(); }
                        drop(d2d_target); drop(audio_vol); drop(audio_tx);
                        let _ = MFShutdown(); CoUninitialize(); return;
                    }
                    Cmd::Seek(ms) => {
                        let pos = (ms as i64) * 10_000;
                        let sid = seek_id.fetch_add(1, Ordering::SeqCst) + 1;
                        if let Some(ref vbuf) = hls_video_buffer {
                            vbuf.request_seek(ms);
                            if let Some(ref abuf) = hls_audio_buffer { abuf.request_seek(ms); }
                            seek_reader(&reader, 0);
                            hls_pts_offset = pos; pts_start = None; clock_start = Some(Instant::now()); seeking = true;
                            position.store(pos, Ordering::SeqCst); last_stored_ms = ms;
                        } else {
                            if seek_reader(&reader, pos) {
                                hls_pts_offset = 0; pts_start = None; clock_start = Some(Instant::now()); seeking = true;
                                position.store(pos, Ordering::SeqCst); last_stored_ms = ms;
                            }
                        }
                        seek_done_id.store(sid, Ordering::SeqCst);
                        if let Some(ref atx) = audio_tx { let _ = atx.send(AudioCmd::Flush); }
                    }
                    Cmd::SetVolume(v) => { if let Some(ref vol) = audio_vol { let _ = vol.SetMasterVolume(v, std::ptr::null()); } }
                    Cmd::SetMute(m) => { if let Some(ref vol) = audio_vol { let _ = vol.SetMute(BOOL(if m {1} else {0}), std::ptr::null()); } }
                }
            }

            if !playing { thread::sleep(Duration::from_millis(10)); continue; }

            let mut actual = 0; let mut flags = 0; let mut ts = 0; let mut s: Option<IMFSample> = None;
            if reader.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut actual), Some(&mut flags), Some(&mut ts), Some(&mut s)).is_err() { break; }
            if flags & 1 != 0 {
                if seeking { seeking = false; continue; }
                state.store(PlayerState::EndOfStream as u8, Ordering::SeqCst); playing = false; continue;
            }

            if let Some(sample) = s {
                if seeking { seeking = false; pts_start = Some(ts); position.store(hls_pts_offset, Ordering::SeqCst); }
                
                if Some(actual) == audio_idx {
                    if let Some(ref atx) = audio_tx {
                        if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                            let mut p = std::ptr::null_mut(); let mut cl = 0;
                            if buf.Lock(&mut p, None, Some(&mut cl)).is_ok() {
                                let data = std::slice::from_raw_parts(p, cl as usize).to_vec();
                                let _ = buf.Unlock();
                                let _ = atx.send(AudioCmd::Data(data));
                            }
                        }
                    }
                    continue;
                }

                if actual != video_idx { continue; }
                if pts_start.is_none() { pts_start = Some(ts); }
                
                let base = pts_start.unwrap();
                let absolute_pos = (ts - base) + hls_pts_offset;
                let current_ms = (absolute_pos / 10_000) as u64;
                
                if current_ms != last_stored_ms {
                    position.store(absolute_pos, Ordering::SeqCst);
                    last_stored_ms = current_ms;
                }

                if let Some(start) = clock_start {
                    let elapsed = start.elapsed().as_micros() as i64;
                    let target = (ts - base) / 10;
                    if target > elapsed { thread::sleep(Duration::from_micros((target - elapsed).min(100_000) as u64)); }
                }

                let buf = sample.ConvertToContiguousBuffer().unwrap();
                let mut p = std::ptr::null_mut(); let mut cl = 0;
                buf.Lock(&mut p, None, Some(&mut cl)).unwrap();
                let pixels = std::slice::from_raw_parts(p, cl as usize);
                
                if let Some(ref target) = d2d_target {
                    if d2d_bitmap.is_none() {
                        let props = D2D1_BITMAP_PROPERTIES { pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE }, dpiX: 96.0, dpiY: 96.0 };
                        if let Ok(bmp) = target.CreateBitmap(D2D_SIZE_U { width, height }, Some(pixels.as_ptr() as *const _), width * 4, &props) { d2d_bitmap = Some(bmp); }
                    } else if let Some(ref bmp) = d2d_bitmap {
                        let rect = D2D_RECT_U { left: 0, top: 0, right: width, bottom: height };
                        let _ = bmp.CopyFromMemory(Some(&rect), pixels.as_ptr() as *const _, width * 4);
                    }

                    if let Some(ref bmp) = d2d_bitmap {
                        target.BeginDraw(); target.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
                        let size = target.GetSize();
                        let scale = (size.width / width as f32).min(size.height / height as f32);
                        let tw = width as f32 * scale; let th = height as f32 * scale;
                        let rect = D2D_RECT_F { left: (size.width - tw) / 2.0, top: (size.height - th) / 2.0, right: (size.width + tw) / 2.0, bottom: (size.height + th) / 2.0 };
                        target.DrawBitmap(bmp, Some(&rect), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                        let _ = target.EndDraw(None, None);
                    }
                } else {
                    let mut data = vec![0u32; (width * height) as usize];
                    for y in 0..height as usize {
                        for x in 0..width as usize {
                            let o = (y * width as usize + x) * 4;
                            data[y * width as usize + x] = ((pixels[o+2] as u32) << 16) | ((pixels[o+1] as u32) << 8) | (pixels[o] as u32);
                        }
                    }
                    let _ = frame_tx.send(VideoFrame { width, height, data, timestamp_ms: current_ms });
                }
                let _ = buf.Unlock();
            }
        }
    }
}
