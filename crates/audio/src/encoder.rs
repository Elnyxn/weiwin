use opus::{Application, Bandwidth, Bitrate, Channels, Encoder, Signal};

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SAMPLES: usize = 320;
pub const CHUNK_SAMPLES: usize = 1920;
pub const HEADER: &[u8; 16] = b"#!OPUS_RAW_V1\x02\x01\x00";

pub struct OpusStream {
    encoder: Encoder,
    first: bool,
    packet: [u8; 1275],
}

impl OpusStream {
    pub fn new() -> Result<Self, opus::Error> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::Bits(24_000))?;
        encoder.set_complexity(5)?;
        encoder.set_bandwidth(Bandwidth::Wideband)?;
        encoder.set_signal(Signal::Voice)?;
        encoder.set_dtx(false)?;
        Ok(Self {
            encoder,
            first: true,
            packet: [0; 1275],
        })
    }

    /// 20 ms Opus frames, prefixed by little-endian packet lengths.
    pub fn encode(
        &mut self,
        samples: &[i16],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        if samples.is_empty() || samples.len() > CHUNK_SAMPLES || samples.len() % FRAME_SAMPLES != 0
        {
            return Err("PCM chunk must contain 1..6 complete 20 ms frames".into());
        }
        let mut output = Vec::with_capacity(samples.len() / FRAME_SAMPLES * 96 + 16);
        if self.first {
            output.extend_from_slice(HEADER);
        }
        for frame in samples.chunks_exact(FRAME_SAMPLES) {
            let size = self.encoder.encode(frame, &mut self.packet)?;
            output.extend_from_slice(&(size as u16).to_le_bytes());
            output.extend_from_slice(&self.packet[..size]);
        }
        self.first = false;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_container_has_six_decodable_frames_and_one_header() {
        let pcm: Vec<i16> = (0..CHUNK_SAMPLES)
            .map(|i| ((i as f32 * 0.17).sin() * 6000.0) as i16)
            .collect();
        let mut stream = OpusStream::new().unwrap();
        let data = stream.encode(&pcm).unwrap();
        assert!(data.starts_with(HEADER));
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
        let mut offset = 16;
        let mut samples = 0;
        let mut decoded = [0_i16; FRAME_SAMPLES];
        while offset < data.len() {
            let size = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            samples += decoder
                .decode(&data[offset..offset + size], &mut decoded, false)
                .unwrap();
            offset += size;
        }
        assert_eq!(samples, CHUNK_SAMPLES);
        assert!(!stream.encode(&pcm).unwrap().starts_with(HEADER));
    }
}
