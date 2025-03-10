use std::sync::Arc;

use log::trace;
use lru::LruCache;
use skia_safe::{TextBlob, TextBlobBuilder};
use swash::shape::ShapeContext;
use swash::text::cluster::{CharCluster, Parser, Status, Token};
use swash::text::Script;
use swash::Metrics;
use unicode_segmentation::UnicodeSegmentation;

use super::font_loader::*;
use super::font_options::*;

const DEFAULT_FONT_SIZE: f32 = 14.0;

#[derive(new, Clone, Hash, PartialEq, Eq, Debug)]
struct ShapeKey {
    pub cells: Vec<String>,
    pub bold: bool,
    pub italic: bool,
}

pub struct CachingShaper {
    pub options: Option<FontOptions>,
    font_loader: FontLoader,
    blob_cache: LruCache<ShapeKey, Vec<TextBlob>>,
    shape_context: ShapeContext,
}

impl CachingShaper {
    pub fn new() -> CachingShaper {
        CachingShaper {
            options: None,
            font_loader: FontLoader::new(DEFAULT_FONT_SIZE),
            blob_cache: LruCache::new(10000),
            shape_context: ShapeContext::new(),
        }
    }

    fn current_font_pair(&mut self) -> Arc<FontPair> {
        let font_key = self
            .options
            .as_ref()
            .map(|options| options.fallback_list.first().unwrap().clone().into())
            .unwrap_or(FontKey::Default);

        self.font_loader
            .get_or_load(&font_key)
            .expect("Could not load font")
    }

    pub fn current_size(&self) -> f32 {
        self.options
            .as_ref()
            .map(|options| options.size)
            .unwrap_or(DEFAULT_FONT_SIZE)
    }

    pub fn update_font(&mut self, guifont_setting: &str) -> bool {
        let new_options = FontOptions::parse(guifont_setting, DEFAULT_FONT_SIZE);

        if new_options != self.options && new_options.is_some() {
            self.font_loader = FontLoader::new(new_options.as_ref().unwrap().size);
            self.blob_cache.clear();
            self.options = new_options;

            true
        } else {
            false
        }
    }

    fn metrics(&mut self) -> Metrics {
        let font_pair = self.current_font_pair();
        let size = self.current_size();
        let shaper = self
            .shape_context
            .builder(font_pair.swash_font.as_ref())
            .size(size)
            .build();

        shaper.metrics()
    }

    pub fn font_base_dimensions(&mut self) -> (u64, u64) {
        let metrics = self.metrics();
        let font_height = (metrics.ascent + metrics.descent + metrics.leading).ceil() as u64;
        let font_width = metrics.average_width as u64;

        (font_width, font_height)
    }

    pub fn underline_position(&mut self) -> u64 {
        self.metrics().underline_offset as u64
    }

    pub fn y_adjustment(&mut self) -> u64 {
        let metrics = self.metrics();
        (metrics.ascent + metrics.leading) as u64
    }

    fn build_clusters(&mut self, text: &str) -> Vec<(Vec<CharCluster>, Arc<FontPair>)> {
        let mut cluster = CharCluster::new();

        // Enumerate the characters storing the glyph index in the user data so that we can position
        // glyphs according to Neovim's grid rules
        let mut character_index = 0;
        let mut parser = Parser::new(
            Script::Latin,
            text.graphemes(true)
                .enumerate()
                .map(|(glyph_index, unicode_segment)| {
                    unicode_segment.chars().map(move |character| {
                        let token = Token {
                            ch: character,
                            offset: character_index as u32,
                            len: character.len_utf8() as u8,
                            info: character.into(),
                            data: glyph_index as u32,
                        };
                        character_index += 1;
                        token
                    })
                })
                .flatten(),
        );

        let mut results = Vec::new();
        'cluster: while parser.next(&mut cluster) {
            // TODO: Don't redo this work for every cluster. Save it some how
            // Create font fallback list
            let mut font_fallback_keys = Vec::new();

            // Add guifont fallback list
            if let Some(options) = &self.options {
                font_fallback_keys.extend(
                    options
                        .fallback_list
                        .iter()
                        .map(|font_name| font_name.into()),
                );
            }
            // Add default font
            font_fallback_keys.push(FontKey::Default);

            // Add skia fallback
            font_fallback_keys.push(cluster.chars()[0].ch.into());

            // Add last resort
            font_fallback_keys.push(FontKey::LastResort);

            let mut best = None;
            // Use the cluster.map function to select a viable font from the fallback list
            for fallback_key in font_fallback_keys.iter() {
                if let Some(font_pair) = self.font_loader.get_or_load(fallback_key) {
                    let charmap = font_pair.swash_font.as_ref().charmap();
                    match cluster.map(|ch| charmap.map(ch)) {
                        Status::Complete => {
                            results.push((cluster.to_owned(), font_pair.clone()));
                            continue 'cluster;
                        }
                        Status::Keep => best = Some(font_pair),
                        Status::Discard => {}
                    }
                }
            }

            if let Some(best) = best {
                // Last Resort covers all of the unicode space so we will always have a fallback
                results.push((cluster.to_owned(), best.clone()));
            }
        }

        // Now we have to group clusters by the font used so that the shaper can actually form
        // ligatures across clusters
        let mut grouped_results = Vec::new();
        let mut current_group = Vec::new();
        let mut current_font_option = None;
        for (cluster, font) in results {
            if let Some(current_font) = current_font_option.clone() {
                if current_font == font {
                    current_group.push(cluster);
                } else {
                    grouped_results.push((current_group, current_font));
                    current_group = vec![cluster];
                    current_font_option = Some(font);
                }
            } else {
                current_group = vec![cluster];
                current_font_option = Some(font);
            }
        }

        if !current_group.is_empty() {
            grouped_results.push((current_group, current_font_option.unwrap()));
        }

        grouped_results
    }

    pub fn shape(&mut self, cells: &[String]) -> Vec<TextBlob> {
        let current_size = self.current_size();
        let (glyph_width, _) = self.font_base_dimensions();

        let mut resulting_blobs = Vec::new();

        let text = cells.concat();
        trace!("Shaping text: {}", text);

        for (cluster_group, font_pair) in self.build_clusters(&text) {
            let mut shaper = self
                .shape_context
                .builder(font_pair.swash_font.as_ref())
                .size(current_size)
                .build();

            let charmap = font_pair.swash_font.as_ref().charmap();
            for mut cluster in cluster_group {
                cluster.map(|ch| charmap.map(ch));
                shaper.add_cluster(&cluster);
            }

            let mut glyph_data = Vec::new();

            shaper.shape_with(|glyph_cluster| {
                for glyph in glyph_cluster.glyphs {
                    glyph_data.push((glyph.id, glyph.data as u64 * glyph_width));
                }
            });

            if glyph_data.is_empty() {
                continue;
            }

            let mut blob_builder = TextBlobBuilder::new();
            let (glyphs, positions) =
                blob_builder.alloc_run_pos_h(&font_pair.skia_font, glyph_data.len(), 0.0, None);
            for (i, (glyph_id, glyph_x_position)) in glyph_data.iter().enumerate() {
                glyphs[i] = *glyph_id;
                positions[i] = *glyph_x_position as f32;
            }

            let blob = blob_builder.make();
            resulting_blobs.push(blob.expect("Could not create textblob"));
        }

        resulting_blobs
    }

    pub fn shape_cached(&mut self, cells: &[String], bold: bool, italic: bool) -> &Vec<TextBlob> {
        let key = ShapeKey::new(cells.to_vec(), bold, italic);

        if !self.blob_cache.contains(&key) {
            let blobs = self.shape(cells);
            self.blob_cache.put(key.clone(), blobs);
        }

        self.blob_cache.get(&key).unwrap()
    }
}
