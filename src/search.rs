//! The search index of track names: a Tantivy index with the schema, tokenizer and queries of LRCLIB's server, so
//! that searches rank the same (BM25 over the normalized name, artist and album).
//!
//! The query semantics are ported from LRCLIB (<https://github.com/tranxuanthang/lrclib>, MIT, Copyright (c) 2024
//! tranxuanthang): `server/src/search_index.rs` and `server/src/search_index/query.rs`.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// At most this many results.
pub const LIMIT: usize = 20;

const TOKENIZER: &str = "lrclib_normalized";
const TRACK_ID: &str = "track_id";
const NAME: &str = "name_lower";
const ARTIST: &str = "artist_name_lower";
const ALBUM: &str = "album_name_lower";

#[derive(Clone, Copy)]
struct Fields {
    track_id: Field,
    name: Field,
    artist: Field,
    album: Field,
}

impl Fields {
    fn of(schema: &Schema) -> tantivy::Result<Self> {
        Ok(Self {
            track_id: schema.get_field(TRACK_ID)?,
            name: schema.get_field(NAME)?,
            artist: schema.get_field(ARTIST)?,
            album: schema.get_field(ALBUM)?,
        })
    }
}

fn schema() -> Schema {
    let mut builder = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    builder.add_i64_field(TRACK_ID, FAST | INDEXED);
    builder.add_text_field(NAME, text.clone());
    builder.add_text_field(ARTIST, text.clone());
    builder.add_text_field(ALBUM, text);
    builder.build()
}

fn register_tokenizer(index: &Index) {
    index.tokenizers().register(
        TOKENIZER,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build(),
    );
}

/// A track's searchable names: the dump's normalized ones.
pub struct Doc {
    pub track_id: i64,
    pub name_lower: Option<String>,
    pub artist_lower: Option<String>,
    pub album_lower: Option<String>,
}

/// Builds an index from scratch.
pub struct IndexBuilder {
    index: Index,
    writer: IndexWriter,
    fields: Fields,
}

impl IndexBuilder {
    pub fn create(dir: &Path, threads: usize, memory: usize) -> tantivy::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let index = Index::create_in_dir(dir, schema())?;
        Self::with_index(index, threads, memory)
    }

    /// Continues an index being built: what was committed is kept (its commit's payload is returned), what wasn't
    /// is dropped.
    pub fn open(
        dir: &Path,
        threads: usize,
        memory: usize,
    ) -> tantivy::Result<(Self, Option<String>)> {
        let index = Index::open_in_dir(dir)?;
        let payload = index.load_metas()?.payload;
        Ok((Self::with_index(index, threads, memory)?, payload))
    }

    fn with_index(index: Index, threads: usize, memory: usize) -> tantivy::Result<Self> {
        register_tokenizer(&index);
        let fields = Fields::of(&index.schema())?;
        let threads = threads.clamp(1, 8).min(memory / (16 << 20)).max(1);
        let writer: IndexWriter = index.writer_with_num_threads(threads, memory)?;
        // One merge at the end instead of many along the way.
        writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        Ok(Self {
            index,
            writer,
            fields,
        })
    }

    pub fn add(&mut self, doc: &Doc) -> tantivy::Result<()> {
        let mut d = TantivyDocument::default();
        d.add_i64(self.fields.track_id, doc.track_id);
        for (field, value) in [
            (self.fields.name, &doc.name_lower),
            (self.fields.artist, &doc.artist_lower),
            (self.fields.album, &doc.album_lower),
        ] {
            if let Some(value) = value
                && !value.is_empty()
            {
                d.add_text(field, value);
            }
        }
        self.writer.add_document(d)?;
        Ok(())
    }

    /// Makes what was added durable, with `payload` (read back by [`Self::open`]).
    pub fn commit(&mut self, payload: &str) -> tantivy::Result<()> {
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload(payload);
        prepared.commit()?;
        Ok(())
    }

    /// Commits and merges everything into one segment.
    pub fn finish(mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
        let segments = self.index.searchable_segment_ids()?;
        if segments.len() > 1 {
            self.writer.merge(&segments).wait()?;
        }
        self.writer.wait_merging_threads()?;
        Ok(())
    }
}

/// Searches an index.
pub struct Search {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

impl Search {
    pub fn open(dir: &Path) -> tantivy::Result<Self> {
        let index = Index::open_in_dir(dir)?;
        register_tokenizer(&index);
        let fields = Fields::of(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    /// The ids of the best matching tracks, best first, for normalized parameters (as LRCLIB's server searches):
    /// `q`, when given, must match every one of its words in the name, artist or album; otherwise `track_name` is a
    /// phrase in the name, and `artist_name` and `album_name`, when given, phrases in the artist and album.
    pub fn search(
        &self,
        q: Option<&str>,
        track_name: Option<&str>,
        artist_name: Option<&str>,
        album_name: Option<&str>,
    ) -> tantivy::Result<Vec<i64>> {
        self.page(q, track_name, artist_name, album_name, 0, LIMIT)
    }

    /// [`Self::search`], `limit` results from `offset` on (not something LRCLIB's server does: for the web search).
    pub fn page(
        &self,
        q: Option<&str>,
        track_name: Option<&str>,
        artist_name: Option<&str>,
        album_name: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> tantivy::Result<Vec<i64>> {
        let Some(query) = self.query(q, track_name, artist_name, album_name) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let top = searcher.search(
            &*query,
            &TopDocs::with_limit(limit)
                .and_offset(offset)
                .order_by_score(),
        )?;
        let mut ids = Vec::with_capacity(top.len());
        for (_, address) in top {
            let column = searcher
                .segment_reader(address.segment_ord)
                .fast_fields()
                .i64(TRACK_ID)?;
            if let Some(id) = column.first(address.doc_id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The score a track gets for a search (for comparing rankings); `None` when it doesn't match.
    pub fn score(
        &self,
        track_id: i64,
        q: Option<&str>,
        track_name: Option<&str>,
        artist_name: Option<&str>,
        album_name: Option<&str>,
    ) -> tantivy::Result<Option<f32>> {
        let Some(query) = self.query(q, track_name, artist_name, album_name) else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let by_id = TermQuery::new(
            Term::from_field_i64(self.fields.track_id, track_id),
            IndexRecordOption::Basic,
        );
        let found = searcher.search(&by_id, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, address)) = found.first() else {
            return Ok(None);
        };
        match query.explain(&searcher, *address) {
            Ok(explanation) => Ok(Some(explanation.value())),
            Err(tantivy::TantivyError::InvalidArgument(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn query(
        &self,
        q: Option<&str>,
        track_name: Option<&str>,
        artist_name: Option<&str>,
        album_name: Option<&str>,
    ) -> Option<Box<dyn Query>> {
        if let Some(q) = q {
            let tokens = self.tokenize(q);
            if tokens.is_empty() {
                return None;
            }
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = tokens
                .iter()
                .map(|token| {
                    let any_column: Box<dyn Query> = Box::new(BooleanQuery::new(
                        [self.fields.name, self.fields.artist, self.fields.album]
                            .into_iter()
                            .map(|field| (Occur::Should, term(field, token)))
                            .collect(),
                    ));
                    (Occur::Must, any_column)
                })
                .collect();
            // One word: the query is the word in any column. The score is the same (a boolean query's is the sum of
            // its clauses'), and a plain disjunction lets Tantivy skip what can't make the top results (block-max
            // WAND), where the conjunction around it scores every match.
            if let [(_, only)] = clauses.as_mut_slice() {
                return Some(std::mem::replace(
                    only,
                    Box::new(BooleanQuery::new(Vec::new())),
                ));
            }
            return Some(Box::new(BooleanQuery::new(clauses)));
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(3);
        // Every constraint given must have words: one without can't match anything.
        for (field, value) in [
            (self.fields.name, Some(track_name?)),
            (self.fields.artist, artist_name),
            (self.fields.album, album_name),
        ] {
            let Some(value) = value else { continue };
            let tokens = self.tokenize(value);
            let phrase: Box<dyn Query> = match tokens.as_slice() {
                [] => return None,
                [one] => term(field, one),
                many => Box::new(PhraseQuery::new(
                    many.iter()
                        .map(|t| Term::from_field_text(field, t))
                        .collect(),
                )),
            };
            clauses.push((Occur::Must, phrase));
        }
        Some(Box::new(BooleanQuery::new(clauses)))
    }

    fn tokenize(&self, text: &str) -> Vec<String> {
        let Some(mut analyzer) = self.index.tokenizers().get(TOKENIZER) else {
            return Vec::new();
        };
        let mut tokens = Vec::new();
        let mut stream = analyzer.token_stream(text);
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }
}

fn term(field: Field, token: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, token),
        IndexRecordOption::WithFreqs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: i64, name: &str, artist: &str, album: &str) -> Doc {
        Doc {
            track_id: id,
            name_lower: Some(name.to_owned()),
            artist_lower: Some(artist.to_owned()),
            album_lower: Some(album.to_owned()),
        }
    }

    #[test]
    fn searches_like_lrclib() {
        let dir = std::env::temp_dir().join(format!("lrclib-mirror-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = IndexBuilder::create(&dir, 1, 32 << 20).unwrap();
        b.add(&doc(1, "hello world", "artist one", "album"))
            .unwrap();
        b.add(&doc(2, "world hello", "artist two", "hello"))
            .unwrap();
        b.add(&doc(3, "something else", "hello", "")).unwrap();
        b.finish().unwrap();
        let s = Search::open(&dir).unwrap();
        let mut all = s.search(Some("hello"), None, None, None).unwrap();
        all.sort_unstable();
        assert_eq!(all, [1, 2, 3]);
        assert_eq!(
            s.search(Some("hello artist one"), None, None, None)
                .unwrap(),
            [1]
        );
        // A phrase: word order matters.
        assert_eq!(
            s.search(None, Some("hello world"), None, None).unwrap(),
            [1]
        );
        assert_eq!(
            s.search(None, Some("hello"), Some("artist two"), None)
                .unwrap(),
            [2]
        );
        assert_eq!(
            s.search(None, None, Some("hello"), None).unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(
            s.search(Some(" "), None, None, None).unwrap(),
            Vec::<i64>::new()
        );
        // Pages follow the ranking.
        let all = s.search(Some("hello"), None, None, None).unwrap();
        let mut paged = s.page(Some("hello"), None, None, None, 0, 2).unwrap();
        paged.extend(s.page(Some("hello"), None, None, None, 2, 2).unwrap());
        assert_eq!(paged, all);
        assert_eq!(
            s.page(Some("hello"), None, None, None, 3, 2).unwrap(),
            Vec::<i64>::new()
        );
        assert!(
            s.score(1, Some("hello"), None, None, None)
                .unwrap()
                .is_some()
        );
        assert!(
            s.score(3, Some("world"), None, None, None)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
