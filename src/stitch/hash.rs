use super::perceptual::{
    estimate_vertical_perceptual_motion_from_frame_with_config, PerceptualFrame,
    PerceptualMotionConfig, PerceptualMotionEstimate,
};
use super::rgb::validate_rgb_frame;
use super::{MotionEstimate, RgbFrame};

const HASH_MOTION_MIN_MATCH: usize = 4;
const FUZZY_ROW_MAX_AVERAGE_DIFF: f64 = 1.0;
const MULTI_LEVEL_FUZZY_ROW_MAX_AVERAGE_DIFF: f64 = 2.0;
const PERCEPTUAL_HASH_COARSE_BAND_HEIGHT: u32 = 24;
const PERCEPTUAL_HASH_COARSE_BAND_STEP: u32 = 12;
const PERCEPTUAL_HASH_COARSE_BINS: usize = 8;
const PERCEPTUAL_HASH_LOW_BAND_HEIGHT: u32 = 16;
const PERCEPTUAL_HASH_LOW_BAND_STEP: u32 = 8;
const PERCEPTUAL_HASH_LOW_BINS: usize = 16;
const PERCEPTUAL_HASH_MAX_TAIL_DIFF: f64 = 12.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuzzyHashMotionStats {
    pub total_row_pairs: usize,
    pub signature_candidate_pairs: usize,
    pub average_diff_row_pairs: usize,
}

#[allow(dead_code)]
pub fn estimate_vertical_fuzzy_hash_motion(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<MotionEstimate> {
    estimate_vertical_multi_level_fuzzy_hash_motion_with_stats(previous, current)
        .map(|(estimate, _stats)| estimate)
}

#[allow(dead_code)]
pub fn estimate_vertical_lazy_fuzzy_hash_motion(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<MotionEstimate> {
    estimate_vertical_fuzzy_hash_motion(previous, current)
}

#[allow(dead_code)]
pub fn estimate_vertical_multi_level_fuzzy_hash_motion(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<MotionEstimate> {
    estimate_vertical_multi_level_fuzzy_hash_motion_with_stats(previous, current)
        .map(|(estimate, _stats)| estimate)
}

#[allow(dead_code)]
pub fn estimate_vertical_multi_level_fuzzy_hash_motion_with_stats(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<(MotionEstimate, FuzzyHashMotionStats)> {
    if previous.width != current.width
        || previous.height != current.height
        || previous.height == 0
        || validate_rgb_frame(previous).is_err()
        || validate_rgb_frame(current).is_err()
    {
        return None;
    }

    let previous_perceptual = PerceptualFrame::from_source(previous);
    let current_perceptual = PerceptualFrame::from_source(current);
    estimate_vertical_fuzzy_hash_motion_with_max_average_diff(
        &previous_perceptual,
        &current_perceptual,
        FUZZY_ROW_MAX_AVERAGE_DIFF,
    )
    .or_else(|| {
        estimate_vertical_fuzzy_hash_motion_with_max_average_diff(
            &previous_perceptual,
            &current_perceptual,
            MULTI_LEVEL_FUZZY_ROW_MAX_AVERAGE_DIFF,
        )
    })
}

#[allow(dead_code)]
pub fn estimate_vertical_lazy_multi_level_fuzzy_hash_motion(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<MotionEstimate> {
    estimate_vertical_multi_level_fuzzy_hash_motion(previous, current)
}

fn estimate_vertical_fuzzy_hash_motion_with_max_average_diff(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    max_average_diff: f64,
) -> Option<(MotionEstimate, FuzzyHashMotionStats)> {
    if previous.width != current.width || previous.height != current.height || previous.height == 0
    {
        return None;
    }

    let total_candidate_groups = previous.height.min(current.height) as usize;
    let configs = perceptual_hash_configs(previous, current);
    let mut stats = FuzzyHashMotionStats {
        total_row_pairs: configs.len(),
        signature_candidate_pairs: 0,
        average_diff_row_pairs: 0,
    };

    for config in configs {
        stats.signature_candidate_pairs += 1;
        let Some(estimate) =
            estimate_vertical_perceptual_motion_from_frame_with_config(previous, current, config)
        else {
            continue;
        };
        if hash_estimate_accepted(estimate, max_average_diff) {
            let matched_groups = estimate.overlap_rows as usize;
            if matched_groups < HASH_MOTION_MIN_MATCH {
                return None;
            }
            return Some((
                MotionEstimate {
                    delta_y: estimate.delta_y,
                    confidence: matched_groups as f64 / total_candidate_groups as f64,
                    matched_groups,
                    total_candidate_groups,
                },
                stats,
            ));
        }
    }

    None
}

pub(super) fn estimate_vertical_perceptual_hash_motion_from_frame(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
) -> Option<PerceptualMotionEstimate> {
    estimate_vertical_perceptual_hash_motion_from_frame_with_max_average_diff(
        previous,
        current,
        FUZZY_ROW_MAX_AVERAGE_DIFF,
    )
    .or_else(|| {
        estimate_vertical_perceptual_hash_motion_from_frame_with_max_average_diff(
            previous,
            current,
            MULTI_LEVEL_FUZZY_ROW_MAX_AVERAGE_DIFF,
        )
    })
}

fn estimate_vertical_perceptual_hash_motion_from_frame_with_max_average_diff(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    max_average_diff: f64,
) -> Option<PerceptualMotionEstimate> {
    if previous.width != current.width || previous.height != current.height || previous.height == 0
    {
        return None;
    }

    for config in perceptual_hash_configs(previous, current) {
        let estimate =
            estimate_vertical_perceptual_motion_from_frame_with_config(previous, current, config)?;
        if hash_estimate_accepted(estimate, max_average_diff)
            && estimate.overlap_rows as usize >= HASH_MOTION_MIN_MATCH
        {
            return Some(estimate);
        }
    }

    None
}

fn hash_estimate_accepted(estimate: PerceptualMotionEstimate, max_average_diff: f64) -> bool {
    estimate.median <= max_average_diff
        && estimate.p75 <= PERCEPTUAL_HASH_MAX_TAIL_DIFF
        && estimate.p90 <= PERCEPTUAL_HASH_MAX_TAIL_DIFF
        && estimate.mean <= PERCEPTUAL_HASH_MAX_TAIL_DIFF
}

fn perceptual_hash_configs(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
) -> [PerceptualMotionConfig; 2] {
    let delta_range = previous.height.min(current.height) as i32 - 1;
    [
        PerceptualMotionConfig {
            delta_range,
            band_height: PERCEPTUAL_HASH_COARSE_BAND_HEIGHT,
            band_step: PERCEPTUAL_HASH_COARSE_BAND_STEP,
            bins: PERCEPTUAL_HASH_COARSE_BINS,
        },
        PerceptualMotionConfig {
            delta_range,
            band_height: PERCEPTUAL_HASH_LOW_BAND_HEIGHT,
            band_step: PERCEPTUAL_HASH_LOW_BAND_STEP,
            bins: PERCEPTUAL_HASH_LOW_BINS,
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::hint::black_box;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum ProfileStage {
        CoarseThreshold1,
        LowThreshold1,
        CoarseThreshold2,
        LowThreshold2,
        None,
    }

    #[derive(Debug, Default)]
    struct CascadeProfile {
        frame_pairs: usize,
        coarse_threshold1_hits: usize,
        low_threshold1_hits: usize,
        coarse_threshold2_hits: usize,
        low_threshold2_hits: usize,
        misses: usize,
        first_luminance_time: Duration,
        coarse_threshold1_time: Duration,
        low_threshold1_time: Duration,
        second_luminance_time: Duration,
        coarse_threshold2_time: Duration,
        low_threshold2_time: Duration,
    }

    #[test]
    #[ignore = "profiling output for the vertical_down_basic perceptual cascade"]
    fn vertical_down_basic_perceptual_cascade_profile() -> Result<(), Box<dyn Error>> {
        let frames = load_vertical_down_basic_frames()?;
        profile_scenario("natural", &frames)?;
        Ok(())
    }

    fn profile_scenario(name: &str, frames: &[RgbFrame]) -> Result<(), Box<dyn Error>> {
        let mut profile = CascadeProfile::default();

        for pair in frames.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];

            profile.frame_pairs += 1;
            match profile_pair(previous, current, &mut profile) {
                ProfileStage::CoarseThreshold1 => profile.coarse_threshold1_hits += 1,
                ProfileStage::LowThreshold1 => profile.low_threshold1_hits += 1,
                ProfileStage::CoarseThreshold2 => profile.coarse_threshold2_hits += 1,
                ProfileStage::LowThreshold2 => profile.low_threshold2_hits += 1,
                ProfileStage::None => profile.misses += 1,
            }
        }

        eprintln!(
            "cascade-profile scenario={name} frame_pairs={}",
            profile.frame_pairs
        );
        eprintln!(
            "cascade-profile hits coarse@1={} low@1={} coarse@2={} low@2={} miss={}",
            profile.coarse_threshold1_hits,
            profile.low_threshold1_hits,
            profile.coarse_threshold2_hits,
            profile.low_threshold2_hits,
            profile.misses
        );
        eprintln!(
            "cascade-profile time first_luminance={:?} coarse@1={:?} low@1={:?} second_luminance={:?} coarse@2={:?} low@2={:?}",
            profile.first_luminance_time,
            profile.coarse_threshold1_time,
            profile.low_threshold1_time,
            profile.second_luminance_time,
            profile.coarse_threshold2_time,
            profile.low_threshold2_time,
        );

        Ok(())
    }

    fn profile_pair(
        previous: &RgbFrame,
        current: &RgbFrame,
        profile: &mut CascadeProfile,
    ) -> ProfileStage {
        let started = Instant::now();
        let previous_perceptual = PerceptualFrame::from_source(previous);
        let current_perceptual = PerceptualFrame::from_source(current);
        profile.first_luminance_time += started.elapsed();

        let coarse_config = perceptual_coarse_config(previous, current);
        let low_config = perceptual_low_config(previous, current);

        let started = Instant::now();
        let coarse_threshold1 =
            black_box(estimate_vertical_perceptual_motion_from_frame_with_config(
                &previous_perceptual,
                &current_perceptual,
                coarse_config,
            ));
        profile.coarse_threshold1_time += started.elapsed();
        if estimate_accepted(coarse_threshold1, FUZZY_ROW_MAX_AVERAGE_DIFF) {
            return ProfileStage::CoarseThreshold1;
        }

        let started = Instant::now();
        let low_threshold1 = black_box(estimate_vertical_perceptual_motion_from_frame_with_config(
            &previous_perceptual,
            &current_perceptual,
            low_config,
        ));
        profile.low_threshold1_time += started.elapsed();
        if estimate_accepted(low_threshold1, FUZZY_ROW_MAX_AVERAGE_DIFF) {
            return ProfileStage::LowThreshold1;
        }

        let started = Instant::now();
        let previous_perceptual = PerceptualFrame::from_source(previous);
        let current_perceptual = PerceptualFrame::from_source(current);
        profile.second_luminance_time += started.elapsed();

        let started = Instant::now();
        let coarse_threshold2 =
            black_box(estimate_vertical_perceptual_motion_from_frame_with_config(
                &previous_perceptual,
                &current_perceptual,
                coarse_config,
            ));
        profile.coarse_threshold2_time += started.elapsed();
        if estimate_accepted(coarse_threshold2, MULTI_LEVEL_FUZZY_ROW_MAX_AVERAGE_DIFF) {
            return ProfileStage::CoarseThreshold2;
        }

        let started = Instant::now();
        let low_threshold2 = black_box(estimate_vertical_perceptual_motion_from_frame_with_config(
            &previous_perceptual,
            &current_perceptual,
            low_config,
        ));
        profile.low_threshold2_time += started.elapsed();
        if estimate_accepted(low_threshold2, MULTI_LEVEL_FUZZY_ROW_MAX_AVERAGE_DIFF) {
            return ProfileStage::LowThreshold2;
        }

        ProfileStage::None
    }

    fn perceptual_coarse_config(previous: &RgbFrame, current: &RgbFrame) -> PerceptualMotionConfig {
        PerceptualMotionConfig {
            delta_range: previous.height.min(current.height) as i32 - 1,
            band_height: PERCEPTUAL_HASH_COARSE_BAND_HEIGHT,
            band_step: PERCEPTUAL_HASH_COARSE_BAND_STEP,
            bins: PERCEPTUAL_HASH_COARSE_BINS,
        }
    }

    fn perceptual_low_config(previous: &RgbFrame, current: &RgbFrame) -> PerceptualMotionConfig {
        PerceptualMotionConfig {
            delta_range: previous.height.min(current.height) as i32 - 1,
            band_height: PERCEPTUAL_HASH_LOW_BAND_HEIGHT,
            band_step: PERCEPTUAL_HASH_LOW_BAND_STEP,
            bins: PERCEPTUAL_HASH_LOW_BINS,
        }
    }

    fn estimate_accepted(
        estimate: Option<super::super::perceptual::PerceptualMotionEstimate>,
        threshold: f64,
    ) -> bool {
        estimate.is_some_and(|estimate| {
            estimate.delta_y != 0
                && estimate.median <= threshold
                && estimate.overlap_rows as usize >= HASH_MOTION_MIN_MATCH
        })
    }

    fn load_vertical_down_basic_frames() -> Result<Vec<RgbFrame>, Box<dyn Error>> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let frame_dir = root
            .join("datasets")
            .join("cases")
            .join("vertical_down_basic")
            .join("frames");
        let mut paths: Vec<_> = fs::read_dir(&frame_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("frame_") && name.ends_with(".png"))
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|path| read_png_as_rgb_frame(path))
            .collect()
    }

    fn read_png_as_rgb_frame(path: &Path) -> Result<RgbFrame, Box<dyn Error>> {
        let decoder = png::Decoder::new(fs::File::open(path)?);
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf)?;

        let (width, height) = (info.width, info.height);
        let stride = width.checked_mul(3).ok_or("RGB stride overflow")?;
        let mut data = vec![
            0u8;
            (stride as usize)
                .checked_mul(height as usize)
                .ok_or("RGB size overflow")?
        ];

        match info.color_type {
            png::ColorType::Rgba => {
                for y in 0..height as usize {
                    let src_row = y * width as usize * 4;
                    let dst_row = y * stride as usize;
                    for x in 0..width as usize {
                        let src = src_row + x * 4;
                        let dst = dst_row + x * 3;
                        data[dst] = buf[src];
                        data[dst + 1] = buf[src + 1];
                        data[dst + 2] = buf[src + 2];
                    }
                }
            }
            png::ColorType::Rgb => {
                for y in 0..height as usize {
                    let src_row = y * width as usize * 3;
                    let dst_row = y * stride as usize;
                    let row_bytes = width as usize * 3;
                    data[dst_row..dst_row + row_bytes]
                        .copy_from_slice(&buf[src_row..src_row + row_bytes]);
                }
            }
            other => return Err(format!("unsupported PNG color type: {other:?}").into()),
        }

        Ok(RgbFrame {
            width,
            height,
            stride,
            data,
        })
    }
}
