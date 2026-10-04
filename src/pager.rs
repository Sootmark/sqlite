//! Pages by number: from the write-ahead log when it committed one, else
//! from the database file.

use std::collections::HashMap;

/// The database's pages, borrowed from the file and the log.
pub(crate) struct Pages<'a> {
    file: &'a [u8],
    wal: HashMap<u32, &'a [u8]>,
    page_size: usize,
    usable_size: usize,
    count: u32,
}

impl<'a> Pages<'a> {
    pub(crate) fn new(
        file: &'a [u8],
        wal: HashMap<u32, &'a [u8]>,
        page_size: u32,
        usable_size: u32,
        count: u32,
    ) -> Self {
        Self {
            file,
            wal,
            page_size: page_size as usize,
            usable_size: usable_size as usize,
            count,
        }
    }

    /// Bytes of each page that hold data (the rest is reserved).
    pub(crate) fn usable_size(&self) -> usize {
        self.usable_size
    }

    /// Page `number`'s usable bytes, or why it can't be read.
    pub(crate) fn get(&self, number: u32) -> Result<&'a [u8], String> {
        if number == 0 || number > self.count {
            return Err(format!(
                "page {number} is outside the database's {} pages",
                self.count
            ));
        }
        let page = match self.wal.get(&number) {
            Some(page) => page,
            None => self.file_page(number)?,
        };
        Ok(&page[..self.usable_size])
    }

    /// Every page, whole, in order: from the log when it committed one,
    /// else from the file; zeros for a page neither holds (a truncated
    /// file). No more pages than the file and the log hold, whatever a
    /// damaged header or commit frame claims.
    pub(crate) fn image(&self) -> Vec<u8> {
        let in_file = u32::try_from(self.file.len() / self.page_size).unwrap_or(u32::MAX);
        let last_logged = self.wal.keys().copied().max().unwrap_or(0);
        let count = self.count.min(in_file.max(last_logged));
        let mut image = Vec::with_capacity(count as usize * self.page_size);
        for number in 1..=count {
            match self
                .wal
                .get(&number)
                .copied()
                .or_else(|| self.file_page(number).ok())
            {
                Some(page) => image.extend_from_slice(page),
                None => image.resize(image.len() + self.page_size, 0),
            }
        }
        image
    }

    fn file_page(&self, number: u32) -> Result<&'a [u8], String> {
        // Checked: on 32-bit targets a far page's offset overflows `usize`.
        let start = (number as usize - 1).checked_mul(self.page_size);
        let end = start.and_then(|start| start.checked_add(self.page_size));
        start
            .zip(end)
            .and_then(|(start, end)| self.file.get(start..end))
            .ok_or_else(|| format!("page {number} is past the end of the file (truncated)"))
    }
}
