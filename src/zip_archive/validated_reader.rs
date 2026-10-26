use std::{
    cell::Cell,
    io::{self, Read, Seek, SeekFrom},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadSegment {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SeekPoint {
    pub(crate) offset: u64,
    pub(crate) first_read_cap: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub(crate) logical_eof: u64,
    pub(crate) eocd_start: u64,
    pub(crate) eocd: [u8; 22],
    pub(crate) segments: Vec<ReadSegment>,
    pub(crate) seek_points: Vec<SeekPoint>,
}

pub(crate) struct Reader<'a, R> {
    inner: R,
    layout: Layout,
    normal: &'a Cell<bool>,
    position: u64,
    initial_cap: Option<usize>,
    bootstrap_start: u64,
    bootstrap_len: u64,
    bootstrap_active: bool,
    bootstrap_done: bool,
    bootstrap_candidate: bool,
}

impl<'a, R> Reader<'a, R> {
    #[cfg(not(test))]
    pub(super) fn new(inner: R, layout: Layout, normal: &'a Cell<bool>) -> Self {
        Self::create(inner, layout, normal)
    }

    #[cfg(test)]
    pub(crate) fn new(inner: R, layout: Layout, normal: &'a Cell<bool>) -> Self {
        Self::create(inner, layout, normal)
    }

    fn create(inner: R, layout: Layout, normal: &'a Cell<bool>) -> Self {
        let bootstrap_len = layout.logical_eof.min(2048);
        Self {
            inner,
            normal,
            position: 0,
            bootstrap_start: layout.logical_eof - bootstrap_len,
            bootstrap_len,
            layout,
            initial_cap: None,
            bootstrap_active: false,
            bootstrap_done: false,
            bootstrap_candidate: false,
        }
    }

}

impl<R: Read> Read for Reader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.normal.get() {
            let count = self.inner.read(buffer)?;
            self.position = self.position.saturating_add(count as u64);
            return Ok(count);
        }
        if self.bootstrap_done && self.position >= self.layout.logical_eof {
            return Ok(0);
        }
        if self.bootstrap_candidate {
            let has_seek_point =
                self.layout.seek_points.iter().any(|point| {
                    point.offset == self.position && buffer.len() <= point.first_read_cap
                });
            if buffer.len() as u64 == self.bootstrap_len || !has_seek_point {
                self.bootstrap_active = true;
                self.initial_cap = None;
            }
            self.bootstrap_candidate = false;
        }
        if self.bootstrap_active {
            // 初回の末尾探索には承認済み EOCD だけを見せ、未検証の footer を候補にさせない。
            if self.position >= self.layout.logical_eof {
                self.bootstrap_active = false;
                self.bootstrap_done = true;
                return Ok(0);
            }
            let count = buffer
                .len()
                .min((self.layout.logical_eof - self.position) as usize);
            buffer[..count].fill(0);
            let start = self.position;
            let end = start + count as u64;
            let eocd_end = self.layout.eocd_start + 22;
            let overlap_start = start.max(self.layout.eocd_start);
            let overlap_end = end.min(eocd_end);
            if overlap_start < overlap_end {
                let source = (overlap_start - self.layout.eocd_start) as usize;
                let target = (overlap_start - start) as usize;
                let length = (overlap_end - overlap_start) as usize;
                buffer[target..target + length]
                    .copy_from_slice(&self.layout.eocd[source..source + length]);
            }
            self.position = end;
            if self.position == self.layout.logical_eof {
                self.bootstrap_active = false;
                self.bootstrap_done = true;
            }
            return Ok(count);
        }

        if let Some(cap) = self.initial_cap.take() {
            if buffer.len() > cap {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized first metadata read",
                ));
            }
        }
        let index = self
            .layout
            .segments
            .partition_point(|segment| segment.start <= self.position)
            .checked_sub(1);
        let segment = index
            .and_then(|index| self.layout.segments.get(index))
            .filter(|segment| self.position < segment.end)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "unapproved metadata read")
            })?;
        let available = (segment.end - self.position) as usize;
        if buffer.len() > available {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "metadata read crosses an approved segment",
            ));
        }
        let read_start = self.position;
        let count = self.inner.read(buffer)?;
        let read_end = read_start.saturating_add(count as u64);
        let eocd_end = self.layout.eocd_start + 22;
        let overlap_start = read_start.max(self.layout.eocd_start);
        let overlap_end = read_end.min(eocd_end);
        if overlap_start < overlap_end {
            let source = (overlap_start - self.layout.eocd_start) as usize;
            let target = (overlap_start - read_start) as usize;
            let length = (overlap_end - overlap_start) as usize;
            buffer[target..target + length]
                .copy_from_slice(&self.layout.eocd[source..source + length]);
        }
        self.position = self.position.saturating_add(count as u64);
        Ok(count)
    }
}

impl<R: Seek> Seek for Reader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if self.normal.get() {
            let position = self.inner.seek(from)?;
            self.position = position;
            return Ok(position);
        }
        let target = match from {
            SeekFrom::Current(0) => return Ok(self.position),
            SeekFrom::Start(position) => position,
            SeekFrom::End(delta) => self
                .layout
                .logical_eof
                .checked_add_signed(delta)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek overflow"))?,
            SeekFrom::Current(delta) => self
                .position
                .checked_add_signed(delta)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek overflow"))?,
        };
        if !self.bootstrap_done && !self.bootstrap_active && target == self.bootstrap_start {
            // 同じ位置が固定 header でもある場合、短い実読込に備えて物理 cursor も合わせる。
            if let Ok(index) = self
                .layout
                .seek_points
                .binary_search_by_key(&target, |point| point.offset)
            {
                let point = self.layout.seek_points[index];
                self.inner.seek(SeekFrom::Start(target))?;
                self.initial_cap = Some(point.first_read_cap);
            } else {
                self.initial_cap = None;
            }
            self.bootstrap_candidate = true;
            self.position = target;
            return Ok(target);
        }
        if target == self.layout.logical_eof {
            self.position = target;
            self.initial_cap = None;
            return Ok(target);
        }
        let point = self
            .layout
            .seek_points
            .binary_search_by_key(&target, |point| point.offset)
            .ok()
            .map(|index| self.layout.seek_points[index])
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "unapproved metadata seek")
            })?;
        self.inner.seek(SeekFrom::Start(target))?;
        self.position = target;
        self.initial_cap = Some(point.first_read_cap);
        Ok(target)
    }
}
