//! DOT → SVG (system `dot` or the bundled Graphviz) → PNG.

use crate::settings::{RendererChoice, Settings};
use resvg::{tiny_skia, usvg};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// The largest preview we rasterize, in pixels (about 256 MB of RGBA).
const MAX_PIXELS: f32 = 64_000_000.0;
/// tiny-skia and most viewers cope with this per side.
const MAX_SIDE: f32 = 16_384.0;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Renderer {
    Dot(PathBuf),
    #[cfg(feature = "bundled")]
    Bundled,
}

impl Renderer {
    pub fn name(&self) -> &'static str {
        match self {
            Renderer::Dot(_) => "dot",
            #[cfg(feature = "bundled")]
            Renderer::Bundled => "bundled",
        }
    }
}

/// Whether this build embeds Graphviz.
pub const HAS_BUNDLED: bool = cfg!(feature = "bundled");

/// Picks the renderer for `settings` and explains the choice for the log.
pub fn resolve(settings: &Settings) -> Result<(Renderer, String), String> {
    let configured_dot = || -> Option<Result<PathBuf, String>> {
        let path = settings.dot_path.as_ref()?;
        Some(if path.is_file() {
            Ok(path.clone())
        } else {
            Err(format!("dot_path {} does not exist", path.display()))
        })
    };
    let dot_on_path = || which::which(dot_name()).ok();

    match settings.renderer {
        RendererChoice::Dot => match configured_dot() {
            Some(Ok(path)) => Ok((Renderer::Dot(path), "renderer setting \"dot\", dot_path setting".into())),
            Some(Err(e)) => Err(e),
            None => dot_on_path()
                .map(|p| (Renderer::Dot(p), "renderer setting \"dot\", found on PATH".into()))
                .ok_or_else(|| {
                    "renderer is \"dot\" but no dot executable is on PATH; install Graphviz or set dot_path".into()
                }),
        },
        RendererChoice::Bundled => bundled("renderer setting \"bundled\"".into()),
        RendererChoice::Auto => {
            let mut why = String::new();
            match configured_dot() {
                Some(Ok(path)) => return Ok((Renderer::Dot(path), "dot_path setting".into())),
                Some(Err(e)) => why = format!("{e}; "),
                None => {}
            }
            if let Some(path) = dot_on_path() {
                return Ok((Renderer::Dot(path), format!("{why}found on PATH")));
            }
            bundled(format!("{why}no dot on PATH"))
        }
    }
}

#[cfg(feature = "bundled")]
fn bundled(reason: String) -> Result<(Renderer, String), String> {
    Ok((Renderer::Bundled, reason))
}

#[cfg(not(feature = "bundled"))]
fn bundled(reason: String) -> Result<(Renderer, String), String> {
    Err(format!(
        "{reason}, and this build of graphviz-preview has no bundled Graphviz; install Graphviz or set dot_path"
    ))
}

fn dot_name() -> &'static str {
    if cfg!(windows) {
        "dot.exe"
    } else {
        "dot"
    }
}

/// Dark defaults in a Zed One Dark–like palette, as `(statement, attribute,
/// value)` triples.
pub const DARK_PALETTE: &[(&str, &str, &str)] = &[
    ("graph", "bgcolor", "#282c33"),
    ("graph", "color", "#a9afbc"),
    ("graph", "fontcolor", "#dce0e5"),
    ("graph", "fillcolor", "#2f343e"),
    ("node", "color", "#a9afbc"),
    ("node", "fontcolor", "#dce0e5"),
    ("node", "fillcolor", "#3b414d"),
    ("edge", "color", "#a9afbc"),
    ("edge", "fontcolor", "#dce0e5"),
];

/// The source with dark defaults inserted as the first statements of the
/// graph body, on the same line as its `{` so no line moves.
///
/// Statements that come later in the file override them, which is what
/// lets a file's own colours win. Graphviz's `-G`/`-N`/`-E` flags cannot be
/// used for this: `-G` replaces the file's graph attributes and `-N`/`-E`
/// replace its `node [...]`/`edge [...]` defaults.
pub fn with_dark_defaults(source: &str) -> Option<String> {
    let at = crate::document::body_start(source)?;
    let mut statements = String::new();
    for kind in ["graph", "node", "edge"] {
        statements.push(' ');
        statements.push_str(kind);
        statements.push_str(" [");
        let attrs: Vec<String> = DARK_PALETTE
            .iter()
            .filter(|(k, _, _)| *k == kind)
            .map(|(_, name, value)| format!("{name}=\"{value}\""))
            .collect();
        statements.push_str(&attrs.join(" "));
        statements.push_str("];");
    }
    statements.push(' ');
    Some(format!("{}{}{}", &source[..at], statements, &source[at..]))
}

/// Graphviz arguments for one render: the output format and an optional
/// layout engine. Without one, Graphviz honours the file's `layout=`.
pub fn graphviz_args(engine: Option<&str>) -> Vec<String> {
    let mut args = vec!["-Tsvg".to_string()];
    if let Some(engine) = engine {
        args.push(format!("-K{engine}"));
    }
    args
}

pub struct SvgOutput {
    /// The SVG when Graphviz exited successfully and produced output.
    pub svg: Option<Vec<u8>>,
    pub stderr: String,
    pub timed_out: bool,
    /// A failure to run Graphviz at all (not a problem in the graph).
    pub failure: Option<String>,
}

/// Runs Graphviz on `source`, bounded by `timeout`.
pub fn render_svg(
    renderer: &Renderer,
    source: &str,
    args: &[String],
    timeout: Duration,
) -> SvgOutput {
    match renderer {
        Renderer::Dot(path) => run_dot(path, source, args, timeout),
        #[cfg(feature = "bundled")]
        Renderer::Bundled => crate::bundled::run(source, args, timeout),
    }
}

fn run_dot(path: &Path, source: &str, args: &[String], timeout: Duration) -> SvgOutput {
    let failed = |msg: String| SvgOutput {
        svg: None,
        stderr: String::new(),
        timed_out: false,
        failure: Some(msg),
    };
    let mut command = Command::new(path);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return failed(format!("could not start {}: {e}", path.display())),
    };

    // Feed stdin and drain both pipes on their own threads so a large graph
    // cannot deadlock on a full pipe.
    let stdin = child.stdin.take();
    let input = source.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            // A broken pipe here means dot exited early; its stderr says why.
            let _ = stdin.write_all(&input);
        }
    });
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => Some(status),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return failed(format!("waiting for {} failed: {e}", path.display()));
        }
    };
    let _ = writer.join();
    let stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    match status {
        None => SvgOutput {
            svg: None,
            stderr,
            timed_out: true,
            failure: None,
        },
        Some(status) => SvgOutput {
            svg: (status.success() && !stdout.is_empty()).then_some(stdout),
            stderr,
            timed_out: false,
            failure: None,
        },
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// System fonts, loaded once. Generic families fall back to installed fonts
/// so text never silently disappears on a machine without Times or Arial.
pub fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let started = Instant::now();
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        let families: Vec<String> = db
            .faces()
            .flat_map(|f| f.families.iter().map(|(name, _)| name.clone()))
            .collect();
        let has = |name: &str| families.iter().any(|f| f.eq_ignore_ascii_case(name));
        let pick = |candidates: &[&str]| -> Option<String> {
            candidates
                .iter()
                .find(|c| has(c))
                .map(|c| c.to_string())
                .or_else(|| families.first().cloned())
        };
        if let Some(f) = pick(&[
            "Times New Roman",
            "Times",
            "DejaVu Serif",
            "Liberation Serif",
            "Noto Serif",
        ]) {
            db.set_serif_family(f);
        }
        if let Some(f) = pick(&[
            "Arial",
            "Helvetica",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
        ]) {
            db.set_sans_serif_family(f);
        }
        if let Some(f) = pick(&[
            "Courier New",
            "Courier",
            "Menlo",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Noto Sans Mono",
        ]) {
            db.set_monospace_family(f);
        }
        crate::log(format!(
            "fonts: {} faces loaded in {} ms",
            db.len(),
            started.elapsed().as_millis()
        ));
        Arc::new(db)
    })
    .clone()
}

fn parse_svg(svg: &[u8], resources_dir: Option<&Path>) -> Result<usvg::Tree, String> {
    let options = usvg::Options {
        resources_dir: resources_dir.map(Path::to_path_buf),
        fontdb: fontdb(),
        ..Default::default()
    };
    usvg::Tree::from_data(svg, &options).map_err(|e| format!("could not read Graphviz SVG: {e}"))
}

/// Rasterizes `svg` at `scale`, optionally capping the width in pixels.
/// Huge graphs are scaled down to stay within `MAX_PIXELS`.
pub fn svg_to_png(
    svg: &[u8],
    scale: f32,
    max_width: Option<f32>,
    resources_dir: Option<&Path>,
) -> Result<Vec<u8>, String> {
    let tree = parse_svg(svg, resources_dir)?;
    let (w, h) = (tree.size().width(), tree.size().height());
    let mut scale = scale;
    if let Some(max) = max_width {
        scale = scale.min(max / w.max(1.0));
    }
    scale = scale
        .min((MAX_PIXELS / (w * h).max(1.0)).sqrt())
        .min(MAX_SIDE / w.max(1.0))
        .min(MAX_SIDE / h.max(1.0));
    let width = (w * scale).ceil().max(1.0) as u32;
    let height = (h * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| format!("cannot allocate a {width}x{height} image"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|e| format!("PNG encoding failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_set_the_format_and_optional_engine() {
        assert_eq!(graphviz_args(None), vec!["-Tsvg"]);
        assert_eq!(graphviz_args(Some("neato")), vec!["-Tsvg", "-Kneato"]);
    }

    #[test]
    fn dark_defaults_are_inserted_without_moving_lines() {
        let source = "// c\ndigraph G {\n  a -> b\n}\n";
        let dark = with_dark_defaults(source).unwrap();
        assert_eq!(dark.lines().count(), source.lines().count());
        let header = dark.lines().nth(1).unwrap();
        assert!(
            header.starts_with("digraph G { graph [bgcolor=\"#282c33\""),
            "{header}"
        );
        assert!(header
            .contains("node [color=\"#a9afbc\" fontcolor=\"#dce0e5\" fillcolor=\"#3b414d\"];"));
        assert!(header.contains("edge [color=\"#a9afbc\" fontcolor=\"#dce0e5\"];"));
        assert_eq!(dark.lines().nth(2), Some("  a -> b"));
        assert_eq!(with_dark_defaults("node [x=y]"), None);
    }

    fn dot() -> Renderer {
        Renderer::Dot(which::which(dot_name()).expect("the tests need Graphviz `dot` on PATH"))
    }

    fn svg_of(source: &str) -> String {
        let out = render_svg(
            &dot(),
            source,
            &graphviz_args(None),
            Duration::from_secs(10),
        );
        String::from_utf8(
            out.svg
                .unwrap_or_else(|| panic!("dot failed: {}", out.stderr)),
        )
        .unwrap()
    }

    #[test]
    fn file_colours_win_over_dark_defaults_in_real_dot() {
        let plain = svg_of(&with_dark_defaults("digraph { a -> b }").unwrap());
        assert!(plain.contains(r##"<polygon fill="#282c33""##), "{plain}");
        assert!(plain.contains(r##"stroke="#a9afbc""##));
        assert!(plain.contains(r##"fill="#dce0e5""##), "text colour");

        let own = svg_of(
            &with_dark_defaults(
                "digraph {\n bgcolor=white\n node [color=red fontcolor=blue]\n edge [color=green]\n a -> b\n c [color=orange]\n}",
            )
            .unwrap(),
        );
        assert!(
            own.contains(r##"<polygon fill="white""##),
            "graph attribute: {own}"
        );
        assert!(own.contains(r##"stroke="red""##), "node default");
        assert!(own.contains(r##"fill="blue""##), "node fontcolor");
        assert!(own.contains(r##"stroke="green""##), "edge default");
        assert!(own.contains(r##"stroke="orange""##), "node attribute");
        assert!(!own.contains("#a9afbc"), "a dark default leaked: {own}");
    }

    #[test]
    fn explicit_dot_renderer_reports_a_missing_dot_path() {
        let settings = Settings {
            renderer: RendererChoice::Dot,
            dot_path: Some(PathBuf::from("/definitely/not/here/dot")),
            ..Settings::default()
        };
        let err = resolve(&settings).unwrap_err();
        assert!(err.contains("/definitely/not/here/dot"), "{err}");
    }

    #[test]
    fn png_respects_scale_and_width_cap() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="white"/></svg>"#;
        let png = svg_to_png(svg, 2.0, None, None).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = tiny_skia::Pixmap::decode_png(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (200, 100));
        let capped = svg_to_png(svg, 2.0, Some(50.0), None).unwrap();
        let decoded = tiny_skia::Pixmap::decode_png(&capped).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (50, 25));
    }
}
