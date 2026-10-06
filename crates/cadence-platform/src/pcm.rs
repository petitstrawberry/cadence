//! Keep malformed floating point audio from reaching native output drivers.

pub fn normalized(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(any(target_os = "scarlet", test))]
pub fn s16le(sample: f32) -> [u8; 2] {
    let sample = normalized(sample);
    let scale = if sample < 0.0 { 32768.0 } else { 32767.0 };
    ((sample * scale).round() as i16).to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16le_preserves_endpoints_and_byte_order() {
        assert_eq!(s16le(-1.0), [0x00, 0x80]);
        assert_eq!(s16le(1.0), [0xff, 0x7f]);
        assert_eq!(s16le(0.5), [0x00, 0x40]);
        assert_eq!(s16le(-0.5), [0x00, 0xc0]);
        assert_eq!(s16le(0.0), [0, 0]);
    }

    #[test]
    fn clips_overloads_and_silences_nonfinite_samples() {
        assert_eq!(s16le(-4.0), s16le(-1.0));
        assert_eq!(s16le(4.0), s16le(1.0));
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(s16le(sample), [0, 0]);
        }
    }
}
