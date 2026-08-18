//! Rendering of LaTeX math expressions (`$…$` and `$$…$$`) as SVG images.
//!
//! Expressions are laid out by RaTeX into a display list, serialized to a
//! self-contained SVG (glyphs embedded as paths, so no fonts need to be
//! installed), and rasterized through GPUI's `SvgRenderer`. This mirrors the
//! mermaid pipeline in `mermaid.rs`: expressions render on a background task
//! into a cached `RenderImage`, and the markdown element is re-rendered once
//! the result lands.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use collections::HashMap;
use gpui::{AppContext, Context, Hsla, RenderImage, SharedString, Task};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_svg::{SvgOptions, render_to_svg};
use ratex_types::{Color, MathStyle};
use theme::ActiveTheme;

use crate::parser::MarkdownEvent;

/// Font size, in SVG user units, at which expressions are laid out. The
/// rasterized image is scaled when displayed, so this mainly controls
/// rasterization quality; 40 matches RaTeX's defaults.
pub(crate) const SVG_FONT_SIZE: f64 = 40.;
/// Padding around the expression, in the same units as [`SVG_FONT_SIZE`].
const SVG_PADDING: f64 = 10.;
/// Stroke width for rules and delimiters, in the same units as [`SVG_FONT_SIZE`].
const SVG_STROKE_WIDTH: f64 = 1.5;
/// Which revision of the bundled KaTeX fonts the extracted directory holds.
/// Bump this when the fonts under `assets/katex-fonts` change.
const KATEX_FONTS_VERSION: &str = "v0.1.14";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParsedMathExpression {
    pub(crate) source: SharedString,
    pub(crate) display: bool,
}

/// Collects the math expressions of a parsed document, keyed by the source
/// offset of their event.
pub(crate) fn extract_math_expressions(
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMathExpression> {
    let mut expressions = BTreeMap::default();
    for (range, event) in events {
        match event {
            MarkdownEvent::InlineMath(source) => {
                expressions.insert(
                    range.start,
                    ParsedMathExpression {
                        source: source.clone(),
                        display: false,
                    },
                );
            }
            MarkdownEvent::DisplayMath(source) => {
                expressions.insert(
                    range.start,
                    ParsedMathExpression {
                        source: source.clone(),
                        display: true,
                    },
                );
            }
            _ => {}
        }
    }
    expressions
}

/// Caches one rasterized image per distinct expression, like `MermaidState`
/// caches one image per distinct diagram.
#[derive(Default, Clone)]
pub(crate) struct MathState {
    cache: HashMap<ParsedMathExpression, Arc<CachedMathExpression>>,
}

impl MathState {
    pub(crate) fn update(
        &mut self,
        expressions: &BTreeMap<usize, ParsedMathExpression>,
        cx: &mut Context<crate::Markdown>,
    ) {
        let mut referenced = Vec::new();
        for expression in expressions.values() {
            referenced.push(expression.clone());
            self.cache
                .entry(expression.clone())
                .or_insert_with(|| {
                    Arc::new(CachedMathExpression::new(
                        expression.clone(),
                        cx.theme().colors().text,
                        cx,
                    ))
                });
        }
        self.cache.retain(|expression, _| referenced.contains(expression));
    }

    pub(crate) fn clear(&mut self) {
        self.cache.clear();
    }

    pub(crate) fn get(&self, expression: &ParsedMathExpression) -> Option<&Arc<CachedMathExpression>> {
        self.cache.get(expression)
    }
}

pub(crate) struct CachedMathExpression {
    render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
    _task: Task<()>,
}

impl CachedMathExpression {
    fn new(
        expression: ParsedMathExpression,
        text_color: Hsla,
        cx: &mut Context<crate::Markdown>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::new());
        let svg_renderer = cx.svg_renderer();
        let task = cx.spawn({
            let render_image = render_image.clone();
            let source = expression.source.clone();
            async move |this, cx| {
                let value = cx
                    .background_spawn(async move {
                        let svg =
                            render_math_to_svg(&expression.source, expression.display, text_color)?;
                        svg_renderer
                            .render_single_frame(svg.as_bytes(), 1.)
                            .map_err(anyhow::Error::from)
                    })
                    .await;
                if let Err(error) = &value {
                    log::debug!("failed to render math expression {source:?}: {error}");
                }
                let _ = render_image.set(value);
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        });
        Self {
            render_image,
            _task: task,
        }
    }

    pub(crate) fn image(&self) -> Option<&anyhow::Result<Arc<RenderImage>>> {
        self.render_image.get()
    }
}

/// Renders a LaTeX expression to a self-contained SVG string.
pub(crate) fn render_math_to_svg(
    source: &str,
    display: bool,
    color: Hsla,
) -> anyhow::Result<String> {
    let ast = parse(source).context("while parsing math")?;
    let style = if display {
        MathStyle::Display
    } else {
        MathStyle::Text
    };
    let options = LayoutOptions::default()
        .with_style(style)
        .with_color(hsla_to_ratex_color(color));
    let layout_box = layout(&ast, &options);
    let display_list = to_display_list(&layout_box);
    let font_dir = ensure_katex_fonts()?;
    Ok(render_to_svg(
        &display_list,
        &SvgOptions {
            font_size: SVG_FONT_SIZE,
            padding: SVG_PADDING,
            stroke_width: SVG_STROKE_WIDTH,
            embed_glyphs: true,
            font_dir: font_dir.to_string_lossy().into_owned(),
        },
    ))
}

fn hsla_to_ratex_color(color: Hsla) -> Color {
    let rgba = color.to_rgb();
    Color::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

const KATEX_FONTS: &[(&str, &[u8])] = &[
    (
        "KaTeX_AMS-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_AMS-Regular.ttf"),
    ),
    (
        "KaTeX_Caligraphic-Bold.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Caligraphic-Bold.ttf"),
    ),
    (
        "KaTeX_Caligraphic-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Caligraphic-Regular.ttf"),
    ),
    (
        "KaTeX_Fraktur-Bold.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Fraktur-Bold.ttf"),
    ),
    (
        "KaTeX_Fraktur-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Fraktur-Regular.ttf"),
    ),
    (
        "KaTeX_Main-Bold.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Main-Bold.ttf"),
    ),
    (
        "KaTeX_Main-BoldItalic.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Main-BoldItalic.ttf"),
    ),
    (
        "KaTeX_Main-Italic.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Main-Italic.ttf"),
    ),
    (
        "KaTeX_Main-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Main-Regular.ttf"),
    ),
    (
        "KaTeX_Math-BoldItalic.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Math-BoldItalic.ttf"),
    ),
    (
        "KaTeX_Math-Italic.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Math-Italic.ttf"),
    ),
    (
        "KaTeX_SansSerif-Bold.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_SansSerif-Bold.ttf"),
    ),
    (
        "KaTeX_SansSerif-Italic.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_SansSerif-Italic.ttf"),
    ),
    (
        "KaTeX_SansSerif-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_SansSerif-Regular.ttf"),
    ),
    (
        "KaTeX_Script-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Script-Regular.ttf"),
    ),
    (
        "KaTeX_Size1-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Size1-Regular.ttf"),
    ),
    (
        "KaTeX_Size2-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Size2-Regular.ttf"),
    ),
    (
        "KaTeX_Size3-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Size3-Regular.ttf"),
    ),
    (
        "KaTeX_Size4-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Size4-Regular.ttf"),
    ),
    (
        "KaTeX_Typewriter-Regular.ttf",
        include_bytes!("../assets/katex-fonts/KaTeX_Typewriter-Regular.ttf"),
    ),
];

static KATEX_FONT_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// RaTeX's `standalone` SVG output reads glyph outlines from KaTeX `.ttf`
/// files on disk, so the bundled fonts are extracted once into the data
/// directory and that directory is handed to the renderer.
fn ensure_katex_fonts() -> anyhow::Result<&'static Path> {
    let dir = KATEX_FONT_DIR.get_or_init(|| match extract_katex_fonts() {
        Ok(dir) => Some(dir),
        Err(error) => {
            log::error!("failed to extract KaTeX fonts: {error:#}");
            None
        }
    });
    dir.as_deref()
        .ok_or_else(|| anyhow::anyhow!("KaTeX fonts are unavailable"))
}

fn extract_katex_fonts() -> anyhow::Result<PathBuf> {
    let dir = paths::data_dir().join(format!("katex-fonts-{KATEX_FONTS_VERSION}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (file_name, bytes) in KATEX_FONTS {
        let path = dir.join(file_name);
        if !path.is_file() {
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_expression_to_svg_with_embedded_glyphs() {
        let svg = render_math_to_svg(r"\frac{1}{2}", false, gpui::black()).unwrap();
        assert!(svg.starts_with("<svg "));
        // Standalone mode must embed glyph outlines as paths rather than
        // relying on webfonts.
        assert!(svg.contains("<path"));
    }
}
