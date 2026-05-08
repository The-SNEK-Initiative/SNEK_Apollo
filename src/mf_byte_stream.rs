#[cfg(windows)]
use windows::core::*;
#[cfg(windows)]
use windows::Win32::Media::MediaFoundation::*;
#[cfg(windows)]
use windows::Win32::Foundation::*;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use crate::hls::StreamBuffer;

#[cfg(windows)]
#[implement(IMFByteStream, IMFAttributes, IMFGetService)]
pub struct SnekByteStream {
    buffer: Arc<StreamBuffer>,
    pos: std::sync::atomic::AtomicU64,
    last_read_count: std::sync::atomic::AtomicU32,
    attributes: IMFAttributes,
}

#[cfg(windows)]
impl SnekByteStream {
    pub fn new(buffer: Arc<StreamBuffer>) -> Self {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 0).unwrap() };
        Self { 
            buffer,
            pos: std::sync::atomic::AtomicU64::new(0),
            last_read_count: std::sync::atomic::AtomicU32::new(0),
            attributes: attributes.unwrap(),
        }
    }
}

#[cfg(windows)]
impl IMFGetService_Impl for SnekByteStream {
    fn GetService(&self, guidservice: *const GUID, riid: *const GUID, ppvobject: *mut *mut std::ffi::c_void) -> Result<()> {
        unsafe {
            let _service = *guidservice;
            let interface = *riid;
            
            // Many MF components query for IMFAttributes via GetService
            if interface == IMFAttributes::IID {
                let attr: IMFAttributes = self.cast()?;
                *ppvobject = std::mem::transmute(attr);
                return Ok(());
            }
            
            Err(E_NOINTERFACE.into())
        }
    }
}

#[cfg(windows)]
impl IMFAttributes_Impl for SnekByteStream {
    fn GetItem(&self, guidkey: *const GUID, pvalue: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attributes.GetItem(guidkey, Some(pvalue)) }
    }
    fn GetItemType(&self, guidkey: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        unsafe { self.attributes.GetItemType(guidkey) }
    }
    fn CompareItem(&self, guidkey: *const GUID, value: *const PROPVARIANT) -> Result<BOOL> {
        unsafe { self.attributes.CompareItem(guidkey, value) }
    }
    fn Compare(&self, ptheattributes: Option<&IMFAttributes>, resulttype: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> {
        unsafe { self.attributes.Compare(ptheattributes, resulttype) }
    }
    fn GetUINT32(&self, guidkey: *const GUID) -> Result<u32> {
        unsafe { self.attributes.GetUINT32(guidkey) }
    }
    fn GetUINT64(&self, guidkey: *const GUID) -> Result<u64> {
        unsafe { self.attributes.GetUINT64(guidkey) }
    }
    fn GetDouble(&self, guidkey: *const GUID) -> Result<f64> {
        unsafe { self.attributes.GetDouble(guidkey) }
    }
    fn GetGUID(&self, guidkey: *const GUID) -> Result<GUID> {
        unsafe { self.attributes.GetGUID(guidkey) }
    }
    fn GetStringLength(&self, guidkey: *const GUID) -> Result<u32> {
        unsafe { self.attributes.GetStringLength(guidkey) }
    }
    fn GetString(&self, guidkey: *const GUID, pwszvalue: PWSTR, cchmaxlength: u32, pcchlength: *mut u32) -> Result<()> {
        unsafe {
            let opt_pcchlength = if pcchlength.is_null() { None } else { Some(pcchlength) };
            if pwszvalue.0.is_null() {
                self.attributes.GetString(guidkey, &mut [], opt_pcchlength)
            } else {
                let slice = std::slice::from_raw_parts_mut(pwszvalue.0, cchmaxlength as usize);
                self.attributes.GetString(guidkey, slice, opt_pcchlength)
            }
        }
    }
    fn GetAllocatedString(&self, guidkey: *const GUID, ppwszvalue: *mut PWSTR, pcchlength: *mut u32) -> Result<()> {
        unsafe { self.attributes.GetAllocatedString(guidkey, ppwszvalue, pcchlength) }
    }
    fn GetBlobSize(&self, guidkey: *const GUID) -> Result<u32> {
        unsafe { self.attributes.GetBlobSize(guidkey) }
    }
    fn GetBlob(&self, guidkey: *const GUID, pbuf: *mut u8, cbsize: u32, pcbsize: *mut u32) -> Result<()> {
        unsafe {
            let opt_pcbsize = if pcbsize.is_null() { None } else { Some(pcbsize) };
            if pbuf.is_null() {
                self.attributes.GetBlob(guidkey, &mut [], opt_pcbsize)
            } else {
                let slice = std::slice::from_raw_parts_mut(pbuf, cbsize as usize);
                self.attributes.GetBlob(guidkey, slice, opt_pcbsize)
            }
        }
    }
    fn GetAllocatedBlob(&self, guidkey: *const GUID, ppbuf: *mut *mut u8, pcbsize: *mut u32) -> Result<()> {
        unsafe { self.attributes.GetAllocatedBlob(guidkey, ppbuf, pcbsize) }
    }
    fn GetUnknown(&self, guidkey: *const GUID, riid: *const GUID, ppv: *mut *mut std::ffi::c_void) -> Result<()> {
        unsafe {
            let unknown: IUnknown = self.attributes.GetUnknown(guidkey)?;
            let result = unknown.query(&*riid, ppv as _);
            result.ok()
        }
    }
    fn SetItem(&self, guidkey: *const GUID, value: *const PROPVARIANT) -> Result<()> {
        unsafe { self.attributes.SetItem(guidkey, value) }
    }
    fn DeleteItem(&self, guidkey: *const GUID) -> Result<()> {
        unsafe { self.attributes.DeleteItem(guidkey) }
    }
    fn DeleteAllItems(&self) -> Result<()> {
        unsafe { self.attributes.DeleteAllItems() }
    }
    fn SetUINT32(&self, guidkey: *const GUID, unvalue: u32) -> Result<()> {
        unsafe { self.attributes.SetUINT32(guidkey, unvalue) }
    }
    fn SetUINT64(&self, guidkey: *const GUID, unvalue: u64) -> Result<()> {
        unsafe { self.attributes.SetUINT64(guidkey, unvalue) }
    }
    fn SetDouble(&self, guidkey: *const GUID, fvalue: f64) -> Result<()> {
        unsafe { self.attributes.SetDouble(guidkey, fvalue) }
    }
    fn SetGUID(&self, guidkey: *const GUID, guidvalue: *const GUID) -> Result<()> {
        unsafe { self.attributes.SetGUID(guidkey, guidvalue) }
    }
    fn SetString(&self, guidkey: *const GUID, pwszvalue: &PCWSTR) -> Result<()> {
        unsafe { self.attributes.SetString(guidkey, *pwszvalue) }
    }
    fn SetBlob(&self, guidkey: *const GUID, pbuf: *const u8, cbsize: u32) -> Result<()> {
        unsafe {
            if pbuf.is_null() {
                self.attributes.SetBlob(guidkey, &[])
            } else {
                let slice = std::slice::from_raw_parts(pbuf, cbsize as usize);
                self.attributes.SetBlob(guidkey, slice)
            }
        }
    }
    fn SetUnknown(&self, guidkey: *const GUID, punknown: Option<&IUnknown>) -> Result<()> {
        unsafe { self.attributes.SetUnknown(guidkey, punknown) }
    }
    fn LockStore(&self) -> Result<()> {
        unsafe { self.attributes.LockStore() }
    }
    fn UnlockStore(&self) -> Result<()> {
        unsafe { self.attributes.UnlockStore() }
    }
    fn GetCount(&self) -> Result<u32> {
        unsafe { self.attributes.GetCount() }
    }
    fn GetItemByIndex(&self, unindex: u32, pguidkey: *mut GUID, pvalue: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attributes.GetItemByIndex(unindex, pguidkey, Some(pvalue)) }
    }
    fn CopyAllItems(&self, pdest: Option<&IMFAttributes>) -> Result<()> {
        unsafe { self.attributes.CopyAllItems(pdest) }
    }
}

#[cfg(windows)]
impl IMFByteStream_Impl for SnekByteStream {
    fn GetCapabilities(&self) -> Result<u32> {
        Ok(MFBYTESTREAM_IS_READABLE | MFBYTESTREAM_IS_SEEKABLE)
    }

    fn GetLength(&self) -> Result<u64> {
        let len = self.buffer.get_total_len();
        if len > 0 {
            Ok(len)
        } else {
            Ok(u64::MAX)
        }
    }

    fn SetCurrentPosition(&self, position: u64) -> Result<()> {
        // Only log if it's a significant jump or reset
        let old_pos = self.pos.load(std::sync::atomic::Ordering::SeqCst);
        if position == 0 || (position as i64 - old_pos as i64).abs() > 1024 * 1024 {
            eprintln!("[SnekByteStream] SetCurrentPosition jump: {} -> {}", old_pos, position);
        }
        self.pos.store(position, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn SetLength(&self, _length: u64) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetCurrentPosition(&self) -> Result<u64> {
        let pos = self.pos.load(std::sync::atomic::Ordering::SeqCst);
        Ok(pos)
    }

    fn IsEndOfStream(&self) -> Result<BOOL> {
        let pos = self.pos.load(std::sync::atomic::Ordering::SeqCst) as usize;
        let is_eos = self.buffer.is_eos() && pos >= self.buffer.total_written();
        Ok(BOOL(if is_eos { 1 } else { 0 }))
    }

    fn Read(&self, pb: *mut u8, cb: u32, pcbread: *mut u32) -> Result<()> {
        if pb.is_null() { return Err(E_POINTER.into()); }
        let buf = unsafe { std::slice::from_raw_parts_mut(pb, cb as usize) };
        let pos = self.pos.load(std::sync::atomic::Ordering::SeqCst) as usize;
        let bytes_read = self.buffer.read_at(pos, buf);
        
        
        self.pos.fetch_add(bytes_read as u64, std::sync::atomic::Ordering::SeqCst);
        if !pcbread.is_null() {
            unsafe { *pcbread = bytes_read as u32; }
        }
        Ok(())
    }

    fn BeginRead(
        &self,
        pb: *mut u8,
        cb: u32,
        pcallback: Option<&IMFAsyncCallback>,
        punkstate: Option<&IUnknown>,
    ) -> Result<()> {
        if pb.is_null() { return Err(E_POINTER.into()); }
        let buf = unsafe { std::slice::from_raw_parts_mut(pb, cb as usize) };
        let pos = self.pos.load(std::sync::atomic::Ordering::SeqCst) as usize;
        let bytes_read = self.buffer.read_at(pos, buf);
        self.pos.fetch_add(bytes_read as u64, std::sync::atomic::Ordering::SeqCst);
        self.last_read_count.store(bytes_read as u32, std::sync::atomic::Ordering::SeqCst);

        if let Some(cb_obj) = pcallback {
            unsafe {
                let result_obj = MFCreateAsyncResult(None, cb_obj, punkstate)?;
                MFInvokeCallback(&result_obj)?;
            }
        }
        Ok(())
    }

    fn EndRead(&self, _presult: Option<&IMFAsyncResult>) -> Result<u32> {
        let bytes_read = self.last_read_count.load(std::sync::atomic::Ordering::SeqCst);
        Ok(bytes_read)
    }

    fn Write(&self, _pb: *const u8, _cb: u32) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn BeginWrite(
        &self,
        _pb: *const u8,
        _cb: u32,
        _pcallback: Option<&IMFAsyncCallback>,
        _punkstate: Option<&IUnknown>,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndWrite(&self, _presult: Option<&IMFAsyncResult>) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn Seek(
        &self,
        seekorigin: MFBYTESTREAM_SEEK_ORIGIN,
        llseekoffset: i64,
        _dwseekflags: u32,
    ) -> Result<u64> {
        let current = self.pos.load(std::sync::atomic::Ordering::SeqCst) as i64;

        let new_pos = match seekorigin.0 {
            0 => llseekoffset, // mso_begin
            1 => current + llseekoffset, // mso_current
            _ => {
                let length = {
                    let tl = self.buffer.get_total_len();
                    if tl > 0 { tl as i64 } else { self.buffer.total_written() as i64 }
                };
                length + llseekoffset
            }
        };

        if new_pos < 0 {
            return Err(E_INVALIDARG.into());
        }

        let new_pos = new_pos as u64;
        self.pos.store(new_pos, std::sync::atomic::Ordering::SeqCst);
        Ok(new_pos)
    }

    fn Flush(&self) -> Result<()> {
        Ok(())
    }

    fn Close(&self) -> Result<()> {
        Ok(())
    }
}
