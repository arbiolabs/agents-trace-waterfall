//! Color tokens from docs/design.md, one column per appearance.

use crate::present::{Ink, Tone};
use gpui_kit::{Hsla, rgb, rgba};

#[derive(Clone, Copy)]
pub struct KindColors {
    pub bg: Hsla,
    pub edge: Hsla,
    pub ink: Hsla,
    pub mark: Hsla,
}

#[derive(Clone, Copy)]
pub struct Pill {
    pub bg: Hsla,
    pub fg: Hsla,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub tray: Hsla,
    pub card: Hsla,
    /// Floating surfaces; a step above the card in dark mode.
    pub pop: Hsla,
    pub ink: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub line: Hsla,
    pub grid: Hsla,
    pub guide: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub selected_ring: Hsla,
    pub chip: Hsla,
    pub wash: Hsla,
    pub accent: Hsla,
    pub now: Hsla,
    pub agent: KindColors,
    pub llm: KindColors,
    pub tool: KindColors,
    pub error: KindColors,
    pub brand: Pill,
    pub ok: Pill,
    pub slow: Pill,
    pub amber: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex: u32) -> Hsla {
    rgba(hex).into()
}

/// WCAG relative luminance.
fn luminance(c: Hsla) -> f32 {
    let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let c = c.to_rgb();
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

fn kind(bg: u32, edge: u32, ink: u32, mark: u32) -> KindColors {
    KindColors { bg: c(bg), edge: c(edge), ink: c(ink), mark: c(mark) }
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    fn light() -> Self {
        Self {
            dark: false,
            tray: c(0xf0f0f0),
            card: c(0xffffff),
            pop: c(0xffffff),
            ink: c(0x171717),
            text: c(0x515154),
            muted: c(0x6e6e73),
            faint: c(0xb8b8b8),
            line: c(0xececec),
            grid: c(0xf1f1f1),
            guide: c(0xe6e6e6),
            hover: ca(0x00000006),
            selected: ca(0x2f6bff0d),
            selected_ring: ca(0x2f6bff2e),
            chip: c(0xf5f5f5),
            wash: c(0xf7f7f7),
            accent: c(0x2f6bff),
            now: ca(0x2f6bff8c),
            agent: kind(0xeef0f3, 0xd3d8e0, 0x4b5363, 0x9aa3b2),
            llm: kind(0xefecff, 0xcbc2fb, 0x5a45d4, 0x8d7cf0),
            tool: kind(0xe4f5f1, 0xa9ddd1, 0x0e7a67, 0x3db39a),
            error: kind(0xfde8e5, 0xf1a397, 0xc2331f, 0xe5604c),
            brand: Pill { bg: c(0xeaf1ff), fg: c(0x2456d6) },
            ok: Pill { bg: c(0xe6f6ec), fg: c(0x1a7f45) },
            slow: Pill { bg: c(0xfff3e0), fg: c(0xb85c00) },
            amber: c(0xd99a2b),
        }
    }

    fn dark() -> Self {
        Self {
            dark: true,
            tray: c(0x19191c),
            card: c(0x232326),
            pop: c(0x2a2a2e),
            ink: c(0xededed),
            text: c(0xc7c7cc),
            muted: c(0xa1a1a6),
            faint: c(0x4a4a50),
            line: c(0x2e2e33),
            grid: c(0x2a2a2e),
            guide: c(0x34343a),
            hover: ca(0xffffff06),
            selected: ca(0x5b9aff14),
            selected_ring: ca(0x5b9aff40),
            chip: c(0x2c2c30),
            wash: c(0x1d1d20),
            accent: c(0x5b9aff),
            now: ca(0x5b9aff99),
            agent: kind(0x3a3d44, 0x50555f, 0xb8c0cc, 0x7a8291),
            llm: KindColors { bg: ca(0x8d7cf047), edge: ca(0xa797ffa6), ink: c(0xc9bfff), mark: c(0x8d7cf0) },
            tool: kind(0x1c3b35, 0x2f7465, 0x62d5bb, 0x3db39a),
            error: kind(0x46221e, 0x8c3c33, 0xff977d, 0xe5604c),
            brand: Pill { bg: c(0x16243f), fg: c(0x8ab4ff) },
            ok: Pill { bg: c(0x12301f), fg: c(0x3dd68c) },
            slow: Pill { bg: c(0x3a2410), fg: c(0xffa057) },
            amber: c(0xd99a2b),
        }
    }

    pub fn kind(&self, t: Tone) -> KindColors {
        match t {
            Tone::Agent => self.agent,
            Tone::Llm => self.llm,
            Tone::Tool => self.tool,
            Tone::Error => self.error,
        }
    }

    /// A brand mark's color. In dark mode a near-black brand color (GitHub, Notion, Confluence) would
    /// vanish on the card, so it takes the ink instead; an untinted mark always does.
    pub fn brand_tint(&self, tint: Option<Hsla>) -> Hsla {
        match tint {
            Some(t) if !(self.dark && luminance(t) < 0.05) => t,
            _ => self.ink,
        }
    }

    pub fn ink_of(&self, i: Ink) -> Hsla {
        match i {
            Ink::Kind(t) => self.kind(t).ink,
            Ink::Amber => self.amber,
        }
    }
}
