use std::sync::Arc;
use std::sync::atomic::AtomicU64;

#[cfg(windows)]
use windows::{
    core::*,
    Win32::Media::MediaFoundation::*,
};

#[cfg(windows)]
pub fn remux_to_mp4(input_path: &str, output_path: &str, is_ts: bool, _progress: Option<Arc<AtomicU64>>, _total: Option<Arc<AtomicU64>>) -> Result<()> {
    unsafe {
        let input_path_w = HSTRING::from(input_path);
        let output_path_w = HSTRING::from(output_path);

        let byte_stream = MFCreateFile(MF_ACCESSMODE_READ, MF_OPENMODE_FAIL_IF_NOT_EXIST, MF_FILEFLAGS_NONE, &input_path_w)?;

        let reader = MFCreateSourceReaderFromByteStream(&byte_stream, None)?;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.unwrap();
        
        let container_type = if is_ts {
            &MFTranscodeContainerType_MPEG2
        } else {
            &MFTranscodeContainerType_MPEG4
        };
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, container_type)?;

        let sink_writer = MFCreateSinkWriterFromURL(&output_path_w, None, Some(&attrs))?;

        // Map source streams to sink streams
        let mut stream_map = std::collections::HashMap::new();

        for i in 0..16u32 {
            let mt = match reader.GetNativeMediaType(i, 0) {
                Ok(mt) => mt,
                Err(_) => break,
            };

            let major_type = mt.GetGUID(&MF_MT_MAJOR_TYPE)?;
            if major_type != MFMediaType_Video && major_type != MFMediaType_Audio {
                continue;
            }

            let sink_stream_idx = sink_writer.AddStream(&mt)?;
            sink_writer.SetInputMediaType(sink_stream_idx, &mt, None)?;
            stream_map.insert(i, sink_stream_idx);
            
            // Enable stream for reading
            reader.SetStreamSelection(i, true)?;
        }

        if stream_map.is_empty() {
            return Err(Error::new(HRESULT(0x80040154u32 as i32), "No streams found")); 
        }

        sink_writer.BeginWriting()?;

        loop {
            let mut stream_index = 0u32;
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;

            reader.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut stream_index), Some(&mut flags), Some(&mut timestamp), Some(&mut sample))?;

            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }

            if let Some(s) = sample {
                if let Some(&sink_idx) = stream_map.get(&stream_index) {
                    sink_writer.WriteSample(sink_idx, &s)?;
                }
            }
        }

        sink_writer.Finalize()?;
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn remux_to_mp4(_input_path: &str, _output_path: &str, _is_ts: bool, _progress: Option<Arc<AtomicU64>>, _total: Option<Arc<AtomicU64>>) -> Result<(), String> {
    Err("Remuxing is only supported on Windows".into())
}
