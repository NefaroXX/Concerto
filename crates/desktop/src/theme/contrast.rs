use iced::Color;

pub const AA_NORMAL_TEXT: f32 = 4.5;
pub const AA_LARGE_TEXT_UI: f32 = 3.0;

/// WCAG 2.x relative luminance from an sRGB color.
fn relative_luminance(c: Color) -> f32 {
    fn lin(ch: f32) -> f32 {
        if ch <= 0.03928 {
            ch / 12.92
        } else {
            ((ch + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio in the range 1.0..=21.0.
pub fn contrast_ratio(fg: Color, bg: Color) -> f32 {
    let (l1, l2) = (relative_luminance(fg), relative_luminance(bg));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;

    #[test]
    fn all_themes_meet_aa_for_body_text() {
        for theme in AppTheme::all() {
            let p = &theme.palette;
            assert!(
                contrast_ratio(p.text, p.background) >= AA_NORMAL_TEXT,
                "{}: text/background = {:.2}",
                theme.name,
                contrast_ratio(p.text, p.background)
            );
            assert!(
                contrast_ratio(p.text, p.surface) >= AA_NORMAL_TEXT,
                "{}: text/surface fails AA: {:.2}",
                theme.name,
                contrast_ratio(p.text, p.surface)
            );
            assert!(
                contrast_ratio(p.border, p.surface) >= AA_LARGE_TEXT_UI,
                "{}: border/surface fails 3:1: {:.2}",
                theme.name,
                contrast_ratio(p.border, p.surface)
            );
        }
    }

    /// Additional semantic contrast checks:
    /// - muted text on background (empty-state descriptions)
    /// - muted text on surface (labels)
    /// - success text on surface (status indicators)
    /// - danger text on surface (error indicators)
    #[test]
    fn all_themes_meet_semantic_contrast() {
        for theme in AppTheme::all() {
            let p = &theme.palette;

            // Muted text should be at least 3:1 against background (AA-large for UI text)
            assert!(
                contrast_ratio(p.text_muted, p.background) >= AA_LARGE_TEXT_UI,
                "{}: text_muted/background = {:.2} (min 3:1)",
                theme.name,
                contrast_ratio(p.text_muted, p.background)
            );

            // Muted text on surface
            assert!(
                contrast_ratio(p.text_muted, p.surface) >= AA_LARGE_TEXT_UI,
                "{}: text_muted/surface = {:.2} (min 3:1)",
                theme.name,
                contrast_ratio(p.text_muted, p.surface)
            );

            // Success indicators against surface (3:1 for UI components)
            assert!(
                contrast_ratio(p.success, p.surface) >= AA_LARGE_TEXT_UI,
                "{}: success/surface = {:.2} (min 3:1)",
                theme.name,
                contrast_ratio(p.success, p.surface)
            );

            // Danger indicators against surface (3:1 for UI components)
            assert!(
                contrast_ratio(p.danger, p.surface) >= AA_LARGE_TEXT_UI,
                "{}: danger/surface = {:.2} (min 3:1)",
                theme.name,
                contrast_ratio(p.danger, p.surface)
            );
        }
    }

    #[test]
    fn contrast_ratio_is_one_for_equal_colors() {
        let c = Color::from_rgb(0.5, 0.5, 0.5);
        let ratio = contrast_ratio(c, c);
        assert!((ratio - 1.0).abs() < 1e-6);
    }

    #[test]
    fn contrast_ratio_black_on_white() {
        let ratio = contrast_ratio(Color::BLACK, Color::WHITE);
        assert!((ratio - 21.0).abs() < 0.1);
    }
}
