use blitz_traits::node_id::NodeId;
use selectors::context::QuirksMode;
use std::sync::atomic::Ordering as Ao;
use std::{
    io::Cursor,
    sync::{Arc, atomic::AtomicUsize, mpsc::Sender},
};
use style::{
    font_face::{FontFaceSourceFormat, FontFaceSourceFormatKeyword, FontStyleRange, Source},
    media_queries::MediaList,
    servo_arc::Arc as ServoArc,
    shared_lock::SharedRwLock,
    shared_lock::{Locked, SharedRwLockReadGuard},
    stylesheets::{
        AllowImportRules, CssRule, DocumentStyleSheet, ImportRule, Origin, Stylesheet,
        StylesheetInDocument, StylesheetLoader as ServoStylesheetLoader, UrlExtraData,
        import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition},
    },
    values::{CssUrl, SourceLocation},
};

use blitz_traits::net::{AbortSignal, Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::ShellProvider;

use url::Url;

use crate::{document::DocumentEvent, util::ImageType};

pub(crate) fn stamped_request(url: Url, signal: Option<&AbortSignal>) -> Request {
    let mut req = Request::get(url);
    if let Some(sig) = signal {
        req = req.signal(sig.clone());
    }
    req
}

/// Carries `@font-face` descriptors from CSS parsing through to font
/// registration so `parley::Collection::register_fonts` can alias the bytes
/// under the `font-family` declared in CSS rather than whatever family name
/// the TTF's own `name` table reports.
///
/// All fields are `Option` because each descriptor is independently optional
/// at the CSS level. Missing fields fall back to the values parley reads
/// from the font's own metadata.
#[derive(Clone, Debug, Default)]
pub struct FontFaceOverrides {
    /// `font-family` descriptor (the alias the rest of the stylesheet uses).
    pub family_name: Option<String>,
    /// `font-weight` descriptor when it names a single weight; `None` for
    /// true ranges (variable faces match via their own fvar/wght axis).
    pub weight: Option<f32>,
    /// `font-style` descriptor mapped to fontique's `FontStyle`.
    pub style: Option<parley::fontique::FontStyle>,
}

#[derive(Clone, Debug)]
pub enum Resource {
    Image(ImageType, u32, u32, Arc<Vec<u8>>),
    /// Dims-only image load: width/height sniffed from
    /// the container header, payload = original encoded bytes.
    /// Used in renderer mode where paint happens out-of-isolate,
    /// so we never materialise decoded RGBA here.
    ///
    /// Trailing u64 is the embedder-supplied resource key (0 = none),
    /// and the payload is empty when the embedder kept it: see
    /// `kitesurf_set_pending_image`.
    EncodedImage(ImageType, u32, u32, Arc<Vec<u8>>, u64),
    #[cfg(feature = "svg")]
    Svg(ImageType, crate::node::SvgImageData),
    Css(DocumentStyleSheet),
    /// Stylesheet loaded for an `@import` rule, to be attached to the rule on the document thread
    ImportedCss(ServoArc<Locked<ImportRule>>, ServoArc<Stylesheet>),
    Font(Bytes, FontFaceOverrides),
    /// HTML fetched for an `<iframe>` element's `src`
    DocumentSrc(String),
    None,
}

pub(crate) struct ResourceHandler<T: Send + Sync + 'static> {
    doc_id: usize,
    request_id: usize,
    node_id: Option<NodeId>,
    tx: Sender<DocumentEvent>,
    shell_provider: Arc<dyn ShellProvider>,
    data: T,
}

impl<T: Send + Sync + 'static> ResourceHandler<T> {
    pub(crate) fn new(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Self {
        static REQUEST_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);
        Self {
            request_id: REQUEST_ID_COUNTER.fetch_add(1, Ao::Relaxed),
            doc_id,
            node_id,
            tx,
            shell_provider,
            data,
        }
    }

    pub(crate) fn boxed(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Box<dyn NetHandler>
    where
        ResourceHandler<T>: NetHandler,
    {
        Box::new(Self::new(tx, doc_id, node_id, shell_provider, data)) as _
    }

    pub(crate) fn request_id(&self) -> usize {
        self.request_id
    }

    fn respond(&self, resolved_url: String, result: Result<Resource, String>) {
        let response = ResourceLoadResponse {
            request_id: self.request_id,
            node_id: self.node_id,
            resolved_url: Some(resolved_url),
            result,
        };
        let _ = self.tx.send(DocumentEvent::ResourceLoad(response));
        self.shell_provider.request_redraw();
    }
}

#[allow(unused)]
pub struct ResourceLoadResponse {
    pub request_id: usize,
    pub node_id: Option<NodeId>,
    pub resolved_url: Option<String>,
    pub result: Result<Resource, String>,
}

pub struct StylesheetHandler {
    pub source_url: Url,
    pub guard: SharedRwLock,
    pub net_provider: Arc<dyn NetProvider>,
    pub abort_signal: Option<AbortSignal>,
}

impl NetHandler for ResourceHandler<StylesheetHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let Ok(css) = std::str::from_utf8(&bytes) else {
            return self.respond(resolved_url, Err(String::from("Invalid UTF8")));
        };

        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);
        // Legacy ::-webkit-slider-thumb -> accent-color
        // compat rewrite (no-op for sheets without the pseudos).
        let rewritten = crate::css_compat::rewrite_legacy_widget_pseudo_rules(css);
        let css: &str = rewritten.as_deref().unwrap_or(css);

        let sheet = Stylesheet::from_str(
            css,
            self.data.source_url.clone().into(),
            Origin::Author,
            ServoArc::new(self.data.guard.wrap(MediaList::empty())),
            self.data.guard.clone(),
            Some(&StylesheetLoader {
                tx: self.tx.clone(),
                doc_id: self.doc_id,
                net_provider: self.data.net_provider.clone(),
                shell_provider: self.shell_provider.clone(),
                abort_signal: self.data.abort_signal.clone(),
                import_depth: 0,
            }),
            None, // error_reporter
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        );

        self.respond(
            resolved_url,
            Ok(Resource::Css(DocumentStyleSheet(ServoArc::new(sheet)))),
        );
    }
}

/// Maximum depth of nested `@import` rules. Imports nested deeper than this
/// are refused, preventing unbounded recursion (e.g. a stylesheet whose
/// resolved `@import` URL yields another stylesheet importing in turn, with
/// the URL growing geometrically at each level).
const MAX_IMPORT_DEPTH: usize = 16;

#[derive(Clone)]
pub(crate) struct StylesheetLoader {
    pub(crate) tx: Sender<DocumentEvent>,
    pub(crate) doc_id: usize,
    pub(crate) net_provider: Arc<dyn NetProvider>,
    pub(crate) shell_provider: Arc<dyn ShellProvider>,
    pub(crate) abort_signal: Option<AbortSignal>,
    /// Depth of `@import` nesting for the stylesheet this loader is parsing
    /// (0 for a top-level stylesheet)
    pub(crate) import_depth: usize,
}
impl ServoStylesheetLoader for StylesheetLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: ServoArc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> ServoArc<Locked<ImportRule>> {
        if self.import_depth >= MAX_IMPORT_DEPTH || !supports.as_ref().is_none_or(|s| s.enabled) {
            return ServoArc::new(lock.wrap(ImportRule {
                url,
                stylesheet: ImportSheet::new_refused(),
                supports,
                layer,
                source_location: location,
            }));
        }

        let import = ImportRule {
            url,
            stylesheet: ImportSheet::new_pending(),
            supports,
            layer,
            source_location: location,
        };

        let url = import.url.url().unwrap().clone();
        let import = ServoArc::new(lock.wrap(import));
        self.net_provider.fetch(
            self.doc_id,
            stamped_request(url.as_ref().clone(), self.abort_signal.as_ref()),
            ResourceHandler::boxed(
                self.tx.clone(),
                self.doc_id,
                None, // node_id
                self.shell_provider.clone(),
                NestedStylesheetHandler {
                    url: url.clone(),
                    loader: StylesheetLoader {
                        import_depth: self.import_depth + 1,
                        ..self.clone()
                    },
                    lock: lock.clone(),
                    media,
                    import_rule: import.clone(),
                },
            ),
        );

        import
    }
}

struct NestedStylesheetHandler {
    loader: StylesheetLoader,
    lock: SharedRwLock,
    url: ServoArc<Url>,
    media: ServoArc<Locked<MediaList>>,
    import_rule: ServoArc<Locked<ImportRule>>,
}

impl NetHandler for ResourceHandler<NestedStylesheetHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let Ok(css) = std::str::from_utf8(&bytes) else {
            return self.respond(resolved_url, Err(String::from("Invalid UTF8")));
        };

        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);
        // Legacy ::-webkit-slider-thumb -> accent-color
        // compat rewrite (no-op for sheets without the pseudos).
        let rewritten = crate::css_compat::rewrite_legacy_widget_pseudo_rules(css);
        let css: &str = rewritten.as_deref().unwrap_or(css);

        let sheet = ServoArc::new(Stylesheet::from_str(
            css,
            UrlExtraData(self.data.url.clone()),
            Origin::Author,
            self.data.media.clone(),
            self.data.lock.clone(),
            Some(&self.data.loader),
            None, // error_reporter
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        ));

        // Queue @font-face faces (fetched lazily on first use;
        // see PendingFontFace). This handler runs outside any
        // document borrow, so route through the event channel.
        let faces = collect_font_faces(self.node_id, &sheet, &self.data.lock.read());
        if !faces.is_empty() {
            let _ = self.tx.send(DocumentEvent::PendingFontFaces(faces));
        }

        let import_rule = self.data.import_rule.clone();
        self.respond(resolved_url, Ok(Resource::ImportedCss(import_rule, sheet)))
    }
}

struct FontFaceHandler {
    format: FontFaceSourceFormatKeyword,
    overrides: FontFaceOverrides,
}
impl NetHandler for ResourceHandler<FontFaceHandler> {
    fn bytes(mut self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let result = self.data.parse(bytes);
        self.respond(resolved_url, result)
    }
}
impl FontFaceHandler {
    fn parse(&mut self, bytes: Bytes) -> Result<Resource, String> {
        if self.format == FontFaceSourceFormatKeyword::None && bytes.len() >= 4 {
            self.format = match &bytes.as_ref()[0..4] {
                // WOFF (v1) files begin with 0x774F4646 ('wOFF' in ascii)
                // See: <https://w3c.github.io/woff/woff1/spec/Overview.html#WOFFHeader>
                #[cfg(feature = "woff")]
                b"wOFF" => FontFaceSourceFormatKeyword::Woff,
                // WOFF2 files begin with 0x774F4632 ('wOF2' in ascii)
                // See: <https://w3c.github.io/woff/woff2/#woff20Header>
                #[cfg(feature = "woff")]
                b"wOF2" => FontFaceSourceFormatKeyword::Woff2,
                // Opentype fonts with CFF data begin with 0x4F54544F ('OTTO' in ascii)
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                b"OTTO" => FontFaceSourceFormatKeyword::Opentype,
                // Opentype fonts truetype outlines begin with 0x00010000
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                &[0x00, 0x01, 0x00, 0x00] => FontFaceSourceFormatKeyword::Truetype,
                // Truetype fonts begin with 0x74727565 ('true' in ascii)
                // See: <https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6.html#ScalerTypeNote>
                b"true" => FontFaceSourceFormatKeyword::Truetype,
                _ => FontFaceSourceFormatKeyword::None,
            }
        }

        // Satisfy rustc's mutability linting with woff feature both enabled/disabled
        #[cfg(feature = "woff")]
        let mut bytes = bytes;

        match self.format {
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff1 font");

                // Use wuff crate to decompress font
                let decompressed = wuff::decompress_woff1(&bytes).ok();

                if let Some(decompressed) = decompressed {
                    bytes = Bytes::from(decompressed);
                } else {
                    #[cfg(feature = "tracing")]
                    tracing::warn!("Failed to decompress woff1 font");
                }
            }
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff2 => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff2 font");

                // Use wuff crate to decompress font
                let decompressed = wuff::decompress_woff2(&bytes).ok();

                if let Some(decompressed) = decompressed {
                    bytes = Bytes::from(decompressed);
                } else {
                    #[cfg(feature = "tracing")]
                    tracing::warn!("Failed to decompress woff2 font");
                }
            }
            FontFaceSourceFormatKeyword::None => {
                // Should this be an error?
                return Ok(Resource::None);
            }
            _ => {}
        }

        Ok(Resource::Font(bytes, std::mem::take(&mut self.overrides)))
    }
}

/// A parsed-but-not-fetched `@font-face` source. Fonts
/// download lazily: only once some element's computed style
/// actually references the family (Chrome behavior). Eager
/// fetching multiplied network + resident decompressed-font cost
/// by declared/used on font-heavy pages (icon fonts, per-script
/// subsets, unused weights).
#[derive(Clone)]
pub struct PendingFontFace {
    /// Lowercased `font-family` descriptor for case-insensitive
    /// matching against computed styles.
    pub family_lc: String,
    pub url: Url,
    pub format: FontFaceSourceFormatKeyword,
    pub overrides: FontFaceOverrides,
    pub node_id: Option<NodeId>,
}

/// Parse a stylesheet's `@font-face` rules into pending faces.
/// Pure descriptor/source-list parsing: no network. The caller
/// queues the result on the document (directly or via a
/// DocumentEvent) and `flush_needed_font_faces` fetches on first
/// use.
pub(crate) fn collect_font_faces(
    node_id: Option<NodeId>,
    sheet: &Stylesheet,
    read_guard: &SharedRwLockReadGuard,
) -> Vec<PendingFontFace> {
    collect_font_face_rules(
        sheet.contents(read_guard).rules(read_guard).iter(),
        node_id,
        read_guard,
    )
}

/// Parse the `@font-face` rules among `rules` into pending faces (see
/// [`collect_font_faces`]).
pub(crate) fn collect_font_face_rules<'a>(
    rules: impl Iterator<Item = &'a CssRule>,
    node_id: Option<NodeId>,
    read_guard: &SharedRwLockReadGuard,
) -> Vec<PendingFontFace> {
    rules
        .filter_map(|rule| match rule {
            CssRule::FontFace(font_face) => {
                let descriptor = &font_face.read_with(read_guard).descriptors;
                let family = descriptor.font_family.as_ref()?;
                let src = descriptor.src.as_ref()?;
                // Capture the @font-face descriptors so parley can register
                // the font under the CSS-declared family name (and weight /
                // style) rather than whatever metadata the TTF reports.
                let overrides = FontFaceOverrides {
                    family_name: Some(family.name.to_string()),
                    // A true range (e.g. `100 900` on a variable
                    // face) must not pin the face to its lower bound.

                    // Passing None keeps the fvar default weight so wght-
                    // axis synthesis serves every weight in the range.
                    weight: descriptor.font_weight.as_ref().and_then(|range| {
                        let min = range.0.compute()?.value();
                        let max = range.1.compute()?.value();
                        (min == max).then_some(min)
                    }),
                    style: descriptor.font_style.as_ref().map(stylo_to_fontique_style),
                };
                let family_lc = family.name.to_string().to_lowercase();
                Some((family_lc, src, overrides))
            }
            _ => None,
        })
        .filter_map(|(family_lc, source_list, overrides)| {
            // Find the first font source in the source list that specifies a font of a type
            // that we support.
            let preferred_source = source_list
                .0
                .iter()
                .filter_map(|source| match source {
                    Source::Url(url_source) => Some(url_source),
                    // TODO: support local fonts in @font-face
                    Source::Local(_) => None,
                })
                .find_map(|url_source| {
                    let mut format = match &url_source.format_hint {
                        Some(FontFaceSourceFormat::Keyword(fmt)) => *fmt,
                        Some(FontFaceSourceFormat::String(str)) => match str.as_str() {
                            "woff2" => FontFaceSourceFormatKeyword::Woff2,
                            "ttf" => FontFaceSourceFormatKeyword::Truetype,
                            "otf" => FontFaceSourceFormatKeyword::Opentype,
                            _ => FontFaceSourceFormatKeyword::None,
                        },
                        _ => FontFaceSourceFormatKeyword::None,
                    };
                    // A url with no extension, such as a `data:` url, is
                    // sniffed from its bytes once fetched.
                    if format == FontFaceSourceFormatKeyword::None
                        && let Some((_, end)) = url_source.url.as_str().rsplit_once('.')
                    {
                        format = match end {
                            "woff2" => FontFaceSourceFormatKeyword::Woff2,
                            "woff" => FontFaceSourceFormatKeyword::Woff,
                            "ttf" => FontFaceSourceFormatKeyword::Truetype,
                            "otf" => FontFaceSourceFormatKeyword::Opentype,
                            "svg" => FontFaceSourceFormatKeyword::Svg,
                            "eot" => FontFaceSourceFormatKeyword::EmbeddedOpentype,
                            _ => FontFaceSourceFormatKeyword::None,
                        }
                    }

                    if matches!(
                        format,
                        FontFaceSourceFormatKeyword::Svg
                            | FontFaceSourceFormatKeyword::EmbeddedOpentype
                    ) {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping unsupported font of type {:?}", format);
                        return None;
                    }

                    #[cfg(not(feature = "woff"))]
                    if matches!(
                        format,
                        FontFaceSourceFormatKeyword::Woff | FontFaceSourceFormatKeyword::Woff2
                    ) {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping unsupported font of type {:?}", format);
                        return None;
                    }

                    // A relative url with no base url to resolve against
                    // yields None; skip the source instead of panicking
                    let Some(url) = url_source.url.url() else {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping @font-face source with unresolvable url");
                        return None;
                    };
                    let url = url.as_ref().clone();
                    Some((url, format))
                });

            preferred_source.map(|(url, format)| PendingFontFace {
                family_lc,
                url,
                format,
                overrides,
                node_id,
            })
        })
        .collect()
}

/// Perform the network fetch for a pending face whose
/// family became used. The tail of the old eager fetch_font_face.
pub(crate) fn fetch_pending_font_face(
    tx: Sender<DocumentEvent>,
    doc_id: usize,
    network_provider: &Arc<dyn NetProvider>,
    shell_provider: &Arc<dyn ShellProvider>,
    abort_signal: Option<&AbortSignal>,
    face: PendingFontFace,
) {
    // Tag with content_type = "font/*" so the
    // outbound gateway routes through the Font cross-
    // origin bypass (matching the opaque-resource path
    // real browsers take for @font-face). Without this
    // tag, KitesurfNetProvider falls back to URL-
    // extension inference, which fails for
    // content-negotiating CDNs (a font CDN may serve
    // /<hash>.woff2 with no extension hint to
    // the JS fetch bridge).
    let mut req = stamped_request(face.url, abort_signal);
    req.content_type = Some("font/*".to_string());
    network_provider.fetch(
        doc_id,
        req,
        ResourceHandler::boxed(
            tx,
            doc_id,
            face.node_id,
            shell_provider.clone(),
            FontFaceHandler { format: face.format, overrides: face.overrides },
        ),
    );
}

/// Translate stylo's `@font-face` `font-style` descriptor into the fontique
/// `FontStyle` enum used by parley. Stylo encodes Italic and Oblique-with-
/// angle distinctly; CSS's bare `normal` is parsed as `Oblique(0deg, 0deg)`
/// by stylo (see the `FontStyle::parse` impl in stylo's `font_face.rs`), so
/// that pattern is treated as `Normal` here.
fn stylo_to_fontique_style(style: &FontStyleRange) -> parley::fontique::FontStyle {
    use parley::fontique::FontStyle as Fq;
    match style {
        FontStyleRange::Italic => Fq::Italic,
        FontStyleRange::Oblique(min, max) => {
            let angle = min.degrees();
            // Stylo emits `Oblique(0deg, 0deg)` for the literal CSS `normal`
            // keyword. Map that back to `Normal` so parley's font matching
            // doesn't misclassify upright fonts.
            if angle.is_none_or(|a| a == 0.0) && max.degrees().is_none_or(|a| a == 0.0) {
                Fq::Normal
            } else {
                Fq::Oblique(angle)
            }
        }
    }
}

/// Handles HTML fetched for an `<iframe>` element's `src`
pub(crate) struct DocumentSrcHandler;

impl NetHandler for ResourceHandler<DocumentSrcHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        self.respond(resolved_url, Ok(Resource::DocumentSrc(html)));
    }
}

pub struct ImageHandler {
    kind: ImageType,
    /// Maximum dimension of the returned raster. The source image is fully
    /// decoded before resizing, so this does not limit peak decoder memory.
    /// `None` uses `DEFAULT_DECODE_MAX_DIM`.
    decode_max_dim: Option<u32>,
    /// When true, a readable header yields `Resource::EncodedImage` with the
    /// original bytes and no pixel decode. An unreadable header falls back to
    /// normal decoding; renderer mode decodes retained bytes in another isolate.
    dims_only: bool,
    /// URL fragment from the image src (`img.svg#viewname`),
    /// applied as an SVG view selection before the usvg parse.
    view_fragment: Option<String>,
}

/// Default maximum dimension of the returned raster when the `<img>` has no
/// `width` or `height` attribute. At 600 × 600 × 4 bytes, one image can retain
/// up to 1,440,000 bytes of RGBA8 data; this is not a page-wide memory budget.
const DEFAULT_DECODE_MAX_DIM: u32 = 600;

impl ImageHandler {
    pub fn new(kind: ImageType) -> Self {
        Self { kind, decode_max_dim: None, dims_only: false, view_fragment: None }
    }

    /// Dims-only handler for renderer mode. See the
    /// `dims_only` field doc.
    pub fn new_dims_only(kind: ImageType) -> Self {
        Self { kind, decode_max_dim: None, dims_only: true, view_fragment: None }
    }

    /// Remember the src URL's fragment so an SVG body can
    /// honour its `<view>` selection (SVG2 linking.html#ViewElement).
    pub fn with_view_fragment(mut self, fragment: Option<&str>) -> Self {
        self.view_fragment = fragment
            .filter(|f| !f.is_empty())
            .map(|f| f.to_string());
        self
    }

    /// Set the maximum dimension of the returned raster. The source is fully
    /// decoded before this cap is applied.
    pub fn with_max_dim(kind: ImageType, max_dim: u32) -> Self {
        // Clamp to a sane floor (16 px): pages occasionally set
        // width="0" or width="1" placeholders to hide images, and
        // we don't want to decode at 1×1 only to upscale on paint.
        let cap = max_dim.max(16);
        Self {
            kind,
            decode_max_dim: Some(cap),
            dims_only: false,
            view_fragment: None,
        }
    }
}

impl NetHandler for ResourceHandler<ImageHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let result = self.data.parse(bytes);
        self.respond(resolved_url, result)
    }
}

thread_local! {
    /// Resource key and, when the embedder sniffed them,
    /// dimensions for the delivery currently being parsed. Set right
    /// before handing bytes to a stashed handler; `parse` runs
    /// synchronously inside that call, so a thread-local is enough and
    /// no trait signature has to change.
    static KITESURF_PENDING_IMAGE: std::cell::Cell<(u64, u32, u32)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

/// Stage the key + dimensions the next dims-only image parse
/// will adopt. A non-zero key means the embedder owns this resource's
/// identity, which is what lets a consumer holding no bytes name it.
/// Non-zero dimensions mean the payload is absent and these are
/// authoritative: layout reads nothing else from an image.
///
/// Always clear it (0, 0, 0) after the delivery, so an unkeyed load
/// cannot inherit a stale id or, worse, stale dimensions.
pub fn kitesurf_set_pending_image(key: u64, width: u32, height: u32) {
    KITESURF_PENDING_IMAGE.with(|k| k.set((key, width, height)));
}

fn kitesurf_take_pending_image() -> (u64, u32, u32) {
    KITESURF_PENDING_IMAGE.with(|k| k.get())
}

const MAX_IMAGE_RGBA_BYTES: u64 = 128 * 1024 * 1024;
// 16-bit RGBA sources need twice the RGBA8 policy budget during decode.
const MAX_IMAGE_DECODE_BYTES: u64 = MAX_IMAGE_RGBA_BYTES * 2;

pub(crate) fn decode_image_bounded(bytes: &[u8]) -> image::ImageResult<image::DynamicImage> {
    decode_image_with_limits(bytes, MAX_IMAGE_DECODE_BYTES, MAX_IMAGE_RGBA_BYTES)
}

fn rgba_fits((width, height): (u32, u32), max_rgba_bytes: u64) -> bool {
    u128::from(width) * u128::from(height) * 4 <= u128::from(max_rgba_bytes)
}

fn decode_image_with_limits(
    bytes: &[u8],
    max_decode_bytes: u64,
    max_rgba_bytes: u64,
) -> image::ImageResult<image::DynamicImage> {
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?
        .into_dimensions()?;
    if !rgba_fits(dimensions, max_rgba_bytes) {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }

    let mut limits = image::Limits::default();
    limits.max_alloc = Some(max_decode_bytes);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?;
    reader.limits(limits);
    reader.decode()
}

impl ImageHandler {
    fn parse(&self, bytes: Bytes) -> Result<Resource, String> {
        // Renderer-mode fast path: header-only dimension
        // sniff, retain the encoded container bytes for the out-of-
        // isolate rasterizer. Falls through to the normal decode
        // (and the SVG fallback) when the header can't be read.
        if self.dims_only {
            // Embedder-supplied dimensions: the payload is absent by
            // design, so there is nothing to sniff and nothing to keep.
            let (staged_key, staged_w, staged_h) = kitesurf_take_pending_image();
            if staged_w > 0 && staged_h > 0 {
                return Ok(Resource::EncodedImage(
                    self.kind,
                    staged_w,
                    staged_h,
                    Arc::new(Vec::new()),
                    staged_key,
                ));
            }
            if let Ok((w, h)) = image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
                .expect("IO errors impossible with Cursor")
                .into_dimensions()
            {
                if w > 0 && h > 0 {
                    return Ok(Resource::EncodedImage(
                        self.kind,
                        w,
                        h,
                        Arc::new(bytes.to_vec()),
                        staged_key,
                    ));
                }
            }
        }
        let image_err = match decode_image_bounded(&bytes) {
            Ok(image) => {
                // The source image is already decoded. Resize before RGBA conversion to limit
                // the dimensions and storage of the raster returned to the document.
                let cap = self.decode_max_dim.unwrap_or(DEFAULT_DECODE_MAX_DIM);
                let (w, h) = (image.width(), image.height());
                let scaled = if w > cap || h > cap {
                    use image::imageops::FilterType;
                    image.resize(cap, cap, FilterType::Triangle)
                } else {
                    image
                };
                let final_w = scaled.width();
                let final_h = scaled.height();
                let raw_rgba8_data = scaled.into_rgba8().into_raw();
                return Ok(Resource::Image(
                    self.kind,
                    final_w,
                    final_h,
                    Arc::new(raw_rgba8_data),
                ));
            }
            Err(e) => e.to_string(),
        };

        #[cfg(feature = "svg")]
        let svg_err = {
            use crate::util::{apply_svg_target_fragment, apply_svg_view_fragment, parse_svg_image};
            // `img.svg#viewname`: the named <view>'s viewBox overrides the
            // root's before the parse; unmatched fragments fall through.
            // `sprite.svg#icon`: the fragment is also what `:target`
            // matches in the sprite's own stylesheet.
            let rewritten: Option<Vec<u8>> = self
                .view_fragment
                .as_deref()
                .and_then(|frag| {
                    let source = std::str::from_utf8(&bytes).ok()?;
                    let viewed = apply_svg_view_fragment(source, frag);
                    let base = viewed.as_deref().unwrap_or(source);
                    apply_svg_target_fragment(base, frag).or(viewed)
                })
                .map(String::into_bytes);
            match parse_svg_image(rewritten.as_deref().unwrap_or(&bytes)) {
                Ok(svg) => return Ok(Resource::Svg(self.kind, svg)),
                Err(e) => e.to_string(),
            }
        };
        #[cfg(not(feature = "svg"))]
        let svg_err = "svg feature disabled";

        Err(format!(
            "Could not parse image ({} bytes): image-crate error: {image_err}; svg fallback error: {svg_err}",
            bytes.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use parley::fontique::FontStyle as Fq;
    use style::values::specified::Angle;

    fn rgba16_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[0; 8], 1, 1, image::ExtendedColorType::Rgba16)
            .unwrap();
        bytes
    }

    #[test]
    fn bounded_image_decode_limits_decoder_and_rgba_allocations() {
        let bytes = rgba16_png();
        let decoded = decode_image_with_limits(&bytes, 1024, 4).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgba16);

        assert!(matches!(
            decode_image_with_limits(&bytes, 4, 4),
            Err(image::ImageError::Limits(_))
        ));
        assert!(matches!(
            decode_image_with_limits(&bytes, 1024, 3),
            Err(image::ImageError::Limits(_))
        ));
    }

    #[test]
    fn bounded_image_decode_budget_covers_32_megapixel_rgba16_sources() {
        let dimensions = (8192, 4096);
        assert!(rgba_fits(dimensions, MAX_IMAGE_RGBA_BYTES));
        assert!(
            MAX_IMAGE_DECODE_BYTES >= u64::from(dimensions.0) * u64::from(dimensions.1) * 8
        );
        assert!(!rgba_fits((8192, 4097), MAX_IMAGE_RGBA_BYTES));
        assert!(!rgba_fits((u32::MAX, u32::MAX), MAX_IMAGE_RGBA_BYTES));
    }

    fn oblique(min_deg: f32, max_deg: f32) -> FontStyleRange {
        FontStyleRange::Oblique(Angle::from_degrees(min_deg), Angle::from_degrees(max_deg))
    }

    #[test]
    fn italic_maps_to_italic() {
        assert_eq!(stylo_to_fontique_style(&FontStyleRange::Italic), Fq::Italic,);
    }

    #[test]
    fn oblique_zero_zero_maps_to_normal() {
        // Stylo parses bare CSS `normal` as `Oblique(0deg, 0deg)`; the
        // helper must round-trip that back to `FontStyle::Normal` so
        // parley's matching doesn't misclassify upright fonts.
        assert_eq!(stylo_to_fontique_style(&oblique(0.0, 0.0)), Fq::Normal);
    }

    #[test]
    fn oblique_single_angle_maps_to_oblique_with_min() {
        assert_eq!(
            stylo_to_fontique_style(&oblique(14.0, 14.0)),
            Fq::Oblique(Some(14.0)),
        );
    }

    #[test]
    fn oblique_range_uses_min_angle() {
        // For a range, fontique's single-angle representation takes the
        // lower bound — confirm `min` (not `max`) is what gets through.
        assert_eq!(
            stylo_to_fontique_style(&oblique(10.0, 20.0)),
            Fq::Oblique(Some(10.0)),
        );
    }
}
