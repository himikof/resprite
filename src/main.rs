use std::borrow::Cow;
use std::fmt::Formatter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use globwalk::GlobWalkerBuilder;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use resvg::usvg;
use serde_json::json;

mod cli {
    use std::path::PathBuf;

    use bpaf::Bpaf;

    #[derive(Bpaf)]
    #[bpaf(generate(config_parser), options, version)]
    /// Build a Mapbox sprite atlas from an input directory of SVGs.
    ///
    ///
    /// The following files will be created:
    ///   ${output%.*}.json
    ///     Base resolution atlas metadata
    ///   ${output%.*}.png
    ///     Base resolution atlas
    ///   ${output%.*}@2x.json
    ///     Hi-res resolution atlas metadata (in case of --with-hires)
    ///   ${output%.*}@2x.png
    ///     Hi-res resolution atlas (in case of --with-hires)
    /// SVG file names will be used as icon identifiers in the resulting atlas.
    pub struct Config {
        /// Base output file path (with or without an extension)
        #[bpaf(short, long, argument("PATH"))]
        pub output: PathBuf,
        /// Override the XML stylesheet in SVG files
        #[bpaf(long("css"), argument("PATH"))]
        pub css_override: Option<PathBuf>,
        // TODO: custom base scale
        #[bpaf(switch)]
        pub with_hires: bool,
        /// Additional buffer (padding) size
        #[bpaf(long, argument("LENGTH"))]
        pub buffer: Option<svgtypes::Length>,
        /// Verbose console output
        #[bpaf(short, long, switch)]
        pub verbose: bool,
        /// Number of parallel threads to use
        #[cfg(feature = "parallel")]
        #[bpaf(short('j'), long, argument("N"), fallback(0))]
        pub threads: usize,
        /// Input directory with SVG files, can be repeated
        #[bpaf(positional("SVG DIR"))]
        pub svg_dirs: Vec<PathBuf>,
    }

}

struct SvgSource {
    input_path: PathBuf,
    svg_data: Arc<Vec<u8>>,
}

#[derive(Copy, Clone)]
struct AtlasOptions {
    pixel_ratio: f64,
    buffer_px: f64,
}

impl AtlasOptions {
    fn new(args: &cli::Config, ratio: f64) -> Result<Self> {
        let buffer_size = args.buffer.as_ref().map_or(
            Ok(0.0), |len| resolve_length(len, ratio))?;
        Ok(Self {
            pixel_ratio: ratio,
            buffer_px: buffer_size.ceil(),
        })
    }
}

struct AtlasSourceData {
    #[cfg(not(feature = "parallel"))]
    svg_trees: Vec<usvg::Tree>,
    #[cfg(feature = "parallel")]
    svg_data: Vec<Arc<Vec<u8>>>,
}

struct PreparedSvgAtlas {
    atlas_options: AtlasOptions,
    #[allow(dead_code)]
    svg_options: usvg::Options,
    ids: Vec<String>,
    data: AtlasSourceData,
    layout: potpack2::Layout,
}

fn svg_load_options(options: &AtlasOptions) -> usvg::Options {
    usvg::Options {
        resources_dir: None,
        dpi: 96.0 * options.pixel_ratio,
        // default_size: is the default (100, 100) fine?
        ..Default::default()
    }
}

fn resolve_length(length: &svgtypes::Length, pixel_ratio: f64) -> Result<f64> {
    use svgtypes::LengthUnit as Unit;
    let dpi = pixel_ratio * 96.0;
    let n = length.number;
    let result = match length.unit {
        Unit::None | Unit::Px => n,
        Unit::In => n * dpi,
        Unit::Cm => n * dpi / 2.54,
        Unit::Mm => n * dpi / 25.4,
        Unit::Pt => n * dpi / 72.0,
        Unit::Pc => n * dpi / 6.0,

        Unit::Em | Unit::Ex => {
            bail!("Font-dependent sizes are not supported");
        },
        Unit::Percent => {
            bail!("Relative sizes are not supported");
        }
    };
    Ok(result)
}

mod potpack2;

#[cfg(any())]
fn dump_tree_node(usvg_node: &usvg::Node, level: usize) {
    use resvg::usvg::NodeKind;
    let node = usvg_node.borrow();
    let node_debug = match *node {
        NodeKind::Group(ref n) => n as &dyn std::fmt::Debug,
        NodeKind::Path(ref n) => n as &dyn std::fmt::Debug,
        NodeKind::Image(ref n) => n as &dyn std::fmt::Debug,
        NodeKind::Text(ref n) => n as &dyn std::fmt::Debug,
    };
    println!("{}{:?}", " ".repeat(level * 2), node_debug);
    for child in usvg_node.children() {
        dump_tree_node(&child, level + 1);
    }
}

fn href_from_xml_stylesheet(pi_xml: &xmltree::Element) -> Option<&String> {
    if pi_xml.attributes.get("type").map(String::as_str) != Some("text/css") {
        return None;
    }
    let href = pi_xml.attributes.get("href")?;
    if href.contains("..") || href.contains('\x00') {
        return None;
    }
    Some(href)
}

struct PathDisplay<'a> {
    inner: &'a Path,
}

impl<'a, S: AsRef<std::ffi::OsStr> + ?Sized> From<&'a S> for PathDisplay<'a> {
    fn from(s: &'a S) -> Self { Self { inner: Path::new(s) } }
}

impl std::fmt::Display for PathDisplay<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.inner.display().fmt(f)
    }
}

fn pd<S: AsRef<std::ffi::OsStr> + ?Sized>(s: &S) -> PathDisplay {
    s.into()
}

fn name_pd(p: &Path) -> PathDisplay {
    p.file_name().map(Into::into).unwrap_or(p.into())
}

fn patch_xml_style_sheet(fs_path: &Path,
                         css_override: Option<&Path>,
                         verbose: bool) -> Result<Vec<u8>> {
    use xmltree::{Element, XMLNode, EmitterConfig};
    let svg_data = std::fs::read(fs_path)?;
    let data_without_bom = svg_data.as_slice().strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(svg_data.as_slice());
    let mut svg_xml = Element::parse_all(data_without_bom)?;
    let mut stylesheet_path: Option<Cow<Path>> = None;
    let mut root: Option<&mut Element> = None;
    for node in svg_xml.iter_mut() {
        match node {
            XMLNode::ProcessingInstruction(pi_name, pi_data) => {
                if pi_name == "xml-stylesheet" && pi_data.is_some() {
                    let scaffold = format!("<stylesheet {} />", pi_data.as_ref().unwrap());
                    let pi_xml = match Element::parse(scaffold.as_bytes()) {
                        Ok(xml) => { xml }
                        Err(e) => {
                            println!("{}: skipping invalid <?xml-stylesheet {}?> PI: {}",
                                     name_pd(fs_path), pi_data.as_ref().unwrap(), e);
                            continue;
                        }
                    };
                    if let Some(href) = href_from_xml_stylesheet(&pi_xml) {
                        stylesheet_path = Some((fs_path.parent().unwrap()
                            .join(Path::new(href))).into());
                        if verbose {
                            println!("{}: found xml-stylesheet {}",
                                     name_pd(fs_path),
                                     pd(stylesheet_path.as_ref().unwrap().as_ref()));
                        }
                    }
                }
            },
            XMLNode::Element(element) => { root = Some(element) }
            _ => {},
        }
    }
    if let Some(css) = css_override {
        if verbose {
            println!("{}: XML stylesheet overridden by {}", name_pd(fs_path), pd(css));
        }
        stylesheet_path = Some(css.into());
    }
    if let Some(stylesheet_path) = stylesheet_path {
        let css_data = std::fs::read_to_string(stylesheet_path)?;
        let mut style_elem = Element::new("style");
        style_elem.attributes.insert("type".into(), "text/css".into());
        style_elem.children.push(XMLNode::Text(css_data));
        root.as_deref_mut().unwrap()
            .children.insert(0, XMLNode::Element(style_elem));

        let mut new_svg_data: Vec<u8> = vec![];
        root.unwrap().write_with_config(&mut new_svg_data, EmitterConfig::new()
            .write_document_declaration(false))?;
        Ok(new_svg_data)
    } else {
        Ok(svg_data)
    }
}

impl SvgSource {
    fn load(fs_path: PathBuf, css_override: Option<&Path>, verbose: bool) -> Result<Self> {
        let svg_data = patch_xml_style_sheet(&fs_path, css_override, verbose)?;
        Ok(Self { input_path: fs_path, svg_data: Arc::new(svg_data) })
    }
}

impl AtlasSourceData {
    #[cfg(feature = "parallel")]
    fn new<'a>(_svg_trees: Vec<usvg::Tree>,
               svg_data: impl IntoIterator<Item=&'a Arc<Vec<u8>>>) -> Self {
        Self { svg_data: svg_data.into_iter().cloned().collect() }
    }

    #[cfg(not(feature = "parallel"))]
    fn new<'a>(svg_trees: Vec<usvg::Tree>,
               _svg_data: impl IntoIterator<Item=&'a Arc<Vec<u8>>>) -> Self {
        Self { svg_trees }
    }
}

impl PreparedSvgAtlas {
    fn new<'a, I>(options: AtlasOptions, sources: I) -> Result<Self>
        where I: IntoIterator<Item=&'a SvgSource> + Copy {
        let svg_options = svg_load_options(&options);
        let mut svg_trees: Vec<usvg::Tree> = vec![];
        let mut ids: Vec<String> = vec![];
        for source in sources.into_iter() {
            svg_trees.push(usvg::Tree::from_data(&source.svg_data, &svg_options)?);
            let source_id: String = source.input_path.file_stem()
                .ok_or_else(|| anyhow!("Missing file name {}", pd(&source.input_path)))?
                .to_string_lossy().into_owned();
            ids.push(source_id);
        }
        let layout = layout_atlas(svg_trees.iter(), options.buffer_px);
        if layout.items.len() != svg_trees.len() {
            bail!("Layout error: count of input images ({}) does not match layout items count ({})",
                svg_trees.len(), layout.items.len());
        }
        Ok(Self {
            atlas_options: options,
            svg_options,
            ids,
            data: AtlasSourceData::new(
                svg_trees, sources.into_iter().map(|s: &SvgSource| &s.svg_data)),
            layout,
        })
    }

    fn render_single_svg(&self, svg: &usvg::Tree, layout_box: potpack2::Box)
        -> Result<resvg::tiny_skia::Pixmap> {
        let buffer_px: f32 = self.atlas_options.buffer_px as f32;
        let mut sub_pixmap = create_pixmap(layout_box.w, layout_box.h)?;
        resvg::render(
            svg,
            usvg::FitTo::Original,
            resvg::tiny_skia::Transform::from_translate(buffer_px, buffer_px),
            sub_pixmap.as_mut(),
        ).ok_or_else(|| anyhow!("Rendering svg #{} failed", layout_box.id))?;
        Ok(sub_pixmap)
    }

    #[cfg(not(feature = "parallel"))]
    fn render(&self) -> Result<resvg::tiny_skia::Pixmap> {
        let mut pixmap = create_pixmap(self.layout.width, self.layout.height)?;
        let mut image_boxes: Vec<potpack2::Box> = self.layout.items.clone();
        image_boxes.sort_by_key(|b| b.id);
        for (svg, layout_box) in self.data.svg_trees.iter().zip(image_boxes) {
            let sub_pixmap = self.render_single_svg(svg, layout_box)?;
            // The following casts are saturating
            pixmap.draw_pixmap(
                layout_box.x as i32,
                layout_box.y as i32,
                sub_pixmap.as_ref(),
                &Default::default(),
                Default::default(),
                None,
            ).ok_or_else(|| anyhow!("Copying sub-pixmap failed"))?;
        }
        Ok(pixmap)
    }

    #[cfg(feature = "parallel")]
    fn render(&self) -> Result<resvg::tiny_skia::Pixmap> {
        use resvg::tiny_skia::Pixmap;
        let mut pixmap = create_pixmap(self.layout.width, self.layout.height)?;
        let mut image_boxes: Vec<potpack2::Box> = self.layout.items.clone();
        image_boxes.sort_by_key(|b| b.id);
        let results: Vec<_> = self.data.svg_data.par_iter().zip(image_boxes)
            .map(|(image_data, layout_box)| -> Result<(f64, f64, Pixmap)> {
                let svg_tree = usvg::Tree::from_data(image_data, &self.svg_options)?;
                let sub_pixmap = self.render_single_svg(&svg_tree, layout_box)?;
                Ok((layout_box.x, layout_box.y, sub_pixmap))
            })
            .collect::<Result<Vec<_>, _>>()?;

        for (x, y, svg_pixmap) in results.into_iter() {
            // The following casts are saturating
            pixmap.draw_pixmap(
                x as i32,
                y as i32,
                svg_pixmap.as_ref(),
                &Default::default(),
                Default::default(),
                None,
            ).ok_or_else(|| anyhow!("Copying result failed"))?;
        }

        Ok(pixmap)
    }

    fn metadata(&self) -> Result<serde_json::Value> {
        let mut result = json!({});
        for b in self.layout.items.iter() {
            result[self.ids[b.id].clone()] = json!({
                "width": b.w,
                "height": b.h,
                "x": b.x,
                "y": b.y,
                "pixelRatio": self.atlas_options.pixel_ratio,
            });
        }
        Ok(result)
    }
}

fn create_pixmap(width: f64, height: f64) -> Result<resvg::tiny_skia::Pixmap> {
    // The following casts are saturating
    let px_width: u32 = width.ceil() as u32;
    let px_height: u32 = height.ceil() as u32;
    resvg::tiny_skia::Pixmap::new(px_width, px_height)
        .ok_or_else(|| anyhow!("Pixmap creation ({}x{}) failed", px_width, px_height))
}

fn layout_atlas<'a, I: IntoIterator<Item=&'a usvg::Tree>>(
    images: I, buffer_px: f64) -> potpack2::Layout {
    let input: Vec<_> = images
        .into_iter()
        .map(|image| {
            // Ensure that there is a configurable buffer between sprites
            (image.size.width().ceil() + 2. * buffer_px,
             image.size.height().ceil() + 2. * buffer_px)
        })
        .collect();
    potpack2::Layout::new(input)
}

fn process(sources: &Vec<SvgSource>, options: AtlasOptions,
           output_base: &Path, verbose: bool) -> Result<()> {
    let atlas = PreparedSvgAtlas::new(options, sources)?;
    if verbose {
        println!("Atlas layout: {:?}", atlas.layout);
    } else {
        println!("Atlas dimensions: {}x{}",
                 atlas.layout.width.ceil(),
                 atlas.layout.height.ceil())
    }

    let json_metadata = atlas.metadata()?;
    let metadata_path = output_base.with_extension("json");
    std::fs::write(metadata_path, json_metadata.to_string())?;

    let atlas_image = atlas.render()?;
    let png_path = output_base.with_extension("png");
    println!("Saving {}", pd(&png_path));
    atlas_image.save_png(png_path)?;

    Ok(())
}

fn main() -> Result<()> {
    let args_parser: bpaf::OptionParser<cli::Config> = cli::config_parser()
        .usage(concat!("Usage: ", env!("CARGO_BIN_NAME"), " {usage}"));
    let args = args_parser.run();

    if args.output.file_name().is_none() {
        bail!("Invalid output file name: {}", pd(&args.output))
    }

    let input_files: Vec<PathBuf> = {
        let mut result = vec![];
        for path in args.svg_dirs.iter() {
            if path.is_file() {
                result.push(path.clone());
            } else {
                if !path.exists() {
                    bail!("Input path does not exist: {:?}", path)
                }

                let walker = GlobWalkerBuilder::new(path, "*.svg")
                    .max_depth(1)
                    .build()?;

                result.extend(walker
                    .into_iter()
                    .filter_map(|r| {
                        r.map_err(|err| println!("{}", err)).ok()
                            .map(|entry| entry.into_path())
                    }))
            }
        }
        result
    };

    #[cfg(feature = "parallel")]
    {
        println!("Using {} parallel threads", args.threads);
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build_global()?;
    }

    println!("Processing {} input SVG files", input_files.len());

    let svg_sources: Vec<_> = input_files.into_iter()
        .map(|file| SvgSource::load(file,
                                    args.css_override.as_deref(),
                                    args.verbose))
        .collect::<Result<Vec<_>, _>>()?;

    process(&svg_sources, AtlasOptions::new(&args, 1.0)?,
            &args.output, args.verbose)?;

    if args.with_hires {
        let mut output_base = args.output.clone();
        let mut file_name = args.output.file_stem().unwrap().to_owned();
        file_name.push("@2x");
        output_base.set_file_name(file_name);
        process(&svg_sources, AtlasOptions::new(&args, 2.0)?,
                &output_base, args.verbose)?;
    }

    Ok(())
}
