//! Content-addressed blob storage, chunked for the sync transport.
//!
//! Plugin UI bundles are the reason this exists. A React bundle carrying the
//! Powerhouse design system is several megabytes, while [`MAX_MSG_BYTES`] caps
//! a sync message at 1 MiB — and that cap is load-bearing, because it bounds
//! how much memory a hostile peer can make us allocate. So a bundle travels as
//! chunks and is reassembled locally.
//!
//! Everything here is content-addressed: a chunk's name IS the SHA-256 of its
//! bytes, and a blob's name is the SHA-256 of the whole. That gives
//! deduplication and integrity for free — a chunk that hashes correctly is the
//! right chunk no matter which peer sent it, so blobs can be fetched from
//! anyone without trusting them.
//!
//! Verification is not optional and not deferred: [`BlobStore::put_chunk`]
//! refuses bytes that do not match the name they arrived under, so a corrupt or
//! hostile chunk never reaches disk.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::doc::Hash32;

/// Bytes per chunk.
///
/// A quarter of [`crate::p2p::codec::MAX_MSG_BYTES`], leaving generous room for
/// the enclosing message's framing and metadata. Smaller chunks mean more
/// round trips; larger ones risk bumping the cap as the envelope grows.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// A blob: its identity, size, and the ordered chunks it is made of.
///
/// Ordered, because reassembly concatenates them; the order is part of what
/// the blob hash covers, so a reordered set fails verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: Hash32,
    pub size: u64,
    pub chunks: Vec<Hash32>,
}

impl BlobRef {
    /// Describes `bytes` without storing anything.
    pub fn of(bytes: &[u8]) -> Self {
        let chunks = bytes.chunks(CHUNK_BYTES).map(Hash32::of).collect();
        Self {
            hash: Hash32::of(bytes),
            size: bytes.len() as u64,
            chunks,
        }
    }
}

/// A directory of chunks, one file per chunk, named by its hash.
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn chunk_path(&self, h: &Hash32) -> PathBuf {
        self.dir.join(format!("{h}.chunk"))
    }

    pub fn has_chunk(&self, h: &Hash32) -> bool {
        self.chunk_path(h).exists()
    }

    /// Stores a chunk, **verifying it against the hash it claims to be**.
    ///
    /// This is the trust boundary for the whole transport: chunks arrive from
    /// peers we have no reason to trust, and this is what makes that safe.
    pub fn put_chunk(&self, claimed: &Hash32, bytes: &[u8]) -> Result<(), String> {
        let actual = Hash32::of(bytes);
        if actual != *claimed {
            return Err(format!(
                "chunk does not match its hash (claimed {claimed}, got {actual})"
            ));
        }
        let path = self.chunk_path(claimed);
        if path.exists() {
            return Ok(()); // content-addressed: identical by definition
        }
        // Write to a temporary name first so an interrupted write cannot leave
        // a truncated file under a hash that says it is complete.
        let tmp = path.with_extension("partial");
        std::fs::write(&tmp, bytes).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("renaming into place: {e}"))
    }

    pub fn get_chunk(&self, h: &Hash32) -> Option<Vec<u8>> {
        std::fs::read(self.chunk_path(h)).ok()
    }

    /// Splits `bytes` into chunks and stores them.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef, String> {
        let r = BlobRef::of(bytes);
        for (i, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            self.put_chunk(&r.chunks[i], chunk)?;
        }
        Ok(r)
    }

    /// The chunks of `r` this store does not have yet — what to ask peers for.
    pub fn missing(&self, r: &BlobRef) -> Vec<Hash32> {
        let mut seen = BTreeSet::new();
        r.chunks
            .iter()
            .filter(|h| seen.insert(**h) && !self.has_chunk(h))
            .copied()
            .collect()
    }

    pub fn is_complete(&self, r: &BlobRef) -> bool {
        r.chunks.iter().all(|h| self.has_chunk(h))
    }

    /// Reassembles a blob, verifying the result against `r.hash`.
    ///
    /// Every chunk was verified on the way in, but the whole is checked again:
    /// individually valid chunks in the wrong order, or a `BlobRef` that does
    /// not describe what it claims, would otherwise pass unnoticed.
    pub fn get(&self, r: &BlobRef) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(r.size as usize);
        for h in &r.chunks {
            let c = self
                .get_chunk(h)
                .ok_or_else(|| format!("missing chunk {h}"))?;
            out.extend_from_slice(&c);
        }
        if out.len() as u64 != r.size {
            return Err(format!(
                "reassembled {} bytes, expected {}",
                out.len(),
                r.size
            ));
        }
        let actual = Hash32::of(&out);
        if actual != r.hash {
            return Err(format!(
                "reassembled blob does not match its hash (want {}, got {actual})",
                r.hash
            ));
        }
        Ok(out)
    }

    /// Deletes chunks not referenced by any blob in `keep`.
    ///
    /// Uninstalling a package must not orphan megabytes forever, and chunks are
    /// shared between blobs, so deletion has to be by reachability rather than
    /// per-package.
    pub fn gc(&self, keep: &[BlobRef]) -> Result<usize, String> {
        let live: BTreeSet<Hash32> = keep.iter().flat_map(|r| r.chunks.iter().copied()).collect();
        let mut removed = 0;
        let rd = std::fs::read_dir(&self.dir)
            .map_err(|e| format!("reading {}: {e}", self.dir.display()))?;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("chunk") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // An unparseable name is not ours; leaving it alone is safer than
            // deleting a file we do not understand.
            let Ok(h) = Hash32::from_hex(stem) else {
                continue;
            };
            if !live.contains(&h) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().expect("tmp");
        let s = BlobStore::open(&dir.path().join("blobs")).expect("open");
        (dir, s)
    }

    /// Bigger than one chunk, so chunking is actually exercised. `seed`
    /// makes two bundles genuinely distinct — without it they share leading
    /// chunks and deduplication quietly invalidates size assertions.
    fn bundle(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| ((i % 251) as u8).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn a_multi_chunk_blob_round_trips() {
        let (_d, s) = store();
        let data = bundle(CHUNK_BYTES * 3 + 7, 0);
        let r = s.put(&data).expect("put");
        assert_eq!(r.chunks.len(), 4, "three full chunks and a remainder");
        assert_eq!(r.size, data.len() as u64);
        assert_eq!(s.get(&r).expect("get"), data);
    }

    #[test]
    fn a_blob_smaller_than_one_chunk_works() {
        let (_d, s) = store();
        let data = b"small".to_vec();
        let r = s.put(&data).expect("put");
        assert_eq!(r.chunks.len(), 1);
        assert_eq!(s.get(&r).expect("get"), data);
    }

    /// The trust boundary: bytes that do not match their name never land.
    #[test]
    fn a_chunk_that_does_not_match_its_hash_is_refused() {
        let (_d, s) = store();
        let honest = Hash32::of(b"honest");
        let err = s
            .put_chunk(&honest, b"tampered")
            .expect_err("must refuse mismatched content");
        assert!(err.contains("does not match its hash"), "got: {err}");
        assert!(!s.has_chunk(&honest), "nothing may be written");
    }

    #[test]
    fn missing_reports_exactly_what_to_fetch() {
        let (_d, s) = store();
        let data = bundle(CHUNK_BYTES * 2 + 1, 0);
        let r = BlobRef::of(&data);
        assert_eq!(s.missing(&r).len(), 3, "nothing stored yet");
        assert!(!s.is_complete(&r));

        // Store only the first chunk.
        s.put_chunk(&r.chunks[0], &data[..CHUNK_BYTES])
            .expect("put");
        assert_eq!(s.missing(&r).len(), 2);
        assert!(!s.is_complete(&r));
    }

    /// Identical content stored twice occupies one chunk: this is what keeps
    /// two plugin versions that share most of their bundle from doubling disk.
    #[test]
    fn identical_chunks_deduplicate() {
        let (_d, s) = store();
        let block = vec![7u8; CHUNK_BYTES];
        let mut data = block.clone();
        data.extend_from_slice(&block);
        let r = s.put(&data).expect("put");
        assert_eq!(r.chunks.len(), 2);
        assert_eq!(r.chunks[0], r.chunks[1], "same bytes, same hash");
        let files = std::fs::read_dir(&s.dir).expect("read").count();
        assert_eq!(files, 1, "the two identical chunks share one file");
        assert_eq!(s.get(&r).expect("get"), data);
    }

    #[test]
    fn an_incomplete_blob_cannot_be_reassembled() {
        let (_d, s) = store();
        let data = bundle(CHUNK_BYTES * 2, 0);
        let r = BlobRef::of(&data);
        s.put_chunk(&r.chunks[0], &data[..CHUNK_BYTES])
            .expect("put");
        let err = s.get(&r).expect_err("must not reassemble");
        assert!(err.contains("missing chunk"), "got: {err}");
    }

    /// Chunks can be valid individually while the blob is a lie: reassembly
    /// checks the whole, not only the parts.
    #[test]
    fn a_blobref_that_misdescribes_its_content_is_rejected() {
        let (_d, s) = store();
        let real = bundle(CHUNK_BYTES + 10, 0);
        let r = s.put(&real).expect("put");
        let mut lying = r.clone();
        lying.hash = Hash32::of(b"something else entirely");
        let err = s.get(&lying).expect_err("must reject");
        assert!(err.contains("does not match its hash"), "got: {err}");
    }

    #[test]
    fn gc_keeps_referenced_chunks_and_drops_the_rest() {
        let (_d, s) = store();
        let keep = s.put(&bundle(CHUNK_BYTES + 1, 0)).expect("put keep");
        let drop = s.put(&bundle(CHUNK_BYTES * 2 + 3, 99)).expect("put drop");
        // Distinct content, so no chunk is shared between them.
        let before = std::fs::read_dir(&s.dir).expect("read").count();
        assert_eq!(before, keep.chunks.len() + drop.chunks.len());

        let removed = s.gc(std::slice::from_ref(&keep)).expect("gc");
        assert_eq!(removed, drop.chunks.len());
        assert!(s.is_complete(&keep), "the kept blob is untouched");
        assert!(!s.is_complete(&drop));
    }

    /// Two blobs sharing a prefix share chunks on disk. This is the property
    /// that makes plugin upgrades cheap — a new version re-sends only what
    /// actually changed — and it was found by a fixture that assumed otherwise.
    #[test]
    fn blobs_sharing_content_share_chunks_on_disk() {
        let (_d, s) = store();
        let a = bundle(CHUNK_BYTES * 2, 0);
        let mut b = bundle(CHUNK_BYTES, 0); // identical first chunk
        b.extend_from_slice(&bundle(CHUNK_BYTES, 42)); // different second
        let ra = s.put(&a).expect("put a");
        let rb = s.put(&b).expect("put b");
        assert_eq!(ra.chunks[0], rb.chunks[0], "shared prefix, shared chunk");
        assert_ne!(ra.chunks[1], rb.chunks[1]);
        let files = std::fs::read_dir(&s.dir).expect("read").count();
        assert_eq!(files, 3, "four chunks, one shared");
        assert_eq!(s.get(&ra).expect("a"), a);
        assert_eq!(s.get(&rb).expect("b"), b);
    }

    #[test]
    fn gc_is_safe_to_run_twice() {
        let (_d, s) = store();
        let keep = s.put(&bundle(CHUNK_BYTES + 1, 0)).expect("put");
        assert_eq!(s.gc(std::slice::from_ref(&keep)).expect("gc"), 0);
        assert_eq!(s.gc(std::slice::from_ref(&keep)).expect("gc again"), 0);
        assert!(s.is_complete(&keep));
    }

    /// Every chunk must fit a sync message with room for the envelope.
    #[test]
    fn chunk_size_fits_the_transport_cap() {
        assert!(
            CHUNK_BYTES < crate::p2p::codec::MAX_MSG_BYTES as usize,
            "a chunk must fit in one message"
        );
        assert!(
            CHUNK_BYTES * 4 <= crate::p2p::codec::MAX_MSG_BYTES as usize,
            "leave generous headroom for framing and metadata"
        );
    }
}
