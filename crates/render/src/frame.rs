//! Frame-time hooks (0027c-D9): the crate owns no clock; callers wrap spans
//! with a probe that reads its own. The compositor measures the Phase-4
//! frame budget through these.

/// An instrumented span of the drawing pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    /// Path rasterization into a coverage mask.
    Raster,
    /// Shadow mask blurring.
    Blur,
    /// A (possibly scaled) blit.
    Blit,
    /// Damage-limited layer composition.
    Compose,
}

/// A caller-supplied timing sink. `end` is always paired with the matching
/// `begin`; implementations read their own clock.
pub trait FrameProbe {
    fn begin(&mut self, span: Span);
    fn end(&mut self, span: Span);
}

/// The default probe: records nothing.
pub struct NoProbe;

impl FrameProbe for NoProbe {
    fn begin(&mut self, _span: Span) {}
    fn end(&mut self, _span: Span) {}
}
