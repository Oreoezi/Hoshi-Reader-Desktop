use std::path::Path;

pub type Pcm = (Vec<i16>, u32, usize);

#[cfg(windows)]
pub fn decode(path: &Path, start: f64, end: f64, mime: &str) -> Option<Pcm> {
    with_media_foundation(|| unsafe { read_media_foundation(path, start, end, mime) }).ok()
}

#[cfg(windows)]
pub fn with_media_foundation<T>(
    read: impl FnOnce() -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    use windows::Win32::Media::MediaFoundation::{
        MF_VERSION, MFSTARTUP_LITE, MFShutdown, MFStartup,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = MFStartup(MF_VERSION, MFSTARTUP_LITE).and_then(|_| {
            let result = read();
            let _ = MFShutdown();
            result
        });
        if com {
            CoUninitialize();
        }
        result
    }
}

#[cfg(windows)]
unsafe fn read_media_foundation(
    path: &Path,
    start: f64,
    end: f64,
    mime: &str,
) -> windows::core::Result<Pcm> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::core::GUID;

    unsafe {
        let reader = open_reader(path, mime, false)?;
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let format = reader.GetCurrentMediaType(audio)?;
        let mut rate = format.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
        let channels = format.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? as usize;

        reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from((start * 1e7) as i64))?;

        let mut samples = Vec::new();
        loop {
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            reader.ReadSample(
                audio,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else {
                continue;
            };
            let time = timestamp as f64 / 1e7;
            if time > end {
                break;
            }
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data = std::ptr::null_mut();
            let mut len = 0;
            buffer.Lock(&mut data, None, Some(&mut len))?;
            let chunk = std::slice::from_raw_parts(data as *const i16, len as usize / 2);
            if samples.is_empty()
                && let Ok(duration) = sample.GetSampleDuration()
                && duration > 0
            {
                rate = ((chunk.len() / channels) as f64 * 1e7 / duration as f64).round() as u32;
            }
            let skip = ((start - time).max(0.0) * rate as f64) as usize * channels;
            samples.extend_from_slice(&chunk[skip.min(chunk.len())..]);
            buffer.Unlock()?;
        }
        samples.truncate(((end - start) * rate as f64) as usize * channels);
        Ok((samples, rate, channels))
    }
}

#[cfg(target_os = "macos")]
pub fn decode(path: &Path, start: f64, end: f64, _mime: &str) -> Option<Pcm> {
    use std::ffi::{CString, c_char, c_void};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn hoshi_decode_audio(
            path: *const c_char,
            start: f64,
            end: f64,
            context: *mut c_void,
            callback: extern "C" fn(*mut c_void, *const i16, usize, u32, u32),
        );
    }

    extern "C" fn receive(
        context: *mut c_void,
        data: *const i16,
        count: usize,
        rate: u32,
        channels: u32,
    ) {
        let pcm = unsafe { &mut *(context as *mut Option<Pcm>) };
        *pcm = Some((
            unsafe { std::slice::from_raw_parts(data, count) }.to_vec(),
            rate,
            channels as usize,
        ));
    }

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut pcm: Option<Pcm> = None;
    unsafe {
        hoshi_decode_audio(
            path.as_ptr(),
            start,
            end,
            &mut pcm as *mut Option<Pcm> as *mut c_void,
            receive,
        )
    };
    pcm
}

#[cfg(target_os = "linux")]
pub fn decode(path: &Path, start: f64, end: f64, _mime: &str) -> Option<Pcm> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{Decoder, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::core::units::Time;

    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(std::ffi::OsStr::to_str) {
        hint.with_extension(extension);
    }
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|track| {
        track.codec_params.channels.is_some() && track.codec_params.sample_rate.is_some()
    })?;
    let track_id = track.id;
    let time_base = track.codec_params.time_base?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .ok()?;
    if format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(start),
                track_id: Some(track_id),
            },
        )
        .is_ok()
    {
        decoder.reset();
    }
    let mut samples = Vec::new();
    let mut sample_rate = None;
    let mut channels = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let packet_start = packet.ts() as f64 * time_base.numer as f64 / time_base.denom as f64;
        if packet_start >= end {
            break;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let rate = decoded.spec().rate;
        let channel_count = decoded.spec().channels.count();
        if channel_count == 0 {
            return None;
        }
        if sample_rate.is_some_and(|previous| previous != rate)
            || channels.is_some_and(|previous| previous != channel_count)
        {
            return None;
        }
        sample_rate = Some(rate);
        channels = Some(channel_count);

        let first_frame = ((start - packet_start).max(0.0) * rate as f64).ceil() as usize;
        let last_frame = ((end - packet_start).max(0.0) * rate as f64)
            .ceil()
            .min(decoded.frames() as f64) as usize;
        if first_frame >= last_frame {
            continue;
        }
        let mut buffer = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let interleaved = buffer.samples();
        let first_sample = first_frame * channel_count;
        let last_sample = (last_frame * channel_count).min(interleaved.len());
        samples.extend_from_slice(interleaved.get(first_sample..last_sample)?);
    }

    Some((samples, sample_rate?, channels?))
}

#[cfg(windows)]
unsafe fn open_reader(
    path: &Path,
    mime: &str,
    mono: bool,
) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFSourceReader> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::core::{HSTRING, Interface};

    unsafe {
        let stream = MFCreateFile(
            MF_ACCESSMODE_READ,
            MF_OPENMODE_FAIL_IF_NOT_EXIST,
            MF_FILEFLAGS_NONE,
            &HSTRING::from(path.as_os_str()),
        )?;
        stream
            .cast::<IMFAttributes>()?
            .SetString(&MF_BYTESTREAM_CONTENT_TYPE, &HSTRING::from(mime))?;
        let reader = MFCreateSourceReaderFromByteStream(&stream, None)?;
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(audio, true)?;

        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        if mono {
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 16000)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?;
            output.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 32000)?;
        }
        reader.SetCurrentMediaType(audio, None, &output)?;
        Ok(reader)
    }
}

#[cfg(windows)]
pub struct MonoReader {
    reader: windows::Win32::Media::MediaFoundation::IMFSourceReader,
    samples: Vec<i16>,
    start: usize,
    ended: bool,
    anchored: bool,
}

#[cfg(windows)]
impl MonoReader {
    pub const RATE: usize = 16000;

    pub fn duration(path: &Path, mime: &str) -> windows::core::Result<f64> {
        use windows::Win32::Media::MediaFoundation::*;

        unsafe {
            let reader = open_reader(path, mime, true)?;
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
            Ok(u64::try_from(&duration)? as f64 / 1e7)
        }
    }

    pub fn open(path: &Path, mime: &str, from: f64) -> windows::core::Result<Self> {
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows::core::GUID;

        unsafe {
            let reader = open_reader(path, mime, true)?;
            reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from((from * 1e7) as i64))?;
            Ok(MonoReader {
                reader,
                samples: Vec::new(),
                start: 0,
                ended: false,
                anchored: false,
            })
        }
    }

    pub fn read(&mut self, index: usize, output: &mut [f32]) -> windows::core::Result<()> {
        use windows::Win32::Media::MediaFoundation::*;

        let skip = index.saturating_sub(self.start).min(self.samples.len());
        self.samples.drain(..skip);
        self.start += skip;
        while !self.ended && self.start + self.samples.len() < index + output.len() {
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )?;
            }
            self.ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            let Some(sample) = sample else {
                continue;
            };
            if !self.anchored {
                self.anchored = true;
                self.start = (timestamp as f64 * Self::RATE as f64 / 1e7)
                    .round()
                    .max(0.0) as usize;
            }
            unsafe {
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut data = std::ptr::null_mut();
                let mut len = 0;
                buffer.Lock(&mut data, None, Some(&mut len))?;
                let chunk = std::slice::from_raw_parts(data.cast::<i16>(), len as usize / 2);
                self.samples.extend_from_slice(chunk);
                buffer.Unlock()?;
            }
        }
        for (position, value) in (index..).zip(output.iter_mut()) {
            *value = position
                .checked_sub(self.start)
                .and_then(|offset| self.samples.get(offset))
                .map_or(0.0, |&sample| sample as f32);
        }
        Ok(())
    }
}
