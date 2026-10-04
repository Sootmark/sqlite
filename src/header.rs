//! The 100-byte database header at the start of page 1.

use crate::bytes::{u16_at, u32_at, u8_at};
use crate::Error;

/// Bytes in the database header.
pub(crate) const HEADER_SIZE: usize = 100;
/// "SQLite format 3" and its NUL.
const MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// The page size field's stand-in for 65536, which doesn't fit in 16 bits.
const PAGE_SIZE_65536: u16 = 1;
const MIN_PAGE_SIZE: u32 = 512;
const MAX_PAGE_SIZE: u32 = 65_536;
/// The smallest usable size (page size less reserved bytes) the format
/// allows; the overflow formulas assume it.
const MIN_USABLE_SIZE: u32 = 480;
/// The highest file format read version this reader understands.
const MAX_READ_VERSION: u8 = 2;
/// The payload fractions are fixed by the format.
const PAYLOAD_FRACTIONS: [u8; 3] = [64, 32, 32];

/// How text is stored throughout a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// 1: UTF-8.
    Utf8,
    /// 2: UTF-16, little-endian.
    Utf16Le,
    /// 3: UTF-16, big-endian.
    Utf16Be,
}

impl TextEncoding {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Utf8),
            2 => Some(Self::Utf16Le),
            3 => Some(Self::Utf16Be),
            _ => None,
        }
    }

    /// Text stored in this encoding, with damaged sequences replaced by
    /// U+FFFD.
    #[must_use]
    pub fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Self::Utf16Le => decode_utf16(bytes, u16::from_le_bytes),
            Self::Utf16Be => decode_utf16(bytes, u16::from_be_bytes),
        }
    }
}

/// UTF-16 text whose code units `unit` reads; a lone last byte is damage.
fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
    let mut text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if bytes.len() % 2 == 1 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

/// The database header: the first 100 bytes of the file, all big-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Offset 16: bytes per page, a power of two from 512 to 65536 (stored
    /// as 1).
    pub page_size: u32,
    /// Offset 18: file format write version, 1 for rollback journals, 2 for
    /// WAL.
    pub write_version: u8,
    /// Offset 19: file format read version, 1 for rollback journals, 2 for
    /// WAL.
    pub read_version: u8,
    /// Offset 20: bytes reserved at the end of every page for extensions
    /// (encryption nonces, checksums), usually 0.
    pub reserved_bytes: u8,
    /// Offset 24: incremented when the file is unlocked after a change (not
    /// necessarily on every transaction in WAL mode).
    pub change_counter: u32,
    /// Offset 28: the database size in pages, valid only when non-zero and
    /// `change_counter` equals `version_valid_for`.
    pub page_count: u32,
    /// Offset 32: the first freelist trunk page, 0 when none.
    pub freelist_trunk: u32,
    /// Offset 36: pages on the freelist.
    pub freelist_count: u32,
    /// Offset 40: incremented on every schema change.
    pub schema_cookie: u32,
    /// Offset 44: schema format number, 1 to 4.
    pub schema_format: u32,
    /// Offset 48: suggested page cache size.
    pub default_cache_size: i32,
    /// Offset 52: the largest root b-tree page in auto- and incremental
    /// vacuum modes (the file then has pointer map pages), else 0.
    pub largest_root_page: u32,
    /// Offset 56: how text is stored.
    pub text_encoding: TextEncoding,
    /// Offset 60: `PRAGMA user_version`.
    pub user_version: u32,
    /// Offset 64: incremental vacuum mode (only with `largest_root_page`).
    pub incremental_vacuum: bool,
    /// Offset 68: `PRAGMA application_id`.
    pub application_id: u32,
    /// Offset 92: the `change_counter` value when `sqlite_version` was
    /// written.
    pub version_valid_for: u32,
    /// Offset 96: `SQLITE_VERSION_NUMBER` of the library that last wrote
    /// the file, e.g. 3046001 for 3.46.1.
    pub sqlite_version: u32,
}

impl Header {
    /// Read the header at the start of `bytes`, and report fields that
    /// break the format's rules but don't stop the file from being read.
    pub(crate) fn parse(bytes: &[u8], problems: &mut Vec<String>) -> Result<Self, Error> {
        let field = |at| u32_at(bytes, at).unwrap_or_default();
        let byte = |at| u8_at(bytes, at).unwrap_or_default();
        if bytes.len() < HEADER_SIZE {
            return Err(Error::TooShort(bytes.len()));
        }
        if !bytes.starts_with(MAGIC) {
            return Err(Error::NotSqlite);
        }
        let page_size = page_size(u16_at(bytes, 16).unwrap_or_default())?;
        let reserved_bytes = byte(20);
        if page_size - u32::from(reserved_bytes) < MIN_USABLE_SIZE {
            return Err(Error::ReservedBytes(reserved_bytes));
        }
        let raw_encoding = field(56);
        let text_encoding = TextEncoding::from_raw(raw_encoding).unwrap_or_else(|| {
            problems.push(format!(
                "text encoding {raw_encoding} is not 1, 2 or 3; read as UTF-8"
            ));
            TextEncoding::Utf8
        });
        let header = Self {
            page_size,
            write_version: byte(18),
            read_version: byte(19),
            reserved_bytes,
            change_counter: field(24),
            page_count: field(28),
            freelist_trunk: field(32),
            freelist_count: field(36),
            schema_cookie: field(40),
            schema_format: field(44),
            default_cache_size: field(48) as i32,
            largest_root_page: field(52),
            text_encoding,
            user_version: field(60),
            incremental_vacuum: field(64) != 0,
            application_id: field(68),
            version_valid_for: field(92),
            sqlite_version: field(96),
        };
        header.check(&bytes[21..24], problems);
        Ok(header)
    }

    /// Bytes of each page that hold data: the page size less the reserved
    /// bytes.
    #[must_use]
    pub fn usable_size(&self) -> u32 {
        self.page_size - u32::from(self.reserved_bytes)
    }

    /// The in-header page count, when it can be trusted: non-zero, and
    /// written by a library that kept `version_valid_for` up to date.
    #[must_use]
    pub fn valid_page_count(&self) -> Option<u32> {
        (self.page_count != 0 && self.change_counter == self.version_valid_for)
            .then_some(self.page_count)
    }

    fn check(&self, fractions: &[u8], problems: &mut Vec<String>) {
        if self.read_version > MAX_READ_VERSION {
            problems.push(format!(
                "file format read version {} is newer than 2; read anyway",
                self.read_version
            ));
        }
        if fractions != PAYLOAD_FRACTIONS {
            problems.push(format!(
                "payload fractions {fractions:?} are not the fixed 64, 32, 32"
            ));
        }
    }
}

/// The page size field: a power of two from 512 to 32768, or 1 for 65536.
fn page_size(raw: u16) -> Result<u32, Error> {
    let size = if raw == PAGE_SIZE_65536 {
        MAX_PAGE_SIZE
    } else {
        u32::from(raw)
    };
    if is_page_size(size) {
        Ok(size)
    } else {
        Err(Error::PageSize(raw))
    }
}

/// Whether `size` is a page size the format allows (for the WAL header,
/// which stores 65536 as is).
pub(crate) fn is_page_size(size: u32) -> bool {
    size.is_power_of_two() && (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&size)
}
