//! Sequence and picture parameter sets.
//!
//! Constrained Baseline, one reference frame, frame numbers of 16 bits and
//! picture order taken from decode order (`pic_order_cnt_type` 2, since there
//! are no B frames to reorder). The VUI says the colours are BT.709 limited
//! range and that nothing is reordered, which is what lets a decoder show
//! each frame the moment it arrives.

use crate::bits::BitWriter;
use crate::nal;

/// `log2_max_frame_num_minus4`: frame numbers wrap at 2^16.
pub(crate) const FRAME_NUM_BITS: u32 = 16;

/// One row of Table A-1: level, macroblocks per second, frame size in
/// macroblocks, decoded picture buffer size in macroblocks.
const LEVELS: [(u8, u32, u32, u32); 19] = [
    (10, 1_485, 99, 396),
    (11, 3_000, 396, 900),
    (12, 6_000, 396, 2_376),
    (13, 11_880, 396, 2_376),
    (20, 11_880, 396, 2_376),
    (21, 19_800, 792, 4_752),
    (22, 20_250, 1_620, 8_100),
    (30, 40_500, 1_620, 8_100),
    (31, 108_000, 3_600, 18_000),
    (32, 216_000, 5_120, 20_480),
    (40, 245_000, 8_192, 32_768),
    (41, 245_000, 8_192, 32_768),
    (42, 522_000, 8_704, 34_816),
    (50, 589_824, 22_080, 110_400),
    (51, 983_040, 36_864, 184_320),
    (52, 2_073_600, 36_864, 184_320),
    (60, 4_177_920, 139_264, 696_320),
    (61, 8_355_840, 139_264, 696_320),
    (62, 16_711_680, 139_264, 696_320),
];

/// The lowest level a stream this size and rate fits, or `None` if none does.
///
/// Level 4.0 is bumped to 4.1, whose limits are the same but whose bitrate
/// ceiling is two and a half times higher: a constant-quality screen recording
/// spikes on a full repaint, and 1080p is the size that lands on 4.0.
pub(crate) fn level(mb_w: u32, mb_h: u32, frame_rate: u32) -> Option<u8> {
    let frame = mb_w * mb_h;
    let per_second = frame.saturating_mul(frame_rate.max(1));
    LEVELS
        .iter()
        .find(|&&(_, mbps, fs, dpb)| {
            // Neither side may exceed sqrt(8 * MaxFS) macroblocks.
            let side = mb_w.max(mb_h);
            frame <= fs && per_second <= mbps && frame <= dpb && side * side <= 8 * fs
        })
        .map(|&(level, ..)| if level == 40 { 41 } else { level })
}

/// The SPS as a NAL unit.
pub(crate) fn sps(width: u32, height: u32, mb_w: u32, mb_h: u32, level: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(8, 66); // profile_idc: Baseline
    // constraint_set0 and set1: Constrained Baseline.
    w.put(8, 0b1100_0000);
    w.put(8, u32::from(level));
    w.ue(0); // seq_parameter_set_id
    w.ue(FRAME_NUM_BITS - 4);
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.flag(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(mb_w - 1);
    w.ue(mb_h - 1);
    w.flag(true); // frame_mbs_only_flag
    w.flag(true); // direct_8x8_inference_flag
    let (crop_right, crop_bottom) = (mb_w * 16 - width, mb_h * 16 - height);
    let cropping = crop_right > 0 || crop_bottom > 0;
    w.flag(cropping);
    if cropping {
        // In chroma units: two luma samples each, for 4:2:0 frames.
        w.ue(0);
        w.ue(crop_right / 2);
        w.ue(0);
        w.ue(crop_bottom / 2);
    }
    w.flag(true); // vui_parameters_present_flag
    vui(&mut w);
    w.trailing();
    nal::wrap(3, nal::SPS, &w.finish())
}

fn vui(w: &mut BitWriter) {
    w.flag(true); // aspect_ratio_info_present_flag
    w.put(8, 1); // square pixels
    w.flag(false); // overscan_info_present_flag
    w.flag(true); // video_signal_type_present_flag
    w.put(3, 5); // video_format: unspecified
    w.flag(false); // video_full_range_flag: limited range
    w.flag(true); // colour_description_present_flag
    w.put(8, 1); // colour_primaries: BT.709
    w.put(8, 1); // transfer_characteristics: BT.709
    w.put(8, 1); // matrix_coefficients: BT.709
    w.flag(false); // chroma_loc_info_present_flag
    w.flag(false); // timing_info_present_flag: the container has the times
    w.flag(false); // nal_hrd_parameters_present_flag
    w.flag(false); // vcl_hrd_parameters_present_flag
    w.flag(false); // pic_struct_present_flag
    w.flag(true); // bitstream_restriction_flag
    w.flag(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom
    w.ue(1); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames: show each frame as it is decoded
    w.ue(1); // max_dec_frame_buffering
}

/// The PPS as a NAL unit.
pub(crate) fn pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.flag(false); // entropy_coding_mode_flag: CAVLC
    w.flag(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.flag(false); // weighted_pred_flag
    w.put(2, 0); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.flag(true); // deblocking_filter_control_present_flag
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // redundant_pic_cnt_present_flag
    w.trailing();
    nal::wrap(3, nal::PPS, &w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_sizes_get_the_expected_levels() {
        // 1080p30 is 8160 macroblocks: 4.0's size, bumped to 4.1.
        assert_eq!(level(120, 68, 30), Some(41));
        assert_eq!(level(80, 45, 30), Some(31)); // 720p30
        assert_eq!(level(240, 135, 30), Some(51)); // 4K30
        assert_eq!(level(10, 10, 30), Some(11));
        // Wider than any level allows.
        assert_eq!(level(2000, 1, 1), None);
    }
}
