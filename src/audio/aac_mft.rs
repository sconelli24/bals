use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::info;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_USER_DATA, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFShutdown, MFStartup, MFTEnumEx,
    MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::audio::EncodedAudioPacket;
use crate::config::AudioConfig;

/// Encoder AAC-LC nativo de Windows Media Foundation (sin dependencias externas).
///
/// Recibe PCM `i16` estéreo a 48 kHz y emite paquetes AAC-LC raw (payload type 0)
/// listos para passthrough directo al contenedor MP4.
pub struct AacEncoder {
    mft: IMFTransform,
    /// Blob `MF_MT_USER_DATA` (HEAACWAVEINFO + AudioSpecificConfig) requerido por `IMFSinkWriter`
    /// para escribir el átomo `esds` en el MP4.
    pub user_data: Vec<u8>,
    pub bitrate_bps: u32,
    pub sample_rate: u32,
    pub channels: u16,
    mft_provides_output_samples: bool,
    output_buffer_size: u32,
    reusable_in_sample: Option<(IMFSample, IMFMediaBuffer, usize)>,
    reusable_out_sample: Option<IMFSample>,
    mf_started: bool,
}

impl AacEncoder {
    pub fn new(config: AudioConfig) -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (AAC) falló")?;

        match Self::init_inner(config) {
            Ok(enc) => Ok(enc),
            Err(err) => {
                unsafe {
                    let _ = MFShutdown();
                }
                Err(err)
            }
        }
    }

    fn init_inner(config: AudioConfig) -> Result<Self> {
        let mft = activate_aac_mft()?;

        // El AAC MFT de Microsoft soporta 96, 128, 160 y 192 kbps (en bytes/s: 12000..24000).
        let avg_bytes_per_sec = match config.bitrate_bps {
            ..=112_000 => 12_000u32,
            112_001..=144_000 => 16_000u32,
            144_001..=176_000 => 20_000u32,
            _ => 24_000u32,
        };
        let actual_bitrate_bps = avg_bytes_per_sec * 8;

        // 1. Configurar tipo de ENTRADA: PCM 16-bit signed, 48 kHz, estéreo.
        let block_align = config.channels as u32 * 2;
        let pcm_bytes_per_sec = config.sample_rate * block_align;

        let input_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType (PCM in) falló")?;
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, config.sample_rate)?;
            input_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, config.channels as u32)?;
            input_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
            input_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, pcm_bytes_per_sec)?;
            mft.SetInputType(0, &input_type, 0)
        }
        .context("AAC MFT SetInputType (PCM 16-bit) falló")?;

        // 2. Configurar tipo de SALIDA: AAC-LC raw (payload type 0) para contenedor MP4.
        let output_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType (AAC out) falló")?;
        unsafe {
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, config.sample_rate)?;
            output_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, config.channels as u32)?;
            output_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes_per_sec)?;
            output_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            output_type.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            mft.SetOutputType(0, &output_type, 0)
        }
        .context("AAC MFT SetOutputType (AAC-LC) falló")?;

        // 3. Extraer MF_MT_USER_DATA (HEAACWAVEINFO + AudioSpecificConfig) del tipo de salida activo.
        let current_out = unsafe { mft.GetOutputCurrentType(0) }
            .context("AAC MFT GetOutputCurrentType falló")?;
        let user_data = read_blob_attribute(&current_out, &MF_MT_USER_DATA)
            .context("No se pudo leer MF_MT_USER_DATA del AAC MFT")?;

        let out_info = unsafe { mft.GetOutputStreamInfo(0) }
            .context("AAC MFT GetOutputStreamInfo falló")?;
        let mft_provides_output_samples = (out_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
            != 0;
        let output_buffer_size = out_info.cbSize.max(4096);

        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        info!(
            sample_rate = config.sample_rate,
            channels = config.channels,
            bitrate_kbps = actual_bitrate_bps / 1000,
            user_data_len = user_data.len(),
            "Encoder AAC-LC Media Foundation iniciado"
        );

        Ok(Self {
            mft,
            user_data,
            bitrate_bps: actual_bitrate_bps,
            sample_rate: config.sample_rate,
            channels: config.channels,
            mft_provides_output_samples,
            output_buffer_size,
            reusable_in_sample: None,
            reusable_out_sample: None,
            mf_started: true,
        })
    }

    /// Codifica muestras PCM `i16` intercaladas (estéreo) y devuelve los paquetes AAC listos.
    pub fn encode_pcm_i16(
        &mut self,
        pcm_samples: &[i16],
        pts_100ns: i64,
    ) -> Result<Vec<EncodedAudioPacket>> {
        if pcm_samples.is_empty() {
            return Ok(Vec::new());
        }

        let frames = pcm_samples.len() / self.channels as usize;
        let duration_100ns = (frames as i64 * 10_000_000) / self.sample_rate as i64;
        let byte_len = std::mem::size_of_val(pcm_samples);

        let (sample, buffer, cap) = match self.reusable_in_sample.take() {
            Some((s, b, c)) if c >= byte_len => (s, b, c),
            _ => {
                let s = unsafe { MFCreateSample() }.context("MFCreateSample (PCM) falló")?;
                let b: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(byte_len as u32) }
                    .context("MFCreateMemoryBuffer (PCM) falló")?;
                unsafe { s.AddBuffer(&b)? };
                (s, b, byte_len)
            }
        };

        let mut dst_ptr: *mut u8 = std::ptr::null_mut();
        unsafe { buffer.Lock(&mut dst_ptr, None, None) }?;
        let input_res = unsafe {
            std::ptr::copy_nonoverlapping(pcm_samples.as_ptr() as *const u8, dst_ptr, byte_len);
            buffer.Unlock()?;
            buffer.SetCurrentLength(byte_len as u32)?;
            sample.SetSampleTime(pts_100ns)?;
            sample.SetSampleDuration(duration_100ns)?;
            self.mft.ProcessInput(0, &sample, 0)
        };

        let out_res = input_res
            .context("AAC MFT ProcessInput falló")
            .and_then(|()| self.drain_outputs());
        self.reusable_in_sample = Some((sample, buffer, cap));
        out_res
    }

    pub fn flush(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        self.drain_outputs()
    }

    fn drain_outputs(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        let mut packets = Vec::new();
        loop {
            let provided_sample = if self.mft_provides_output_samples {
                None
            } else if let Some(cached) = self.reusable_out_sample.take() {
                if let Ok(buf) = unsafe { cached.GetBufferByIndex(0) } {
                    let _ = unsafe { buf.SetCurrentLength(0) };
                }
                Some(cached)
            } else {
                let sample = unsafe { MFCreateSample() }?;
                let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }?;
                unsafe { sample.AddBuffer(&buffer) }?;
                Some(sample)
            };

            let mut output_buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;

            let hr = unsafe { self.mft.ProcessOutput(0, &mut output_buffers, &mut status) };
            let out_sample = unsafe { ManuallyDrop::take(&mut output_buffers[0].pSample) };
            let _out_events = unsafe { ManuallyDrop::take(&mut output_buffers[0].pEvents) };

            match hr {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    if !self.mft_provides_output_samples {
                        self.reusable_out_sample = out_sample;
                    }
                    break;
                }
                Err(err) => return Err(err).context("AAC MFT ProcessOutput falló"),
            }

            let Some(sample) = out_sample else {
                break;
            };

            let pts_100ns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
            let duration_100ns = unsafe { sample.GetSampleDuration() }
                .unwrap_or((1024 * 10_000_000) / self.sample_rate as i64);

            let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }?;
            let data: Arc<[u8]> = if !ptr.is_null() && len > 0 {
                Arc::from(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
            } else {
                Arc::from([])
            };
            unsafe {
                let _ = buffer.Unlock();
            }
            drop(buffer);

            if !self.mft_provides_output_samples {
                self.reusable_out_sample = Some(sample);
            }

            if !data.is_empty() {
                packets.push(EncodedAudioPacket {
                    data,
                    pts_100ns,
                    duration_100ns,
                });
            }
        }
        Ok(packets)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        if self.mf_started {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

fn activate_aac_mft() -> Result<IMFTransform> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };

    let mut raw_activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_info),
            &mut raw_activates,
            &mut count,
        )
    }
    .context("MFTEnumEx (AAC) falló")?;

    if count == 0 || raw_activates.is_null() {
        bail!("No se encontró ningún encoder AAC en Media Foundation");
    }

    let mut chosen: Option<IMFTransform> = None;
    let slice = unsafe { std::slice::from_raw_parts_mut(raw_activates, count as usize) };
    for slot in slice.iter_mut() {
        if let Some(activate) = slot.take() {
            if chosen.is_none() {
                if let Ok(mft) = unsafe { activate.ActivateObject::<IMFTransform>() } {
                    chosen = Some(mft);
                }
            }
        }
    }

    unsafe {
        CoTaskMemFree(Some(raw_activates as *const _));
    }

    chosen.context("No se pudo activar el encoder AAC de Media Foundation")
}

fn read_blob_attribute(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    guid: &windows::core::GUID,
) -> Result<Vec<u8>> {
    let size = unsafe { media_type.GetBlobSize(guid) }?;
    let mut buf = vec![0u8; size as usize];
    unsafe { media_type.GetBlob(guid, &mut buf, None) }?;
    Ok(buf)
}
