//! Reusable Fluent visual primitives for native DeepX WinUI views.
//!
//! This crate deliberately contains visual semantics rather than application
//! state. All colors are WinUI theme resources so light, dark, high-contrast,
//! accent, inactive-window, and future Windows theme changes remain owned by
//! the platform.

use windows_reactor::*;

pub mod motion {
    //! Fluent motion tokens shared by native surfaces.
    //!
    //! Composition animations do not automatically inherit WinUI theme
    //! transition policy, so consult the Windows client-area animation flag
    //! before returning a transition. Callers can use the returned `Option`
    //! directly with `ElementExt::transition`.

    use std::time::Duration;

    use windows_reactor::AnimationConfig;

    const SM_CLIENTAREAANIMATION: i32 = 0x2002;

    #[cfg(windows)]
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }

    /// Whether Windows currently permits non-essential client-area motion.
    pub fn animations_enabled() -> bool {
        #[cfg(windows)]
        {
            // SAFETY: GetSystemMetrics is process-global, takes a constant
            // metric index, and has no pointer or lifetime requirements.
            unsafe { GetSystemMetrics(SM_CLIENTAREAANIMATION) != 0 }
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// Short reveal for a newly mounted status, tool, or command surface.
    pub fn reveal() -> Option<AnimationConfig> {
        animations_enabled().then(|| AnimationConfig::fade_in(Duration::from_millis(120)))
    }

    /// Content-level entrance used when a page or finalized answer replaces
    /// another semantic state. Kept below 200 ms to avoid blocking reading.
    pub fn content_enter() -> Option<AnimationConfig> {
        animations_enabled().then(|| AnimationConfig::fade_in(Duration::from_millis(180)))
    }

    /// Faster exit so dismissed UI never feels slower than its invocation.
    pub fn content_exit() -> Option<AnimationConfig> {
        animations_enabled().then(|| AnimationConfig::fade_out(Duration::from_millis(100)))
    }
}

pub mod tokens {
    //! Shared geometry and type ramp for DeepX native surfaces.

    pub const SPACE_1: f64 = 4.0;
    pub const SPACE_2: f64 = 8.0;
    pub const SPACE_3: f64 = 12.0;
    pub const SPACE_4: f64 = 16.0;
    pub const SPACE_6: f64 = 24.0;

    pub const RADIUS_CONTROL: f64 = 4.0;
    pub const RADIUS_CARD: f64 = 8.0;
    pub const RADIUS_MESSAGE: f64 = 12.0;

    pub const TYPE_CAPTION: f64 = 12.0;
    pub const TYPE_BODY: f64 = 14.0;
    pub const TYPE_BODY_LARGE: f64 = 18.0;
    pub const TYPE_SUBTITLE: f64 = 20.0;

    /// Comfortable reading measure for long-form assistant output.
    pub const READING_MAX_WIDTH: f64 = 880.0;
    /// Shared centered column for a transcript turn and its composer.
    pub const CONVERSATION_MAX_WIDTH: f64 = 1040.0;
    /// User prompts are intentionally narrower and right aligned.
    pub const USER_MESSAGE_MAX_WIDTH: f64 = 720.0;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StatusTone {
    Running,
    Success,
    Critical,
    Neutral,
}

impl StatusTone {
    pub fn foreground(self) -> ThemeRef {
        match self {
            Self::Running => ThemeRef::SystemCaution,
            Self::Success => ThemeRef::SystemSuccess,
            Self::Critical => ThemeRef::SystemCritical,
            Self::Neutral => ThemeRef::SecondaryText,
        }
    }

    pub fn background(self) -> ThemeRef {
        match self {
            Self::Running => ThemeRef::SystemCautionBackground,
            Self::Success => ThemeRef::SystemSuccessBackground,
            Self::Critical => ThemeRef::SystemCriticalBackground,
            Self::Neutral => card_background_secondary(),
        }
    }
}

fn card_background_secondary() -> ThemeRef {
    ThemeRef::custom("CardBackgroundFillColorSecondaryBrush")
}

fn text_on_accent() -> ThemeRef {
    ThemeRef::custom("TextOnAccentFillColorPrimaryBrush")
}

fn hairline() -> Thickness {
    Thickness::uniform(1.0)
}

/// Compact semantic state label. It uses system status resources instead of
/// literal colors, so high contrast and dark mode retain meaning.
pub fn status_badge(label: impl Into<String>, tone: StatusTone) -> Element {
    let label = label.into();
    let text: Element = text_block(label.clone())
        .font_size(tokens::TYPE_CAPTION)
        .foreground(tone.foreground())
        .into();
    let content: Element = if tone == StatusTone::Running {
        hstack((ProgressRing::default().width(12.0).height(12.0), text))
            .spacing(tokens::SPACE_1)
            .into()
    } else {
        text
    };
    border(content)
        .background(tone.background())
        .corner_radius(tokens::RADIUS_CONTROL)
        .padding(Thickness {
            left: 6.0,
            top: 2.0,
            right: 6.0,
            bottom: 2.0,
        })
        .automation_name(label)
        .into()
}

/// Right-aligned prompt surface. Authorship is expressed through layout and a
/// narrow accent indicator; the card itself uses a resting content brush, not
/// an accent button's pointer-over/pressed state brush.
pub fn user_message(body: impl Into<Element>, status: Element) -> Element {
    border(
        vstack((
            hstack((
                text_block("你")
                    .font_size(tokens::TYPE_BODY)
                    .semibold()
                    .foreground(ThemeRef::SecondaryText),
                status,
            ))
            .spacing(tokens::SPACE_2),
            body.into(),
        ))
        .spacing(tokens::SPACE_2),
    )
    .background(ThemeRef::CardBackground)
    .border_brush(ThemeRef::Accent)
    .border_thickness(Thickness {
        left: 2.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    })
    .corner_radius(tokens::RADIUS_MESSAGE)
    .padding(tokens::SPACE_3)
    .max_width(tokens::USER_MESSAGE_MAX_WIDTH)
    .horizontal_alignment(HorizontalAlignment::Right)
    .into()
}

/// Open assistant canvas. Fluent hierarchy comes from whitespace and a small
/// author label; long-form answers are not boxed into a second chat bubble.
pub fn assistant_message(body: impl Into<Element>) -> Element {
    vstack((
        text_block("DeepX")
            .font_size(tokens::TYPE_BODY)
            .semibold()
            .foreground(ThemeRef::PrimaryText),
        body.into(),
    ))
    .spacing(tokens::SPACE_2)
    .padding(Thickness {
        left: tokens::SPACE_3,
        top: tokens::SPACE_2,
        right: tokens::SPACE_3,
        bottom: 0.0,
    })
    .max_width(tokens::READING_MAX_WIDTH)
    .horizontal_alignment(HorizontalAlignment::Stretch)
    .into()
}

/// Secondary information surface for tool details, diagnostics, and other
/// content that should remain subordinate to the answer.
pub fn inset_surface(child: impl Into<Element>) -> Element {
    border(child)
        .background(card_background_secondary())
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(hairline())
        .corner_radius(tokens::RADIUS_CARD)
        .padding(tokens::SPACE_3)
        .into()
}

/// Native code surface with a subdued language eyebrow and theme-aware fill.
pub fn code_surface(
    language: impl Into<String>,
    code: impl Into<String>,
    key: impl Into<String>,
) -> Element {
    let language = language.into();
    let language = if language.trim().is_empty() {
        "代码".to_string()
    } else {
        language.to_uppercase()
    };
    border(
        vstack((
            text_block(language)
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText),
            text_block(code)
                .font_size(13.0)
                .font_family("Cascadia Mono")
                .wrap()
                .selectable(),
        ))
        .spacing(tokens::SPACE_2),
    )
    .background(card_background_secondary())
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(hairline())
    .corner_radius(tokens::RADIUS_CARD)
    .padding(tokens::SPACE_3)
    .with_key(key)
    .into()
}

/// Centered empty/loading state used by content views.
pub fn empty_state(title: impl Into<String>, detail: impl Into<String>, busy: bool) -> Element {
    let progress: Element = if busy {
        ProgressRing::default().width(28.0).height(28.0).into()
    } else {
        border(
            text_block("DX")
                .font_size(tokens::TYPE_CAPTION)
                .semibold()
                .foreground(text_on_accent()),
        )
        .width(40.0)
        .height(40.0)
        .background(ThemeRef::Accent)
        .corner_radius(20.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
    };
    vstack((
        progress,
        text_block(title)
            .font_size(tokens::TYPE_SUBTITLE)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Center),
        text_block(detail)
            .font_size(tokens::TYPE_BODY)
            .foreground(ThemeRef::SecondaryText)
            .wrap()
            .horizontal_alignment(HorizontalAlignment::Center),
    ))
    .spacing(tokens::SPACE_2)
    .padding(tokens::SPACE_6)
    .max_width(420.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    .into()
}

/// Persistent command surface placed above Mica/content. It deliberately uses
/// the layer brush instead of Acrylic; Acrylic remains reserved for transient
/// flyouts and menus provided by WinUI controls.
pub fn command_surface(child: impl Into<Element>) -> Element {
    border(child)
        .background(ThemeRef::LayerFill)
        .border_brush(ThemeRef::SurfaceStroke)
        .border_thickness(hairline())
        .corner_radius(tokens::RADIUS_CARD)
        .into()
}

/// Small non-interactive metadata marker such as a file type.
pub fn metadata_badge(label: impl Into<String>) -> Element {
    border(
        text_block(label)
            .font_size(tokens::TYPE_CAPTION)
            .foreground(ThemeRef::SecondaryText),
    )
    .background(card_background_secondary())
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(hairline())
    .corner_radius(tokens::RADIUS_CONTROL)
    .padding(Thickness::xy(6.0, 3.0))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tones_use_semantic_theme_resources() {
        assert_eq!(StatusTone::Success.foreground(), ThemeRef::SystemSuccess);
        assert_eq!(
            StatusTone::Critical.background(),
            ThemeRef::SystemCriticalBackground
        );
    }

    #[test]
    fn resting_surfaces_do_not_reuse_interaction_state_brushes() {
        assert_eq!(
            card_background_secondary().resource_key(),
            "CardBackgroundFillColorSecondaryBrush"
        );
        assert_eq!(
            text_on_accent().resource_key(),
            "TextOnAccentFillColorPrimaryBrush"
        );
    }

    #[test]
    fn primitives_build_native_reactor_elements() {
        assert_eq!(
            status_badge("完成", StatusTone::Success).kind_name(),
            "Border"
        );
        assert_eq!(empty_state("空", "说明", false).kind_name(), "StackPanel");
        assert_eq!(
            code_surface("rs", "fn main() {}", "code").kind_name(),
            "Border"
        );
        assert_eq!(command_surface(grid(())).kind_name(), "Border");
        assert_eq!(metadata_badge("TXT").kind_name(), "Border");
    }

    #[test]
    fn fluent_motion_tokens_are_short_and_optional() {
        if let Some(reveal) = motion::reveal() {
            assert_eq!(reveal.duration, std::time::Duration::from_millis(120));
        }
        if let Some(exit) = motion::content_exit() {
            assert!(exit.duration < std::time::Duration::from_millis(180));
        }
    }
}
