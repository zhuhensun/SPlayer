use cpal::{BufferSize, SupportedBufferSize};

// Match Android's deep software queue: Pulse/PipeWire server scheduling adds
// the same kind of decode→callback jitter as the Android scheduler, and an
// underrunning pulse stream accumulates extra sink latency it never gives
// back (the mysterious multi-second delay). Absorb the jitter in our own
// preallocated ring — NOT by requesting bigger device buffers below.
pub(in crate::output) const DEFAULT_QUEUE_BLOCKS: usize = 48;

pub(in crate::output) fn stable_buffer_size(
    _sample_rate: u32,
    _supported: &SupportedBufferSize,
) -> BufferSize {
    // Pulse/PipeWire/ALSA backends are more reliable when CPAL/device policy
    // owns the hardware period size. Keep explicit fixed buffers for platforms
    // where we have a concrete low-latency reason to request them.
    BufferSize::Default
}

/// Identity of the current default sink, taken straight from the device CPAL
/// already resolved. On PulseAudio/PipeWire this is the sink name, which the
/// server substituted for `@DEFAULT_SINK@` — so the periodic default-device
/// poll can compare it directly instead of hashing the whole device list.
/// That list costs a `list_sinks` + `list_sources` roundtrip pair on
/// PulseAudio and, worse, a `snd_pcm_open()` per device on ALSA. Hosts whose
/// default device is a fixed alias return `None` and keep that fallback.
pub(in crate::output) fn default_output_id(device: &cpal::Device) -> Option<String> {
    super::resolved_default_output_device_id(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_buffer_size_uses_device_default_on_linux() {
        let buffer = stable_buffer_size(
            48_000,
            &SupportedBufferSize::Range {
                min: 128,
                max: 2_048,
            },
        );

        assert_eq!(buffer, BufferSize::Default);
    }
}
