//! Rustin Studio core: frames, sources, filters, scenes, compositor, transitions.

pub mod compositor;
pub mod effects;
pub mod filter;
pub mod frame;
pub mod scene;
pub mod source;
pub mod text;
pub mod transition;

pub use compositor::{CompositeBackend, Compositor, Layer};
pub use effects::{Effect, FilterSpec, MaskMode};
pub use filter::Filter;
pub use frame::Frame;
pub use scene::{Scene, SceneCollection, SceneItem};
pub use source::{Source, SourceSpec};
pub use text::TextSource;
pub use transition::TransitionKind;
