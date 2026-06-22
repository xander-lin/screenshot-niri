use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedBands {
    pub top: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub current_origin_x: i32,
    pub current_origin_y: i32,
    pub anchor_origin_x: i32,
    pub anchor_origin_y: i32,
    pub compose_width: u32,
    pub compose_height: u32,
    pub active_crop: ComposeCrop,
    pub fixed_bands: FixedBands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendDirection {
    Bottom,
    Top,
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameMatch {
    pub direction: AppendDirection,
    pub overlap: u32,
    pub delta_x: i32,
    pub delta_y: i32,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PushStreakKind {
    Duplicate,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Initialized,
    Accepted { match_info: FrameMatch },
    Duplicate,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuplicateAnalysisMotion {
    pub changed_top: u32,
    pub changed_bottom: u32,
    pub strongest_y: u32,
    pub strongest_diff: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastMotionCandidate {
    pub delta_y: i32,
    pub score: f64,
    pub overlap_rows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastMotionAgreement {
    NoCandidate,
    HeavyExactDelta,
    HeavySameDirection,
    HeavyDifferentDelta,
    HeavyOppositeDirection,
    FallbackExactDelta,
    FallbackSameDirection,
    FallbackOppositeDirection,
    MissedHeavyAccept,
    MissedFallbackAccept,
    CandidateOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastMotionVerifyPass {
    Top20,
    Top50,
    #[allow(dead_code)]
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastMotionTrace {
    pub candidate: Option<FastMotionCandidate>,
    pub reference_delta_y: Option<i32>,
    pub agreement: FastMotionAgreement,
    pub verify_pass: Option<FastMotionVerifyPass>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StitchDecisionPath {
    Initialized,
    Duplicate,
    CanvasAccepted,
    PerceptualAccepted,
    FallbackAccepted,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StitchProfileBreakdown {
    pub path: Option<StitchDecisionPath>,
    pub duplicate_check: Duration,
    pub perceptual_prepare: Duration,
    pub fixed_bands: Duration,
    pub canvas_match: Duration,
    pub perceptual_match: Duration,
    pub fallback_match: Duration,
    pub apply_match: Duration,
    pub append_frame: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub struct MotionEstimate {
    pub delta_y: i32,
    pub confidence: f64,
    pub matched_groups: usize,
    pub total_candidate_groups: usize,
}
