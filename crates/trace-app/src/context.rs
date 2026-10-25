//! The context map panel: what fills the main agent's context window at the selected span, as a treemap.

use crate::theme::{KindColors, Palette};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::*;
use gpui_kit::*;
use trace_view::{ContextMap, PartKind, TraceDoc, context_map, format_tokens, pretty_model, treemap};

pub const PANEL_W: f32 = 300.;
const HEAD_H: f32 = 64.;
const FOOT_H: f32 = 36.;
const PAD: f32 = 12.;
/// Gap between neighbouring tiles.
const GAP: f32 = 3.;
/// Thinner tiles would draw as hairlines; their tokens still count toward the fill.
const MIN_TILE: f32 = GAP + 4.;
const LABEL_MIN: (f32, f32) = (44., 24.);
const TOKENS_MIN_H: f32 = 42.;

/// Hue means work, as in the waterfall: model output is lavender, tool results teal, structure gray.
fn colors(pal: &Palette, k: PartKind) -> KindColors {
    match k {
        PartKind::Baseline => pal.agent,
        PartKind::Prompts => KindColors { bg: pal.brand.bg, edge: pal.brand.fg.opacity(0.3), ink: pal.brand.fg, mark: pal.accent },
        PartKind::Output => pal.llm,
        PartKind::Tool => pal.tool,
        PartKind::Other => KindColors { bg: pal.chip, edge: pal.line, ink: pal.text, mark: pal.faint },
        PartKind::Summary => KindColors { bg: pal.ok.bg, edge: pal.ok.fg.opacity(0.3), ink: pal.ok.fg, mark: pal.ok.fg },
    }
}

/// A `PANEL_W` × `h` card beside the trace card.
pub fn panel(pal: &Palette, doc: &TraceDoc, h: f32) -> impl IntoElement {
    let map = context_map(&doc.state, doc.context_at());
    let model = map.as_ref().and_then(|m| m.model.as_deref()).map(pretty_model).unwrap_or_default();
    let fill = map.as_ref().map_or("—".into(), ContextMap::fill);
    let head = div()
        .h(px(HEAD_H))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(3.))
        .px(px(16.))
        .border_b_1()
        .border_color(pal.line)
        .child(div().flex().justify_between().gap(px(8.)).text_size(px(12.)).text_color(pal.muted).child("Context").child(div().truncate().child(model)))
        .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).font_features(crate::app::tnum()).truncate().child(fill));
    let (body, caption) = match &map {
        Some(m) => (tiles(pal, m, PANEL_W - 2. - 2. * PAD, (h - 2. - HEAD_H - FOOT_H - 2. * PAD).max(0.)).into_any_element(), m.caption(&doc.state)),
        None => (div().text_size(px(12.5)).text_color(pal.muted).child("No LLM call with token usage yet.").into_any_element(), String::new()),
    };
    div()
        .id("context-map")
        .role(Role::Group)
        .aria_label("Context map")
        .w(px(PANEL_W))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(pal.card)
        .rounded(px(16.))
        .border_1()
        .border_color(pal.line)
        .shadow(vec![crate::app::soft_shadow(pal, 0.04, 1., 3.)])
        .overflow_hidden()
        .child(head)
        .child(div().flex_1().min_h(px(0.)).p(px(PAD)).relative().child(body))
        .child(div().h(px(FOOT_H)).flex_shrink_0().flex().items_center().px(px(16.)).border_t_1().border_color(pal.line).text_size(px(12.)).text_color(pal.muted).truncate().child(caption))
}

fn tiles(pal: &Palette, m: &ContextMap, w: f32, h: f32) -> impl IntoElement {
    let mut weights: Vec<u64> = m.parts.iter().map(|p| p.tokens).collect();
    weights.push(m.free());
    let rects = treemap(&weights, w, h);
    div().size_full().relative().children(rects.into_iter().enumerate().filter(|(_, r)| r.2 >= MIN_TILE && r.3 >= MIN_TILE).map(|(i, (x, y, tw, th))| {
        let part = m.parts.get(i);
        let (label, kind, tokens) = part.map_or(("Free", "Free space", weights[i]), |p| (p.label.as_str(), p.kind.label(), p.tokens));
        let tip = format!("{label} · {kind} · {} tokens", format_tokens(tokens));
        let ink = part.map_or(pal.muted, |p| colors(pal, p.kind).ink);
        let (tw, th) = ((tw - GAP).max(1.), (th - GAP).max(1.));
        let labelled = tw >= LABEL_MIN.0 && th >= LABEL_MIN.1;
        div()
            .id(("context-tile", i))
            .role(Role::Label)
            .aria_label(tip.clone())
            .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
            .absolute()
            .left(px(x + GAP / 2.))
            .top(px(y + GAP / 2.))
            .w(px(tw))
            .h(px(th))
            .rounded(px(6.))
            .border_1()
            .overflow_hidden()
            .map(|el| match part {
                Some(p) => {
                    let c = colors(pal, p.kind);
                    el.bg(c.bg).border_color(c.edge)
                }
                None => el.border_dashed().border_color(pal.faint),
            })
            .when(labelled, |el| {
                el.flex()
                    .flex_col()
                    .gap(px(1.))
                    .px(px(7.))
                    .py(px(5.))
                    .child(div().truncate().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(ink).child(label.to_owned()))
                    .when(th >= TOKENS_MIN_H, |el| el.child(div().text_size(px(11.5)).font_features(crate::app::tnum()).text_color(ink.opacity(0.8)).child(format_tokens(tokens))))
            })
    }))
}
