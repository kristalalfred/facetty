use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use facetty_ascii::Image;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{Interface, PWSTR};

use crate::capture::fit;

pub fn list() -> Vec<String> {
    with_media_foundation(|| Ok(sources()?.iter().map(name).collect())).unwrap_or_default()
}

pub fn run(device: &str, stop: &AtomicBool, mut on_frame: impl FnMut(Image)) -> Result<()> {
    with_media_foundation(|| {
        let sources = sources()?;
        let names: Vec<String> = sources.iter().map(name).collect();
        let index = super::pick(&names, device)?;
        let name = &names[index];
        let source: IMFMediaSource = unsafe { sources[index].ActivateObject() }
            .with_context(|| format!("opening {name}"))?;
        let result = read(&source, name, stop, &mut on_frame);
        let _ = unsafe { sources[index].ShutdownObject() };
        result
    })
}

fn read(
    source: &IMFMediaSource,
    name: &str,
    stop: &AtomicBool,
    on_frame: &mut impl FnMut(Image),
) -> Result<()> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let attributes = attributes()?;
    let reader = unsafe {
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        MFCreateSourceReaderFromMediaSource(source, &attributes)?
    };
    let media = unsafe {
        let wanted = MFCreateMediaType()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader
            .SetCurrentMediaType(stream, None, &wanted)
            .with_context(|| format!("{name} cannot deliver RGB video"))?;
        reader.GetCurrentMediaType(stream)?
    };
    let size = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE)? };
    let (width, height) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
    let stride = match unsafe { media.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
        Ok(stride) => stride as i32,
        Err(_) => unsafe {
            MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, width as u32)?
        },
    };

    let stopped = (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32;
    while !stop.load(Ordering::Relaxed) {
        let mut flags = 0u32;
        let mut sample = None;
        unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)) }
            .with_context(|| format!("reading {name}"))?;
        if flags & stopped != 0 {
            bail!("{name} stopped");
        }
        if let Some(sample) = sample {
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            on_frame(to_image(&buffer, width, height, stride as isize)?);
        }
    }
    Ok(())
}

/// Reads a BGRX frame. A negative stride means the rows are stored bottom-up.
fn to_image(buffer: &IMFMediaBuffer, width: usize, height: usize, stride: isize) -> Result<Image> {
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer>() {
        let (mut first_row, mut pitch) = (ptr::null_mut(), 0);
        unsafe { buffer.Lock2D(&mut first_row, &mut pitch)? };
        let image = bgrx(first_row, pitch as isize, width, height);
        let _ = unsafe { buffer.Unlock2D() };
        return Ok(image);
    }
    let (mut start, mut len) = (ptr::null_mut(), 0);
    unsafe { buffer.Lock(&mut start, None, Some(&mut len))? };
    let image = if (len as usize) < stride.unsigned_abs() * height {
        Err(anyhow::anyhow!("short video frame"))
    } else {
        let first_row = if stride < 0 {
            unsafe { start.offset(-stride * (height as isize - 1)) }
        } else {
            start
        };
        Ok(bgrx(first_row, stride, width, height))
    };
    let _ = unsafe { buffer.Unlock() };
    image
}

fn bgrx(first_row: *const u8, pitch: isize, width: usize, height: usize) -> Image {
    fit(width, height, |x, y| {
        let p = unsafe { first_row.offset(y as isize * pitch + x as isize * 4) };
        unsafe { [*p.add(2), *p.add(1), *p] }
    })
}

fn with_media_foundation<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let result = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
        .context("starting Media Foundation")
        .and_then(|()| {
            let result = f();
            let _ = unsafe { MFShutdown() };
            result
        });
    if com.is_ok() {
        unsafe { CoUninitialize() };
    }
    result
}

fn attributes() -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    attributes.context("creating Media Foundation attributes")
}

fn sources() -> Result<Vec<IMFActivate>> {
    let attributes = attributes()?;
    let (mut array, mut count) = (ptr::null_mut(), 0);
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        MFEnumDeviceSources(&attributes, &mut array, &mut count)?;
    }
    if array.is_null() {
        return Ok(Vec::new());
    }
    let sources = (0..count as usize)
        .filter_map(|i| unsafe { (*array.add(i)).take() })
        .collect();
    unsafe { CoTaskMemFree(Some(array as *const _)) };
    Ok(sources)
}

fn name(source: &IMFActivate) -> String {
    let (mut text, mut len) = (PWSTR::null(), 0);
    if unsafe {
        source.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut text, &mut len)
    }
    .is_err()
    {
        return "camera".into();
    }
    let name = unsafe { text.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
    name
}
