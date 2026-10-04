//! The write-ahead log (`-wal` file): a header, then frames, each a page
//! of the database as a transaction left it. A reader takes, for each page,
//! the last valid frame at or before the last commit frame.

use std::collections::HashMap;

use crate::bytes::u32_at;
use crate::header::is_page_size;

const WAL_HEADER_SIZE: usize = 32;
const FRAME_HEADER_SIZE: usize = 24;
/// Bytes of the header, and of each frame header, that the checksum covers.
const WAL_HEADER_CHECKSUMMED: usize = 24;
const FRAME_HEADER_CHECKSUMMED: usize = 8;
/// The magic number; its low bit says the checksums read the content as
/// big-endian words (else little-endian).
const MAGIC_LITTLE_ENDIAN: u32 = 0x377f_0682;
const MAGIC_BIG_ENDIAN: u32 = 0x377f_0683;
/// The only WAL format version.
const FORMAT_VERSION: u32 = 3_007_000;

/// What a write-ahead log held, and how much of it was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalSummary {
    /// Whether the checksums read the content as big-endian words (magic
    /// 0x377f0683, written on big-endian machines) or little-endian ones
    /// (0x377f0682).
    pub big_endian_checksums: bool,
    /// Bytes per page.
    pub page_size: u32,
    /// Checkpoints since the log was created.
    pub checkpoint_sequence: u32,
    /// Salt-1 and salt-2: frames carrying other salts are left over from an
    /// earlier use of the file.
    pub salts: [u32; 2],
    /// Whole frames in the file.
    pub total_frames: usize,
    /// Frames from the start whose salts and running checksum hold; the
    /// first frame that fails ends the log.
    pub valid_frames: usize,
    /// Valid frames up to and including the last commit frame: the ones
    /// applied. Valid frames after it belong to a transaction that never
    /// committed.
    pub committed_frames: usize,
    /// The database size in pages that the last commit frame records, 0
    /// when nothing committed.
    pub database_pages: u32,
}

/// A log's committed pages.
pub(crate) struct Wal<'a> {
    pub(crate) summary: WalSummary,
    /// Page number to its content in the last committed frame for it.
    pub(crate) pages: HashMap<u32, &'a [u8]>,
}

/// The two halves of the running checksum.
type Checksum = (u32, u32);

/// One frame's header fields.
struct Frame<'a> {
    page_number: u32,
    /// The database size after a commit; 0 in other frames.
    commit_size: u32,
    salts: [u32; 2],
    checksum: Checksum,
    /// The first 8 bytes of the frame header, which the checksum covers.
    checksummed_header: &'a [u8],
    page: &'a [u8],
}

impl<'a> Frame<'a> {
    fn read(bytes: &'a [u8]) -> Self {
        let word = |at| u32_at(bytes, at).unwrap_or_default();
        Self {
            page_number: word(0),
            commit_size: word(4),
            salts: [word(8), word(12)],
            checksum: (word(16), word(20)),
            checksummed_header: &bytes[..FRAME_HEADER_CHECKSUMMED],
            page: &bytes[FRAME_HEADER_SIZE..],
        }
    }
}

impl<'a> Wal<'a> {
    /// Read a log. One that can't be used at all (not a log, a damaged
    /// header) is reported in `problems` and ignored, as SQLite ignores it.
    pub(crate) fn parse(bytes: &'a [u8], problems: &mut Vec<String>) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        let header = match WalHeader::read(bytes) {
            Ok(header) => header,
            Err(problem) => {
                problems.push(format!("WAL ignored: {problem}"));
                return None;
            }
        };
        let frame_size = FRAME_HEADER_SIZE + header.page_size as usize;
        let frames = &bytes[WAL_HEADER_SIZE..];
        let extra = frames.len() % frame_size;
        if extra > 0 {
            problems.push(format!(
                "WAL: {extra} bytes after the last whole frame (a write cut short)"
            ));
        }
        let mut wal = Self {
            summary: header.summary(frames.len() / frame_size),
            pages: HashMap::new(),
        };
        wal.apply(frames.chunks_exact(frame_size).map(Frame::read), header);
        Some(wal)
    }

    /// Follow the frames while they are valid, and keep the pages of those
    /// up to the last commit.
    fn apply(&mut self, frames: impl Iterator<Item = Frame<'a>>, header: WalHeader) {
        let mut checksum = header.checksum;
        let mut uncommitted = Vec::new();
        for frame in frames {
            checksum = frame_checksum(checksum, &frame, header.big_endian);
            let valid =
                frame.page_number != 0 && frame.salts == header.salts && frame.checksum == checksum;
            if !valid {
                break;
            }
            self.summary.valid_frames += 1;
            uncommitted.push((frame.page_number, frame.page));
            if frame.commit_size != 0 {
                self.summary.committed_frames = self.summary.valid_frames;
                self.summary.database_pages = frame.commit_size;
                self.pages.extend(uncommitted.drain(..));
            }
        }
    }
}

/// The fields of the 32-byte log header that reading the frames needs.
#[derive(Clone, Copy)]
struct WalHeader {
    big_endian: bool,
    page_size: u32,
    checkpoint_sequence: u32,
    salts: [u32; 2],
    checksum: Checksum,
}

impl WalHeader {
    fn read(bytes: &[u8]) -> Result<Self, String> {
        let word = |at| u32_at(bytes, at).unwrap_or_default();
        if bytes.len() < WAL_HEADER_SIZE {
            return Err(format!("{} bytes, shorter than its header", bytes.len()));
        }
        let big_endian = match word(0) {
            MAGIC_LITTLE_ENDIAN => false,
            MAGIC_BIG_ENDIAN => true,
            magic => return Err(format!("magic number 0x{magic:08x}")),
        };
        if word(4) != FORMAT_VERSION {
            return Err(format!("format version {}", word(4)));
        }
        let page_size = word(8);
        if !is_page_size(page_size) {
            return Err(format!("page size {page_size}"));
        }
        let checksum = (word(24), word(28));
        if checksum != checksum_words((0, 0), &bytes[..WAL_HEADER_CHECKSUMMED], big_endian) {
            return Err("header checksum does not match".to_owned());
        }
        Ok(Self {
            big_endian,
            page_size,
            checkpoint_sequence: word(12),
            salts: [word(16), word(20)],
            checksum,
        })
    }

    fn summary(self, total_frames: usize) -> WalSummary {
        WalSummary {
            big_endian_checksums: self.big_endian,
            page_size: self.page_size,
            checkpoint_sequence: self.checkpoint_sequence,
            salts: self.salts,
            total_frames,
            valid_frames: 0,
            committed_frames: 0,
            database_pages: 0,
        }
    }
}

/// The running checksum after a frame: over the first 8 bytes of its
/// header, then its page.
fn frame_checksum(seed: Checksum, frame: &Frame, big_endian: bool) -> Checksum {
    let seed = checksum_words(seed, frame.checksummed_header, big_endian);
    checksum_words(seed, frame.page, big_endian)
}

/// The log's checksum: Fibonacci-weighted sums over pairs of 32-bit words,
/// continuing from `seed`. `bytes` is a multiple of 8 long.
fn checksum_words(seed: Checksum, bytes: &[u8], big_endian: bool) -> Checksum {
    let word = |chunk: &[u8]| {
        let raw = [chunk[0], chunk[1], chunk[2], chunk[3]];
        if big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        }
    };
    bytes.chunks_exact(8).fold(seed, |(s0, s1), pair| {
        let s0 = s0.wrapping_add(word(&pair[..4])).wrapping_add(s1);
        let s1 = s1.wrapping_add(word(&pair[4..])).wrapping_add(s0);
        (s0, s1)
    })
}
