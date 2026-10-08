use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use facetty_ascii::Image;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset1280x720, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaType, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::capture::fit;

const DENIED: &str = "camera access is off for this terminal app; allow it in System Settings > Privacy & Security > Camera";

pub fn list() -> Vec<String> {
    devices()
        .iter()
        .map(|d| unsafe { d.localizedName() }.to_string())
        .collect()
}

pub fn run(device: &str, stop: &AtomicBool, mut on_frame: impl FnMut(Image)) -> Result<()> {
    let video = unsafe { AVMediaTypeVideo }.context("AVFoundation has no video media type")?;
    let devices = devices();
    let names: Vec<String> = devices
        .iter()
        .map(|d| unsafe { d.localizedName() }.to_string())
        .collect();
    let index = super::pick(&names, device)?;
    if !wait_for_access(video, stop)? {
        return Ok(());
    }

    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&devices[index]) }
        .map_err(|e| anyhow!("opening {}: {}", names[index], e.localizedDescription()))?;
    let session = unsafe { AVCaptureSession::new() };
    let (frames, received) = mpsc::sync_channel(1);
    let delegate = Delegate::new(frames);
    let queue = DispatchQueue::new("facetty.camera", None);
    let output = unsafe { AVCaptureVideoDataOutput::new() };
    let format_key: &NSString = unsafe { kCVPixelBufferPixelFormatTypeKey }.as_ref();
    let bgra = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[format_key], &[&bgra]);
    unsafe {
        if !session.canAddInput(&input) {
            bail!("{} cannot be used for capture", names[index]);
        }
        session.addInput(&input);
        if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
            session.setSessionPreset(AVCaptureSessionPreset1280x720);
        }
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        if !session.canAddOutput(&output) {
            bail!("{} cannot deliver video frames", names[index]);
        }
        session.addOutput(&output);
        session.startRunning();
    }

    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(image) => on_frame(image),
            Err(RecvTimeoutError::Timeout) if unsafe { session.isRunning() } => {}
            Err(_) => break Err(anyhow!("{} stopped", names[index])),
        }
    };
    unsafe { session.stopRunning() };
    result
}

// AVCaptureDeviceDiscoverySession would need a device type per kind of camera,
// and the one for external cameras only exists from macOS 14.
#[allow(deprecated)]
fn devices() -> Vec<Retained<AVCaptureDevice>> {
    match unsafe { AVMediaTypeVideo } {
        Some(video) => unsafe { AVCaptureDevice::devicesWithMediaType(video) }.to_vec(),
        None => Vec::new(),
    }
}

/// Asks for camera access if the user has not been asked yet. False means
/// `stop` was set while the prompt was up.
fn wait_for_access(video: &AVMediaType, stop: &AtomicBool) -> Result<bool> {
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) } {
        AVAuthorizationStatus::Authorized => return Ok(true),
        AVAuthorizationStatus::NotDetermined => {}
        _ => bail!(DENIED),
    }
    let (answer, answered) = mpsc::channel();
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = answer.send(granted.as_bool());
    });
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &handler) };
    loop {
        match answered.recv_timeout(Duration::from_millis(100)) {
            Ok(true) => return Ok(true),
            Ok(false) | Err(RecvTimeoutError::Disconnected) => bail!(DENIED),
            Err(RecvTimeoutError::Timeout) if stop.load(Ordering::Relaxed) => return Ok(false),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

struct Ivars {
    frames: SyncSender<Image>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FacettyCameraDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Delegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if let Some(image) = to_image(sample) {
                let _ = self.ivars().frames.try_send(image);
            }
        }
    }
);

impl Delegate {
    fn new(frames: SyncSender<Image>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { frames });
        unsafe { msg_send![super(this), init] }
    }
}

fn to_image(sample: &CMSampleBuffer) -> Option<Image> {
    let pixels = unsafe { sample.image_buffer() }?;
    if CVPixelBufferGetPixelFormatType(&pixels) != kCVPixelFormatType_32BGRA
        || unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) }
            != kCVReturnSuccess
    {
        return None;
    }
    let base = CVPixelBufferGetBaseAddress(&pixels).cast::<u8>();
    let (width, height) = (
        CVPixelBufferGetWidth(&pixels),
        CVPixelBufferGetHeight(&pixels),
    );
    let stride = CVPixelBufferGetBytesPerRow(&pixels);
    let image = (!base.is_null()).then(|| {
        let data = unsafe { std::slice::from_raw_parts(base, stride * height) };
        fit(width, height, |x, y| {
            let i = y * stride + x * 4;
            [data[i + 2], data[i + 1], data[i]]
        })
    });
    unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
    image
}
