//! Compact file index for efficient storage, transfer, and fuzzy search.
//!
//! # Architecture Overview
//!
//! The file index is designed for three use cases:
//! 1. **Memory-efficient storage**: Path segment interning reduces memory by ~60-80%
//! 2. **Fast network transfer**: Binary format and zstd compression
//! 3. **Incremental updates**: Delta encoding for fs_notify events
//!
//! ## OS Path Support
//!
//! The index uses `bstr` to store path segments as arbitrary byte sequences, supporting OS-native paths that may not be valid UTF-8:
//! - **Unix**: Paths can contain any byte except NUL (stored directly)
//! - **Windows**: UTF-16 paths are converted to UTF-8 (lossy for invalid sequences)
//!
//! Use `iter_bstr()` and `reconstruct_path_bstr()` for lossless byte access.
//!
//! ## Data Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     FileIndex (in memory)                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  StringInterner                                                  │
//! │  ┌─────────────────────────────────────────────────────────────┐│
//! │  │ arena: Vec<u8>  ["src", "lib", "main.rs", ...]  (contiguous)││
//! │  │ offsets: Vec<(u32, u16)>  // (start, len) indexed by SegmentId│
//! │  │ lookup: U64NoHashMap<SmallVec<[SegmentId; 1]>>  // O(1)     ││
//! │  └─────────────────────────────────────────────────────────────┘│
//! │                                                                  │
//! │  entries: Vec<FileEntry>                                         │
//! │  ┌─────────────────────────────────────────────────────────────┐│
//! │  │ FileEntry { segments: SmallVec<[SegmentId; 6]>, flags: u8 } ││
//! │  │ ...                                                          ││
//! │  └─────────────────────────────────────────────────────────────┘│
//! │                                                                  │
//! │  path_to_idx: FxHashMap<PathKey, usize>  // for O(1) removal     │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Wire Format (Binary)
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ Header (16 bytes)                                                 │
//! │ ┌──────────┬──────────┬────────────┬────────────┬──────────────┐ │
//! │ │ magic(4) │ ver(2)   │ flags(2)   │ n_segs(4)  │ n_entries(4) │ │
//! │ │ "FIDX"   │ 0x0001   │ compressed │            │              │ │
//! │ └──────────┴──────────┴────────────┴────────────┴──────────────┘ │
//! ├──────────────────────────────────────────────────────────────────┤
//! │ Segment Table (variable)                                          │
//! │ ┌────────────────────────────────────────────────────────────────┐│
//! │ │ For each segment:                                              ││
//! │ │   len: u16                                                     ││
//! │ │   data: [u8; len]  // arbitrary bytes, no null terminator      ││
//! │ └────────────────────────────────────────────────────────────────┘│
//! ├──────────────────────────────────────────────────────────────────┤
//! │ Entry Table (variable)                                            │
//! │ ┌────────────────────────────────────────────────────────────────┐│
//! │ │ For each entry:                                                ││
//! │ │   flags: u8        // bit 0 = is_dir                           ││
//! │ │   depth: u8        // number of segments (max 255)             ││
//! │ │   segments: [u32; depth]  // segment IDs                       ││
//! │ └────────────────────────────────────────────────────────────────┘│
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Complexity
//!
//! | Operation       | Complexity | Notes                              |
//! |-----------------|------------|------------------------------------ |
//! | `insert()`      | O(k)       | k = path depth (typically 3-6)     |
//! | `remove()`      | O(k)       | k = path depth                     |
//! | `contains()`    | O(k)       | k = path depth                     |
//! | `intern()`      | O(1) avg   | Hash-based lookup                  |
//! | `get_id()`      | O(1) avg   | Hash-based lookup                  |
//! | `from_walk()`   | O(n)       | n = number of files                |
//! | `to_bytes()`    | O(n)       | n = number of entries              |
//! | `from_bytes()`  | O(n)       | n = number of entries              |
//!
//! ## Memory Estimates
//!
//! For a typical project with 10,000 files:
//! - Naive (full paths): ~500KB (avg 50 bytes/path)
//! - Interned: ~150KB (segments shared, ~15 bytes/entry)
//! - Compressed wire: ~50KB (zstd ratio ~3:1 for paths)
//!
//! ## Example Usage
//!
//! ```ignore
//! // Build index from directory walk
//! let index = FileIndex::from_walk("/workspace")?;
//!
//! // Or build incrementally
//! let mut index = FileIndex::new();
//! index.insert("src/main.rs", false);
//! index.insert("src/lib.rs", false);
//!
//! // Serialize for network transfer
//! let bytes = index.to_bytes_compressed()?;
//! send_to_client(bytes);
//!
//! // On client: deserialize
//! let index = FileIndex::from_bytes(&bytes)?;
//!
//! // Iterate with lossy String conversion
//! for (path, is_dir) in index.iter() {
//!     println!("{}", path);
//! }
//!
//! // Or iterate with lossless BString for non-UTF-8 paths
//! for (path, is_dir) in index.iter_bstr() {
//!     fuzzy_finder.inject(path.as_bytes(), is_dir);
//! }
//! ```

use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::path::Path;

use bstr::{BStr, BString};
use hashbrown::HashMap;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nohash_hasher::BuildNoHashHasher;
use rustc_hash::FxHasher;

/// HashMap whose u64 keys are already hashes, so `NoHashHasher` skips re-hashing.
type U64NoHashMap<V> = HashMap<u64, V, BuildNoHashHasher<u64>>;

type FxBuildHasher = std::hash::BuildHasherDefault<FxHasher>;

type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

const MAGIC_INDEX: &[u8; 4] = b"FIDX";
#[allow(dead_code)] // Reserved for delta wire format
const MAGIC_DELTA: &[u8; 4] = b"FDLT";
const VERSION: u16 = 1;

const FLAG_COMPRESSED: u16 = 0x0001;

const ENTRY_FLAG_IS_DIR: u8 = 0x01;

#[allow(dead_code)] // Documentation constant
const MAX_DEPTH: usize = 255;

/// Number of threads to use for parallel directory walking.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8) // Cap at 8 to avoid excessive parallelism
}

/// Options for building a FileIndex from a directory walk.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Whether to respect `.gitignore` files (default: true)
    pub respect_gitignore: bool,
    /// Whether to respect `.ignore` files (default: true)
    pub respect_ignore: bool,
    /// Whether to skip hidden files/directories (default: true)
    pub skip_hidden: bool,
    /// Maximum depth to walk (None = unlimited)
    pub max_depth: Option<usize>,
    /// Whether to use parallel walking (default: true)
    pub parallel: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            respect_ignore: true,
            skip_hidden: true,
            max_depth: None,
            parallel: true,
        }
    }
}

impl WalkOptions {
    pub fn include_all() -> Self {
        Self {
            respect_gitignore: false,
            respect_ignore: false,
            skip_hidden: false,
            max_depth: None,
            parallel: true,
        }
    }

    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    pub fn with_gitignore(mut self, respect: bool) -> Self {
        self.respect_gitignore = respect;
        self
    }

    pub fn with_hidden(mut self, skip: bool) -> Self {
        self.skip_hidden = skip;
        self
    }
}

/// Handle to an interned path segment; u32 allows up to 4 billion unique segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentId(u32);

impl SegmentId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Arena-based string interner for path segments: one contiguous buffer, O(1) average lookup.
/// Segments are arbitrary bytes so non-UTF-8 OS paths are stored losslessly.
#[derive(Debug, Clone)]
pub struct StringInterner {
    /// Contiguous storage for all interned byte strings
    arena: Vec<u8>,
    /// Maps a segment's hash to the ids sharing it; most buckets hold exactly one.
    lookup: U64NoHashMap<smallvec::SmallVec<[SegmentId; 1]>>,
    /// Maps SegmentId to (start, len) in arena
    offsets: Vec<(u32, u16)>,
}

impl Default for StringInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl StringInterner {
    pub fn new() -> Self {
        Self {
            arena: Vec::new(),
            lookup: U64NoHashMap::default(),
            offsets: Vec::new(),
        }
    }

    pub fn with_capacity(string_bytes: usize, num_strings: usize) -> Self {
        Self {
            arena: Vec::with_capacity(string_bytes),
            lookup: U64NoHashMap::with_capacity_and_hasher(
                num_strings,
                BuildNoHashHasher::default(),
            ),
            offsets: Vec::with_capacity(num_strings),
        }
    }

    /// Intern a byte string, returning its existing id if already present.
    /// O(1) average; worst case is the hash-collision chain.
    pub fn intern_bytes(&mut self, s: &[u8]) -> SegmentId {
        let hash = Self::hash_bytes(s);

        // Check if already interned
        if let Some(ids) = self.lookup.get(&hash) {
            // Check each ID with this hash (usually just one)
            for &id in ids {
                if self.get_bytes(id) == Some(s) {
                    return id;
                }
            }
            // Hash collision: same hash but different string
            // Fall through to add new entry
        }

        // Not found, add new
        let start = self.arena.len() as u32;
        let len = s.len() as u16;

        self.arena.extend_from_slice(s);

        let id = SegmentId::new(self.offsets.len() as u32);
        self.offsets.push((start, len));

        self.lookup.entry(hash).or_default().push(id);

        id
    }

    /// Intern a UTF-8 string. Convenience wrapper around `intern_bytes`.
    pub fn intern(&mut self, s: &str) -> SegmentId {
        self.intern_bytes(s.as_bytes())
    }

    /// Intern an OS string (path component). Handles platform-specific encoding.
    #[cfg(unix)]
    pub fn intern_os(&mut self, s: &OsStr) -> SegmentId {
        use std::os::unix::ffi::OsStrExt;
        self.intern_bytes(s.as_bytes())
    }

    #[cfg(windows)]
    pub fn intern_os(&mut self, s: &OsStr) -> SegmentId {
        // On Windows, use lossy UTF-8 conversion since Windows paths are UTF-16 and we need a byte representation for storage
        self.intern_bytes(s.to_string_lossy().as_bytes())
    }

    /// SegmentId for a byte string without interning it, or `None` if absent. O(1) average.
    pub fn get_bytes_id(&self, s: &[u8]) -> Option<SegmentId> {
        let hash = Self::hash_bytes(s);

        if let Some(ids) = self.lookup.get(&hash) {
            for &id in ids {
                if self.get_bytes(id) == Some(s) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// Get the SegmentId for a UTF-8 string without interning it.
    pub fn get_id(&self, s: &str) -> Option<SegmentId> {
        self.get_bytes_id(s.as_bytes())
    }

    /// Get the SegmentId for an OS string without interning it.
    #[cfg(unix)]
    pub fn get_os_id(&self, s: &OsStr) -> Option<SegmentId> {
        use std::os::unix::ffi::OsStrExt;
        self.get_bytes_id(s.as_bytes())
    }

    #[cfg(windows)]
    pub fn get_os_id(&self, s: &OsStr) -> Option<SegmentId> {
        self.get_bytes_id(s.to_string_lossy().as_bytes())
    }

    /// Get the raw bytes for a SegmentId.
    ///
    /// Complexity: O(1)
    pub fn get_bytes(&self, id: SegmentId) -> Option<&[u8]> {
        let (start, len) = *self.offsets.get(id.0 as usize)?;
        self.arena
            .get(start as usize..(start as usize + len as usize))
    }

    /// Get the bytes as a BStr for a SegmentId.
    ///
    /// Complexity: O(1)
    pub fn get_bstr(&self, id: SegmentId) -> Option<&BStr> {
        self.get_bytes(id).map(BStr::new)
    }

    /// Get the string for a SegmentId, if it's valid UTF-8.
    ///
    /// Complexity: O(1)
    pub fn get(&self, id: SegmentId) -> Option<&str> {
        self.get_bytes(id).and_then(|b| std::str::from_utf8(b).ok())
    }

    /// Number of interned segments.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Total bytes used by the arena.
    pub fn arena_bytes(&self) -> usize {
        self.arena.len()
    }

    fn hash_bytes(s: &[u8]) -> u64 {
        let mut hasher = FxHasher::default();
        s.hash(&mut hasher);
        hasher.finish()
    }

    /// Iterate over all segments with their IDs as BStr.
    pub fn iter(&self) -> impl Iterator<Item = (SegmentId, &BStr)> {
        self.offsets
            .iter()
            .enumerate()
            .filter_map(|(idx, &(start, len))| {
                let bytes = self
                    .arena
                    .get(start as usize..(start as usize + len as usize))?;
                Some((SegmentId::new(idx as u32), BStr::new(bytes)))
            })
    }

    /// Iterate over all segments with their IDs, only yielding valid UTF-8.
    pub fn iter_utf8(&self) -> impl Iterator<Item = (SegmentId, &str)> {
        self.offsets
            .iter()
            .enumerate()
            .filter_map(|(idx, &(start, len))| {
                let bytes = self
                    .arena
                    .get(start as usize..(start as usize + len as usize))?;
                let s = std::str::from_utf8(bytes).ok()?;
                Some((SegmentId::new(idx as u32), s))
            })
    }
}

/// Paths up to 6 segments store inline (covers 99% of cases); deeper paths heap-allocate.
const INLINE_SEGMENTS: usize = 6;

#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Path segments (e.g., ["src", "lib", "foo.rs"])
    segments: smallvec::SmallVec<[SegmentId; INLINE_SEGMENTS]>,
    /// Entry flags (is_dir, etc.)
    flags: u8,
}

impl FileEntry {
    pub fn new(
        segments: impl Into<smallvec::SmallVec<[SegmentId; INLINE_SEGMENTS]>>,
        is_dir: bool,
    ) -> Self {
        let mut flags = 0;
        if is_dir {
            flags |= ENTRY_FLAG_IS_DIR;
        }
        Self {
            segments: segments.into(),
            flags,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.flags & ENTRY_FLAG_IS_DIR != 0
    }

    pub fn segments(&self) -> &[SegmentId] {
        &self.segments
    }

    pub fn depth(&self) -> usize {
        self.segments.len()
    }
}

/// Uses the segment IDs directly for fast comparison.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathKey(smallvec::SmallVec<[SegmentId; INLINE_SEGMENTS]>);

impl From<&[SegmentId]> for PathKey {
    fn from(segments: &[SegmentId]) -> Self {
        Self(segments.into())
    }
}

#[derive(Debug, Clone)]
pub struct FileIndex {
    interner: StringInterner,
    entries: Vec<FileEntry>,
    /// Path to entry index lookup (for O(1) removal)
    path_to_idx: FxHashMap<PathKey, usize>,
    /// Tracks removed indices for potential compaction
    removed_count: usize,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndex {
    pub fn new() -> Self {
        Self {
            interner: StringInterner::new(),
            entries: Vec::new(),
            path_to_idx: FxHashMap::default(),
            removed_count: 0,
        }
    }

    pub fn with_capacity(num_entries: usize) -> Self {
        Self {
            interner: StringInterner::with_capacity(num_entries * 20, num_entries),
            entries: Vec::with_capacity(num_entries),
            path_to_idx: FxHashMap::with_capacity_and_hasher(num_entries, FxBuildHasher::default()),
            removed_count: 0,
        }
    }

    /// Build an index by walking a directory, respecting ignore patterns via the `ignore` crate.
    /// Excludes `.git` by default.
    pub fn from_walk(root: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_walk_with_options(root, WalkOptions::default())
    }

    /// Build an index by walking a directory with custom options.
    pub fn from_walk_with_options(
        root: impl AsRef<Path>,
        options: WalkOptions,
    ) -> io::Result<Self> {
        let root = root.as_ref();
        let root_canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

        let mut builder = WalkBuilder::new(&root_canonical);
        builder
            .follow_links(false)
            .git_ignore(options.respect_gitignore)
            .git_global(options.respect_gitignore)
            .git_exclude(options.respect_gitignore)
            .ignore(options.respect_ignore)
            .hidden(options.skip_hidden)
            .require_git(false)
            .max_depth(options.max_depth);

        // Always exclude .git directory
        if let Ok(overrides) = OverrideBuilder::new(&root_canonical)
            .add("!.git")
            .and_then(|b| b.build())
        {
            builder.overrides(overrides);
        }

        if options.parallel {
            builder.threads(num_cpus());
        }

        let mut index = FileIndex::new();

        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if path == root_canonical {
                continue;
            }

            let file_type = match entry.file_type() {
                Some(ft) => ft,
                None => continue,
            };

            if !file_type.is_file() && !file_type.is_dir() {
                continue;
            }

            let rel_path = match path.strip_prefix(&root_canonical) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if rel_path.as_os_str().is_empty() {
                continue;
            }

            index.insert(rel_path, file_type.is_dir());
        }

        Ok(index)
    }

    pub fn insert(&mut self, path: impl AsRef<Path>, is_dir: bool) {
        let segments = self.intern_path(path.as_ref());
        let key = PathKey::from(segments.as_slice());

        if self.path_to_idx.contains_key(&key) {
            return;
        }

        let idx = self.entries.len();
        self.entries.push(FileEntry::new(segments.clone(), is_dir));
        self.path_to_idx.insert(key, idx);
    }

    /// Returns true if the path was found and removed.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> bool {
        let segments = self.intern_path(path.as_ref());
        let key = PathKey::from(segments.as_slice());

        if let Some(idx) = self.path_to_idx.remove(&key) {
            // Mark as removed by clearing segments (tombstone)
            if let Some(entry) = self.entries.get_mut(idx) {
                entry.segments.clear();
            }
            self.removed_count += 1;

            if self.removed_count > self.entries.len() / 4 {
                self.compact();
            }
            true
        } else {
            false
        }
    }

    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        let segments = self.path_segments(path.as_ref());
        let key = PathKey::from(segments.as_slice());
        self.path_to_idx.contains_key(&key)
    }

    /// Number of entries (excluding tombstones).
    pub fn len(&self) -> usize {
        self.entries.len() - self.removed_count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of unique path segments.
    pub fn num_segments(&self) -> usize {
        self.interner.len()
    }

    /// Iterate over all (path, is_dir) pairs as BString (preserves non-UTF-8).
    pub fn iter_bstr(&self) -> impl Iterator<Item = (BString, bool)> + '_ {
        self.entries
            .iter()
            .filter(|e| !e.segments.is_empty())
            .map(|e| {
                let path = self.reconstruct_path_bstr(&e.segments);
                (path, e.is_dir())
            })
    }

    /// Iterate over all (path, is_dir) pairs as String (lossy for non-UTF-8).
    pub fn iter(&self) -> impl Iterator<Item = (String, bool)> + '_ {
        self.entries
            .iter()
            .filter(|e| !e.segments.is_empty())
            .map(|e| {
                let path = self.reconstruct_path(&e.segments);
                (path, e.is_dir())
            })
    }

    /// Iterate over entries with raw access (for nucleo injection).
    pub fn iter_entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| !e.segments.is_empty())
    }

    /// Reconstruct a path as a BString (supports non-UTF-8 paths).
    pub fn reconstruct_path_bstr(&self, segments: &[SegmentId]) -> BString {
        let mut path = BString::new(Vec::new());
        for (i, &seg_id) in segments.iter().enumerate() {
            if i > 0 {
                path.push(b'/');
            }
            if let Some(seg) = self.interner.get_bytes(seg_id) {
                path.extend_from_slice(seg);
            }
        }
        path
    }

    /// Reconstruct a path as a String (lossy conversion for non-UTF-8).
    pub fn reconstruct_path(&self, segments: &[SegmentId]) -> String {
        self.reconstruct_path_bstr(segments).to_string()
    }

    pub fn get_segment_bytes(&self, id: SegmentId) -> Option<&[u8]> {
        self.interner.get_bytes(id)
    }

    pub fn get_segment_bstr(&self, id: SegmentId) -> Option<&BStr> {
        self.interner.get_bstr(id)
    }

    /// Get a segment as str by id (returns None for non-UTF-8).
    pub fn get_segment(&self, id: SegmentId) -> Option<&str> {
        self.interner.get(id)
    }

    fn intern_path(&mut self, path: &Path) -> smallvec::SmallVec<[SegmentId; INLINE_SEGMENTS]> {
        path.components()
            .map(|c| c.as_os_str())
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| self.interner.intern_os(s))
            .collect()
    }

    fn path_segments(&self, path: &Path) -> smallvec::SmallVec<[SegmentId; INLINE_SEGMENTS]> {
        // For lookup only: doesn't intern new segments
        // Uses O(1) get_os_id instead of O(N) iteration
        path.components()
            .map(|c| c.as_os_str())
            .filter(|s| !s.is_empty() && *s != ".")
            .filter_map(|s| self.interner.get_os_id(s))
            .collect()
    }

    fn compact(&mut self) {
        // Rebuild entries without tombstones
        let mut new_entries = Vec::with_capacity(self.entries.len() - self.removed_count);
        let mut new_path_to_idx =
            FxHashMap::with_capacity_and_hasher(new_entries.capacity(), FxBuildHasher::default());

        for entry in self.entries.drain(..) {
            if !entry.segments.is_empty() {
                let key = PathKey::from(entry.segments.as_slice());
                let idx = new_entries.len();
                new_entries.push(entry);
                new_path_to_idx.insert(key, idx);
            }
        }

        self.entries = new_entries;
        self.path_to_idx = new_path_to_idx;
        self.removed_count = 0;
    }

    /// Serialize to binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.write_to(&mut buf).expect("write to vec cannot fail");
        buf
    }

    /// Serialize to binary format with zstd compression.
    #[cfg(feature = "compression")]
    pub fn to_bytes_compressed(&self) -> io::Result<Vec<u8>> {
        let uncompressed = self.to_bytes();
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3)?;
        encoder.write_all(&uncompressed)?;
        let compressed = encoder.finish()?;

        // Add header with compression flag
        let mut result = Vec::with_capacity(16 + compressed.len());
        result.extend_from_slice(MAGIC_INDEX);
        result.extend_from_slice(&VERSION.to_le_bytes());
        result.extend_from_slice(&FLAG_COMPRESSED.to_le_bytes());
        result.extend_from_slice(&(self.interner.len() as u32).to_le_bytes());
        result.extend_from_slice(&(self.len() as u32).to_le_bytes());
        result.extend_from_slice(&compressed);

        Ok(result)
    }

    /// Write to a writer in binary format.
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Header
        w.write_all(MAGIC_INDEX)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // flags (no compression)
        w.write_all(&(self.interner.len() as u32).to_le_bytes())?;
        w.write_all(&(self.len() as u32).to_le_bytes())?;

        // Segment table (handles non-UTF-8 bytes)
        for (_, seg) in self.interner.iter() {
            let len = seg.len() as u16;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(seg)?; // seg is &BStr which derefs to &[u8]
        }

        // Entry table
        for entry in self.iter_entries() {
            w.write_all(&[entry.flags])?;
            w.write_all(&[entry.segments.len() as u8])?;
            for &seg_id in &entry.segments {
                w.write_all(&seg_id.as_u32().to_le_bytes())?;
            }
        }

        Ok(())
    }

    /// Deserialize from binary format.
    pub fn from_bytes(data: &[u8]) -> io::Result<Self> {
        Self::read_from(&mut io::Cursor::new(data))
    }

    /// Read from a reader in binary format.
    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        // Header
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC_INDEX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid magic"));
        }

        let mut buf2 = [0u8; 2];
        let mut buf4 = [0u8; 4];

        r.read_exact(&mut buf2)?;
        let version = u16::from_le_bytes(buf2);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported version: {}", version),
            ));
        }

        r.read_exact(&mut buf2)?;
        let flags = u16::from_le_bytes(buf2);

        // Handle compression
        if flags & FLAG_COMPRESSED != 0 {
            #[cfg(feature = "compression")]
            {
                r.read_exact(&mut buf4)?;
                let _n_segs = u32::from_le_bytes(buf4);
                r.read_exact(&mut buf4)?;
                let _n_entries = u32::from_le_bytes(buf4);

                let mut compressed = Vec::new();
                r.read_to_end(&mut compressed)?;
                let decompressed = zstd::stream::decode_all(io::Cursor::new(compressed))?;
                return Self::from_bytes(&decompressed);
            }
            #[cfg(not(feature = "compression"))]
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed index requires 'compression' feature",
            ));
        }

        r.read_exact(&mut buf4)?;
        let n_segs = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let n_entries = u32::from_le_bytes(buf4) as usize;

        // Read segment table (supports non-UTF-8 bytes)
        let mut interner = StringInterner::with_capacity(n_segs * 20, n_segs);
        let mut seg_id_map = Vec::with_capacity(n_segs);
        for _ in 0..n_segs {
            r.read_exact(&mut buf2)?;
            let len = u16::from_le_bytes(buf2) as usize;
            let mut seg_buf = vec![0u8; len];
            r.read_exact(&mut seg_buf)?;
            let id = interner.intern_bytes(&seg_buf);
            seg_id_map.push(id);
        }

        // Read entry table
        let mut entries = Vec::with_capacity(n_entries);
        let mut path_to_idx =
            FxHashMap::with_capacity_and_hasher(n_entries, FxBuildHasher::default());

        for idx in 0..n_entries {
            let mut flags_buf = [0u8; 1];
            r.read_exact(&mut flags_buf)?;
            let flags = flags_buf[0];

            let mut depth_buf = [0u8; 1];
            r.read_exact(&mut depth_buf)?;
            let depth = depth_buf[0] as usize;

            let mut segments = smallvec::SmallVec::with_capacity(depth);
            for _ in 0..depth {
                r.read_exact(&mut buf4)?;
                let seg_idx = u32::from_le_bytes(buf4) as usize;
                let Some(&id) = seg_id_map.get(seg_idx) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid segment id",
                    ));
                };
                segments.push(id);
            }

            let key = PathKey::from(segments.as_slice());
            path_to_idx.insert(key, idx);

            entries.push(FileEntry { segments, flags });
        }

        Ok(Self {
            interner,
            entries,
            path_to_idx,
            removed_count: 0,
        })
    }
}

#[derive(Debug, Clone)]
pub enum FileIndexDelta {
    Add(Vec<(String, bool)>),
    Remove(Vec<String>),
    Batch(Vec<FileIndexDelta>),
}

impl FileIndexDelta {
    pub fn add(entries: Vec<(String, bool)>) -> Self {
        Self::Add(entries)
    }

    pub fn remove(paths: Vec<String>) -> Self {
        Self::Remove(paths)
    }

    /// Check if the delta is empty (no actual changes).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Add(entries) => entries.is_empty(),
            Self::Remove(paths) => paths.is_empty(),
            Self::Batch(deltas) => deltas.iter().all(Self::is_empty),
        }
    }

    pub fn apply_to(&self, index: &mut FileIndex) {
        match self {
            FileIndexDelta::Add(entries) => {
                for (path, is_dir) in entries {
                    index.insert(path, *is_dir);
                }
            }
            FileIndexDelta::Remove(paths) => {
                for path in paths {
                    index.remove(path);
                }
            }
            FileIndexDelta::Batch(deltas) => {
                for delta in deltas {
                    delta.apply_to(index);
                }
            }
        }
    }

    /// Serialize to JSON (for ACP notifications).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            FileIndexDelta::Add(entries) => {
                serde_json::json!({
                    "op": "add",
                    "entries": entries.iter().map(|(p, d)| {
                        serde_json::json!({"path": p, "isDir": d})
                    }).collect::<Vec<_>>()
                })
            }
            FileIndexDelta::Remove(paths) => {
                serde_json::json!({
                    "op": "remove",
                    "paths": paths
                })
            }
            FileIndexDelta::Batch(deltas) => {
                serde_json::json!({
                    "op": "batch",
                    "deltas": deltas.iter().map(|d| d.to_json()).collect::<Vec<_>>()
                })
            }
        }
    }

    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let op = value.get("op")?.as_str()?;
        match op {
            "add" => {
                let entries = value.get("entries")?.as_array()?;
                let entries: Vec<_> = entries
                    .iter()
                    .filter_map(|e| {
                        let path = e.get("path")?.as_str()?.to_string();
                        let is_dir = e.get("isDir")?.as_bool()?;
                        Some((path, is_dir))
                    })
                    .collect();
                Some(FileIndexDelta::Add(entries))
            }
            "remove" => {
                let paths = value.get("paths")?.as_array()?;
                let paths: Vec<_> = paths
                    .iter()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect();
                Some(FileIndexDelta::Remove(paths))
            }
            "batch" => {
                let deltas = value.get("deltas")?.as_array()?;
                let deltas: Vec<_> = deltas.iter().filter_map(Self::from_json).collect();
                Some(FileIndexDelta::Batch(deltas))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_interner() {
        let mut interner = StringInterner::new();

        let id1 = interner.intern("src");
        let id2 = interner.intern("lib");
        let id3 = interner.intern("src"); // duplicate

        assert_eq!(id1, id3);
        assert_ne!(id1, id2);
        assert_eq!(interner.get(id1), Some("src"));
        assert_eq!(interner.get(id2), Some("lib"));
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn test_file_index_basic() {
        let mut index = FileIndex::new();

        index.insert("src/main.rs", false);
        index.insert("src/lib.rs", false);
        index.insert("src/utils", true);
        index.insert("Cargo.toml", false);

        assert_eq!(index.len(), 4);
        assert!(index.contains("src/main.rs"));
        assert!(index.contains("src/lib.rs"));
        assert!(!index.contains("src/foo.rs"));

        assert!(index.num_segments() < 6); // "src" is interned once across the three src/ paths
    }

    #[test]
    fn test_file_index_remove() {
        let mut index = FileIndex::new();

        index.insert("src/main.rs", false);
        index.insert("src/lib.rs", false);

        assert_eq!(index.len(), 2);

        assert!(index.remove("src/main.rs"));
        assert!(!index.remove("src/main.rs")); // already removed

        assert_eq!(index.len(), 1);
        assert!(!index.contains("src/main.rs"));
        assert!(index.contains("src/lib.rs"));
    }

    #[test]
    fn test_file_index_serialization() {
        let mut index = FileIndex::new();

        index.insert("src/main.rs", false);
        index.insert("src/lib.rs", false);
        index.insert("src/utils", true);
        index.insert("Cargo.toml", false);

        let bytes = index.to_bytes();
        let restored = FileIndex::from_bytes(&bytes).unwrap();

        assert_eq!(restored.len(), index.len());
        assert!(restored.contains("src/main.rs"));
        assert!(restored.contains("src/lib.rs"));
        assert!(restored.contains("Cargo.toml"));
    }

    #[test]
    fn test_delta_apply() {
        let mut index = FileIndex::new();
        index.insert("src/main.rs", false);

        let delta = FileIndexDelta::add(vec![
            ("src/lib.rs".to_string(), false),
            ("src/utils".to_string(), true),
        ]);
        delta.apply_to(&mut index);

        assert_eq!(index.len(), 3);

        let delta = FileIndexDelta::remove(vec!["src/main.rs".to_string()]);
        delta.apply_to(&mut index);

        assert_eq!(index.len(), 2);
        assert!(!index.contains("src/main.rs"));
    }

    #[test]
    fn test_delta_json_roundtrip() {
        let delta = FileIndexDelta::add(vec![
            ("src/main.rs".to_string(), false),
            ("src/lib".to_string(), true),
        ]);

        let json = delta.to_json();
        let restored = FileIndexDelta::from_json(&json).unwrap();

        if let FileIndexDelta::Add(entries) = restored {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries.first(), Some(&("src/main.rs".to_string(), false)));
        } else {
            panic!("expected Add delta");
        }
    }

    #[test]
    fn test_iter() {
        let mut index = FileIndex::new();
        index.insert("src/main.rs", false);
        index.insert("src/lib", true);

        let entries: Vec<_> = index.iter().collect();
        assert_eq!(entries.len(), 2);

        let paths: Vec<_> = entries.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib"));
    }

    #[test]
    fn test_from_walk() {
        // Create a temp directory with some files
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        // Create directory structure
        std::fs::create_dir_all(root.join("src/utils")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(root.join("src/utils/helpers.rs"), "// helpers").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(root.join("README.md"), "# README").unwrap();

        // Create .git directory (should be excluded)
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]").unwrap();

        // Create hidden file (should be excluded by default)
        std::fs::write(root.join(".hidden"), "secret").unwrap();

        let index = FileIndex::from_walk(root).unwrap();

        // Check expected files are present
        assert!(index.contains("src/main.rs"), "should contain src/main.rs");
        assert!(index.contains("src/lib.rs"), "should contain src/lib.rs");
        assert!(
            index.contains("src/utils/helpers.rs"),
            "should contain src/utils/helpers.rs"
        );
        assert!(index.contains("Cargo.toml"), "should contain Cargo.toml");
        assert!(index.contains("README.md"), "should contain README.md");

        // Check directories are indexed
        assert!(index.contains("src"), "should contain src directory");
        assert!(
            index.contains("src/utils"),
            "should contain src/utils directory"
        );

        // Check .git is excluded
        assert!(!index.contains(".git"), "should not contain .git");
        assert!(
            !index.contains(".git/config"),
            "should not contain .git/config"
        );
        assert!(
            !index.contains(".git/objects"),
            "should not contain .git/objects"
        );

        // Check hidden files are excluded by default
        assert!(
            !index.contains(".hidden"),
            "should not contain .hidden by default"
        );
    }

    #[test]
    fn test_from_walk_include_hidden() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join("visible.txt"), "visible").unwrap();
        std::fs::write(root.join(".hidden"), "hidden").unwrap();

        // With default options (skip hidden)
        let index = FileIndex::from_walk(root).unwrap();
        assert!(index.contains("visible.txt"));
        assert!(!index.contains(".hidden"));

        // With include all options
        let index = FileIndex::from_walk_with_options(root, WalkOptions::include_all()).unwrap();
        assert!(index.contains("visible.txt"));
        assert!(index.contains(".hidden"));
    }

    #[test]
    fn test_from_walk_respects_gitignore() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        std::fs::write(root.join(".gitignore"), "*.log\ntarget/\n").unwrap();

        // Create files
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("debug.log"), "log content").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/output"), "binary").unwrap();

        // Build index with gitignore (default)
        let index = FileIndex::from_walk(root).unwrap();

        assert!(index.contains("main.rs"), "should contain main.rs");
        assert!(
            !index.contains("debug.log"),
            "should not contain debug.log (gitignored)"
        );
        assert!(
            !index.contains("target"),
            "should not contain target (gitignored)"
        );
        assert!(
            !index.contains("target/output"),
            "should not contain target/output (gitignored)"
        );

        // Build index without gitignore
        let options = WalkOptions::default().with_gitignore(false);
        let index = FileIndex::from_walk_with_options(root, options).unwrap();

        assert!(index.contains("main.rs"));
        assert!(
            index.contains("debug.log"),
            "should contain debug.log when gitignore disabled"
        );
        assert!(
            index.contains("target"),
            "should contain target when gitignore disabled"
        );
    }

    #[test]
    fn test_walk_options_builder() {
        let opts = WalkOptions::default()
            .with_max_depth(3)
            .with_gitignore(false)
            .with_hidden(false);

        assert_eq!(opts.max_depth, Some(3));
        assert!(!opts.respect_gitignore);
        assert!(!opts.skip_hidden);
    }

    #[test]
    fn test_delta_is_empty() {
        assert!(FileIndexDelta::Add(vec![]).is_empty());

        assert!(!FileIndexDelta::Add(vec![("src/main.rs".to_string(), false)]).is_empty());

        assert!(FileIndexDelta::Remove(vec![]).is_empty());

        assert!(!FileIndexDelta::Remove(vec!["src/main.rs".to_string()]).is_empty());

        assert!(FileIndexDelta::Batch(vec![]).is_empty());

        assert!(
            FileIndexDelta::Batch(vec![
                FileIndexDelta::Add(vec![]),
                FileIndexDelta::Remove(vec![]),
            ])
            .is_empty()
        );

        assert!(
            !FileIndexDelta::Batch(vec![
                FileIndexDelta::Add(vec![]),
                FileIndexDelta::Add(vec![("src/main.rs".to_string(), false)]),
            ])
            .is_empty()
        );
    }

    #[test]
    fn test_interner_get_id() {
        let mut interner = StringInterner::new();

        // Intern some segments
        let id_src = interner.intern("src");
        let id_lib = interner.intern("lib");
        let id_main = interner.intern("main.rs");

        // get_id should find existing segments
        assert_eq!(interner.get_id("src"), Some(id_src));
        assert_eq!(interner.get_id("lib"), Some(id_lib));
        assert_eq!(interner.get_id("main.rs"), Some(id_main));

        // get_id should return None for non-existent segments
        assert_eq!(interner.get_id("nonexistent"), None);
        assert_eq!(interner.get_id("foo.rs"), None);

        // get_id should not modify the interner
        assert_eq!(interner.len(), 3);
        let _ = interner.get_id("bar.rs");
        assert_eq!(interner.len(), 3);
    }

    #[test]
    fn test_interner_many_segments() {
        // Test that interning many segments doesn't degrade to O(N) lookups
        let mut interner = StringInterner::new();

        // Intern 10000 unique segments
        let segment_count = 10_000;
        let mut ids = Vec::with_capacity(segment_count);
        for i in 0..segment_count {
            let segment = format!("segment_{}", i);
            ids.push(interner.intern(&segment));
        }

        assert_eq!(interner.len(), segment_count);

        // Verify all segments can be looked up correctly
        for (i, &id) in ids.iter().enumerate() {
            let segment = format!("segment_{}", i);
            assert_eq!(interner.get_id(&segment), Some(id));
            assert_eq!(interner.get(id), Some(segment.as_str()));
        }

        // Verify interning duplicates returns existing IDs
        for (i, &id) in ids.iter().enumerate().take(100) {
            let segment = format!("segment_{}", i);
            assert_eq!(interner.intern(&segment), id);
        }
        assert_eq!(interner.len(), segment_count);
    }

    #[test]
    fn test_file_index_large_scale() {
        // Test that FileIndex operations remain fast with many entries
        let mut index = FileIndex::with_capacity(10_000);

        // Insert many entries with shared path components
        let dirs = ["src", "lib", "tests", "benches", "examples"];
        let subdirs = ["utils", "core", "api", "internal", "compat"];
        let files = ["mod.rs", "lib.rs", "main.rs", "test.rs", "helpers.rs"];

        for dir in &dirs {
            for subdir in &subdirs {
                for file in &files {
                    let path = format!("{}/{}/{}", dir, subdir, file);
                    index.insert(&path, false);
                }
                // Add subdirectory entry
                let path = format!("{}/{}", dir, subdir);
                index.insert(&path, true);
            }
            // Add directory entry
            index.insert(*dir, true);
        }

        // Total entries: 5 dirs + 5*5 subdirs + 5*5*5 files = 5 + 25 + 125 = 155
        assert_eq!(index.len(), 155);

        // dirs (5) + subdirs (5) + files (5) = 15 unique segments
        assert_eq!(index.num_segments(), 15);

        assert!(index.contains("src/utils/mod.rs"));
        assert!(index.contains("lib/core/main.rs"));
        assert!(!index.contains("nonexistent/path/file.rs"));

        assert!(index.remove("src/utils/mod.rs"));
        assert!(!index.contains("src/utils/mod.rs"));
        assert_eq!(index.len(), 154);
    }

    #[test]
    fn test_path_segments_uses_get_id() {
        // Verify that path_segments doesn't intern new segments
        let mut index = FileIndex::new();

        // Insert some paths
        index.insert("src/main.rs", false);
        index.insert("src/lib.rs", false);

        let initial_segments = index.num_segments();

        // contains() uses path_segments() internally
        assert!(!index.contains("foo/bar/baz.rs"));
        assert!(!index.contains("nonexistent.txt"));

        assert_eq!(index.num_segments(), initial_segments);
    }

    #[test]
    fn test_interner_non_utf8_bytes() {
        use bstr::BStr;
        let mut interner = StringInterner::new();

        // Intern valid UTF-8
        let id_valid = interner.intern("hello");
        assert_eq!(interner.get(id_valid), Some("hello"));
        assert_eq!(interner.get_bytes(id_valid), Some(b"hello".as_slice()));

        // Intern raw bytes that are not valid UTF-8
        let invalid_utf8: &[u8] = &[0x80, 0x81, 0x82];
        let id_invalid = interner.intern_bytes(invalid_utf8);

        assert_eq!(interner.get(id_invalid), None);

        assert_eq!(interner.get_bytes(id_invalid), Some(invalid_utf8));

        assert_eq!(interner.get_bstr(id_invalid), Some(BStr::new(invalid_utf8)));

        let id_invalid2 = interner.intern_bytes(invalid_utf8);
        assert_eq!(id_invalid, id_invalid2);

        assert_eq!(interner.get_bytes_id(invalid_utf8), Some(id_invalid));

        // iter() returns BStr which works for all bytes
        let segments: Vec<_> = interner.iter().collect();
        assert_eq!(segments.len(), 2);
    }
}
