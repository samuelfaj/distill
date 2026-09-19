// Modified for Distill by Samuel Fajreldines, 2026.
//! Integration tests for indexing filters, batching, and cache round-trip.
//!
//! These tests create real file trees and exercise IndexManager / IndexBuilder
//! contracts that are awkward to cover with tiny unit fixtures.

use std::fs;
use std::path::Path;
use tempfile::tempdir;
use distill_codebase_graph::{
    FileEvent, IndexBuilder, IndexManager, IndexManagerConfig, load_index, save_index,
};

/// Create N Rust source files in `dir`, each with `defs_per_file` function defs.
fn create_rust_files(dir: &Path, count: usize, defs_per_file: usize) {
    for i in 0..count {
        let mut content = String::new();
        for d in 0..defs_per_file {
            content.push_str(&format!("fn func_{}_{}() {{}}\n", i, d));
        }
        fs::write(dir.join(format!("file_{}.rs", i)), &content).unwrap();
    }
}

/// Create N binary files with a supported extension.
fn create_binary_files(dir: &Path, count: usize, size: usize) {
    for i in 0..count {
        let mut data = vec![0xFFu8; size];
        // Ensure null byte in first 8000 bytes for binary detection
        data[50] = 0;
        fs::write(dir.join(format!("binary_{}.rs", i)), &data).unwrap();
    }
}

// =========================================================================
// Tests
// =========================================================================

#[test]
fn test_binary_files_no_memory_growth() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("legit.rs"), "fn legit() {}").unwrap();

    let config = IndexManagerConfig::new(root.to_path_buf())
        .without_cache_load()
        .without_cache_save();

    let handle = IndexManager::spawn(config);
    let _ = handle.get_file_count();

    // Create 200 binary files with .rs extension, each 100KB
    create_binary_files(root, 200, 100_000);

    for i in 0..200 {
        let path = root.join(format!("binary_{}.rs", i));
        handle.send_event(FileEvent::created(path)).unwrap();
    }

    let count = handle.get_file_count().unwrap();

    // Binary files should not be indexed (detected via 8KB prefix read)
    assert_eq!(count, 1, "Only legit.rs should be indexed");

    handle.shutdown().unwrap();
}

#[test]
fn test_hidden_dir_files_not_indexed() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("normal.rs"), "fn normal() {}").unwrap();

    let config = IndexManagerConfig::new(root.to_path_buf())
        .without_cache_load()
        .without_cache_save();

    let handle = IndexManager::spawn(config);
    let _ = handle.get_file_count();

    // Create 300 files under a hidden directory (simulating .claude worktree)
    let hidden = root.join(".claude").join("worktrees").join("session1");
    fs::create_dir_all(&hidden).unwrap();
    create_rust_files(&hidden, 300, 20);

    for i in 0..300 {
        let path = hidden.join(format!("file_{}.rs", i));
        handle.send_event(FileEvent::created(path)).unwrap();
    }

    let count = handle.get_file_count().unwrap();

    // Hidden dir files should not be indexed
    assert_eq!(count, 1, "Only normal.rs should be indexed");

    handle.shutdown().unwrap();
}

#[test]
fn test_oversized_files_skipped() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("small.rs"), "fn small() {}").unwrap();

    let config = IndexManagerConfig::new(root.to_path_buf())
        .without_cache_load()
        .without_cache_save();

    let handle = IndexManager::spawn(config);
    let _ = handle.get_file_count();

    // Create a 6MB text file with valid Rust syntax (exceeds MAX_INDEXABLE_FILE_SIZE)
    let big_content = "fn big() {}\n".repeat(500_000);
    let big_path = root.join("huge.rs");
    fs::write(&big_path, &big_content).unwrap();
    drop(big_content);

    handle.send_event(FileEvent::created(big_path)).unwrap();

    let count = handle.get_file_count().unwrap();
    assert_eq!(count, 1, "Only small.rs should be indexed");

    handle.shutdown().unwrap();
}

#[test]
fn test_builder_skips_binary_and_oversized_in_bulk() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Mix of valid, binary, and oversized files
    create_rust_files(root, 100, 5); // 100 valid files
    create_binary_files(root, 50, 10_000); // 50 binary files

    // One oversized file
    let big = "fn x() {}\n".repeat(600_000); // ~6MB
    fs::write(root.join("oversized.rs"), &big).unwrap();
    drop(big);

    let index = IndexBuilder::new().build(root).unwrap();
    let (files, defs, _refs) = index.stats();

    // Only the 100 valid files should be indexed
    assert_eq!(files, 100);
    assert!(defs >= 500); // 100 files × 5 defs
}

/// Verify that bounded merge-batching produces the same index as unbounded.
/// A tiny `build_batch_size` exercises the multi-batch path even on this small corpus.
#[test]
fn test_build_batch_size_produces_correct_index() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    create_rust_files(root, 200, 5); // 200 files, 1 000 defs

    // Build with a very small batch size (10 files per merge batch)
    let batched = IndexBuilder::new()
        .with_build_batch_size(10)
        .build(root)
        .unwrap();

    // Build without batching restriction for comparison
    let reference = IndexBuilder::new().build(root).unwrap();

    let (b_files, b_defs, b_refs) = batched.stats();
    let (r_files, r_defs, r_refs) = reference.stats();

    assert_eq!(
        b_files, r_files,
        "file count must match regardless of batch size"
    );
    assert_eq!(
        b_defs, r_defs,
        "definition count must match regardless of batch size"
    );
    assert_eq!(
        b_refs, r_refs,
        "reference count must match regardless of batch size"
    );
}

// =============================================================================
// Structural compaction tests
// =============================================================================

/// Verify that an index survives a save/load round-trip after compact().
/// Guards the binary format against the u32 line-number change. `IndexBuilder` already compacts.
#[test]
fn test_compact_then_save_load_roundtrip() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    create_rust_files(root, 50, 4); // 50 files, 200 defs

    // build() calls compact() internally via build_fast()
    let original = IndexBuilder::new().build(root).unwrap();

    let cache_path = dir.path().join("test_index.bin");
    save_index(&cache_path, &original).unwrap();

    let loaded = load_index(&cache_path).unwrap();

    let (orig_files, orig_defs, orig_refs) = original.stats();
    let (load_files, load_defs, load_refs) = loaded.stats();

    assert_eq!(orig_files, load_files, "file count survives round-trip");
    assert_eq!(orig_defs, load_defs, "def count survives round-trip");
    assert_eq!(orig_refs, load_refs, "ref count survives round-trip");

    // Verify a representative symbol round-trips
    let sym = "func_0_0";
    let orig_locs = original.find_definitions(sym);
    let load_locs = loaded.find_definitions(sym);
    assert_eq!(
        orig_locs, load_locs,
        "definition locations must survive save/load round-trip"
    );
}
