//! 7z inspection の indexed inventory を運ぶ bounded frame。
//!
//! Browse 用 wire format と magic を共有しない。browse の表示順を SDK index と
//! 誤認して payload を別 entry に結び付ける事故を防ぐ。

use super::seven_zip_extract_plan::IndexedRecord;
use crate::archive_manifest::NodeKind;

pub(super) const REQUEST_MAGIC: &[u8; 8] = b"GP7ZINSP";
pub(super) const RESPONSE_MAGIC: &[u8; 8] = b"GP7ZIXR\0";
const VERSION: u16 = 1;
const HEADER: usize = 32;
const CAPACITY: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
const FLAG_SIZE: u8 = 1;
const FLAG_CRC: u8 = 2;

// response header: magic[8], version u16, header_size u16, complete u32,
// status u32, payload_size u32, record_count u32, reserved u32.
// record prefix: index u32, kind u8, flags u8, reserved u16, size u64,
// crc32 u32, component_count u16, reserved u16; then repeated len u32 + UTF-8.

/// 戻り値は plan validation 前の untrusted SDK metadata。
pub(super) fn encode_response(records: &[IndexedRecord]) -> Result<Vec<u8>, ()> {
    if records.len() > MAX_RECORDS {
        return Err(());
    }
    let mut payload = Vec::new();
    let mut aggregate_name_bytes = 0usize;
    for record in records {
        if record.components.is_empty()
            || record.components.len() > MAX_DEPTH
            || record.components.iter().any(String::is_empty)
        {
            return Err(());
        }
        let full_name_bytes = record
            .components
            .iter()
            .try_fold(0usize, |total, part| {
                total
                    .checked_add(part.len())
                    .filter(|n| *n <= MAX_NAME_BYTES)
            })
            .ok_or(())?;
        let full_name_bytes = full_name_bytes
            .checked_add(record.components.len() - 1)
            .ok_or(())?;
        aggregate_name_bytes = aggregate_name_bytes
            .checked_add(full_name_bytes)
            .filter(|n| *n <= MAX_NAME_BYTES)
            .ok_or(())?;
        let mut flags = 0;
        if record.declared_size.is_some() {
            flags |= FLAG_SIZE;
        }
        if record.crc32.is_some() {
            flags |= FLAG_CRC;
        }
        put_u32(&mut payload, record.index);
        payload.push(match record.kind {
            NodeKind::File => 1,
            NodeKind::Directory => 2,
        });
        payload.push(flags);
        put_u16(&mut payload, 0);
        put_u64(&mut payload, record.declared_size.unwrap_or(0));
        put_u32(&mut payload, record.crc32.unwrap_or(0));
        put_u16(&mut payload, record.components.len() as u16);
        put_u16(&mut payload, 0);
        for part in &record.components {
            let length = u32::try_from(part.len()).map_err(|_| ())?;
            put_u32(&mut payload, length);
            payload.extend_from_slice(part.as_bytes());
        }
        if payload
            .len()
            .checked_add(HEADER)
            .filter(|n| *n <= CAPACITY)
            .is_none()
        {
            return Err(());
        }
    }
    let mut frame = vec![0; HEADER];
    frame[..8].copy_from_slice(RESPONSE_MAGIC);
    frame[8..10].copy_from_slice(&VERSION.to_le_bytes());
    frame[10..12].copy_from_slice(&(HEADER as u16).to_le_bytes());
    frame[12..16].copy_from_slice(&1u32.to_le_bytes());
    frame[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    frame[24..28].copy_from_slice(&(records.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub(super) fn decode_response(frame: &[u8]) -> Result<Vec<IndexedRecord>, ()> {
    if frame.len() < HEADER
        || frame.len() > CAPACITY
        || &frame[..8] != RESPONSE_MAGIC
        || read_u16(frame, 8)? != VERSION
        || read_u16(frame, 10)? as usize != HEADER
        || read_u32(frame, 12)? != 1
        || read_u32(frame, 28)? != 0
    {
        return Err(());
    }
    let count = read_u32(frame, 24)? as usize;
    let payload_len = read_u32(frame, 20)? as usize;
    if read_u32(frame, 16)? != 0
        || count > MAX_RECORDS
        || HEADER.checked_add(payload_len) != Some(frame.len())
    {
        return Err(());
    }
    let mut cursor = HEADER;
    let mut aggregate = 0usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let index = take_u32(frame, &mut cursor)?;
        let kind = match take_byte(frame, &mut cursor)? {
            1 => NodeKind::File,
            2 => NodeKind::Directory,
            _ => return Err(()),
        };
        let flags = take_byte(frame, &mut cursor)?;
        if flags & !(FLAG_SIZE | FLAG_CRC) != 0 || take_u16(frame, &mut cursor)? != 0 {
            return Err(());
        }
        let size = take_u64(frame, &mut cursor)?;
        let crc = take_u32(frame, &mut cursor)?;
        let depth = take_u16(frame, &mut cursor)? as usize;
        if take_u16(frame, &mut cursor)? != 0 || depth == 0 || depth > MAX_DEPTH {
            return Err(());
        }
        if flags & FLAG_SIZE == 0 && size != 0 || flags & FLAG_CRC == 0 && crc != 0 {
            return Err(());
        }
        let mut components = Vec::with_capacity(depth);
        for component_index in 0..depth {
            let length = take_u32(frame, &mut cursor)? as usize;
            if length == 0 {
                return Err(());
            }
            aggregate = aggregate
                .checked_add(length)
                .and_then(|total| total.checked_add(usize::from(component_index != 0)))
                .filter(|n| *n <= MAX_NAME_BYTES)
                .ok_or(())?;
            let end = cursor
                .checked_add(length)
                .filter(|end| *end <= frame.len())
                .ok_or(())?;
            components.push(
                std::str::from_utf8(&frame[cursor..end])
                    .map_err(|_| ())?
                    .to_owned(),
            );
            cursor = end;
        }
        records.push(IndexedRecord {
            index,
            components,
            kind,
            declared_size: (flags & FLAG_SIZE != 0).then_some(size),
            crc32: (flags & FLAG_CRC != 0).then_some(crc),
        });
    }
    if cursor != frame.len() {
        return Err(());
    }
    Ok(records)
}

pub(super) unsafe fn write_response(
    destination: *mut u8,
    capacity: usize,
    records: Result<&[IndexedRecord], ()>,
) -> Result<(), ()> {
    if destination.is_null() || capacity != CAPACITY {
        return Err(());
    }
    let (status, frame) = match records {
        Ok(records) => (0u32, encode_response(records)?),
        Err(()) => {
            let mut frame = vec![0; HEADER];
            frame[..8].copy_from_slice(RESPONSE_MAGIC);
            frame[8..10].copy_from_slice(&VERSION.to_le_bytes());
            frame[10..12].copy_from_slice(&(HEADER as u16).to_le_bytes());
            frame[12..16].copy_from_slice(&1u32.to_le_bytes());
            (1u32, frame)
        }
    };
    if frame.len() > capacity {
        return Err(());
    }
    let mut header = frame[..HEADER].to_vec();
    header[16..20].copy_from_slice(&status.to_le_bytes());
    unsafe {
        std::ptr::copy_nonoverlapping(
            frame[HEADER..].as_ptr(),
            destination.add(HEADER),
            frame.len() - HEADER,
        );
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        std::ptr::copy_nonoverlapping(header.as_ptr(), destination, HEADER);
    }
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn read_u16(bytes: &[u8], at: usize) -> Result<u16, ()> {
    Ok(u16::from_le_bytes(
        bytes
            .get(at..at + 2)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
fn read_u32(bytes: &[u8], at: usize) -> Result<u32, ()> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
fn take_byte(bytes: &[u8], at: &mut usize) -> Result<u8, ()> {
    let value = *bytes.get(*at).ok_or(())?;
    *at += 1;
    Ok(value)
}
fn take_u16(bytes: &[u8], at: &mut usize) -> Result<u16, ()> {
    let v = read_u16(bytes, *at)?;
    *at += 2;
    Ok(v)
}
fn take_u32(bytes: &[u8], at: &mut usize) -> Result<u32, ()> {
    let v = read_u32(bytes, *at)?;
    *at += 4;
    Ok(v)
}
fn take_u64(bytes: &[u8], at: &mut usize) -> Result<u64, ()> {
    let v = u64::from_le_bytes(
        bytes
            .get(*at..*at + 8)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    );
    *at += 8;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_records_roundtrip_optional_zero_crc_empty_and_unicode() {
        let records = vec![
            IndexedRecord {
                index: 4,
                components: vec!["雪".into()],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: Some(0),
            },
            IndexedRecord {
                index: 1,
                components: vec!["empty".into()],
                kind: NodeKind::Directory,
                declared_size: None,
                crc32: None,
            },
        ];
        assert_eq!(
            decode_response(&encode_response(&records).unwrap()).unwrap(),
            records
        );
    }

    #[test]
    fn rejects_wrong_purpose_reserved_flags_and_trailing_data() {
        let record = IndexedRecord {
            index: 0,
            components: vec!["valid.txt".into()],
            kind: NodeKind::File,
            declared_size: None,
            crc32: None,
        };
        let frame = encode_response(&[record]).unwrap();
        assert!(decode_response(&frame[..frame.len() - 1]).is_err());
        let mut trailing = frame.clone();
        trailing.push(0);
        assert!(decode_response(&trailing).is_err());
        let mut wrong_magic = frame.clone();
        wrong_magic[..8].copy_from_slice(b"GP7ZRES\0");
        assert!(decode_response(&wrong_magic).is_err());
        let mut bad_flags = frame;
        bad_flags[HEADER + 5] = 0x80;
        assert!(decode_response(&bad_flags).is_err());
        let mut bad_header_reserved = encode_response(&[IndexedRecord {
            index: 0,
            components: vec!["valid.txt".into()],
            kind: NodeKind::File,
            declared_size: None,
            crc32: None,
        }])
        .unwrap();
        bad_header_reserved[28] = 1;
        assert!(decode_response(&bad_header_reserved).is_err());
        let mut bad_record_reserved = encode_response(&[IndexedRecord {
            index: 0,
            components: vec!["valid.txt".into()],
            kind: NodeKind::File,
            declared_size: None,
            crc32: None,
        }])
        .unwrap();
        bad_record_reserved[HEADER + 6] = 1;
        assert!(decode_response(&bad_record_reserved).is_err());
        let mut bad_component_reserved = encode_response(&[IndexedRecord {
            index: 0,
            components: vec!["valid.txt".into()],
            kind: NodeKind::File,
            declared_size: None,
            crc32: None,
        }])
        .unwrap();
        bad_component_reserved[HEADER + 22] = 1;
        assert!(decode_response(&bad_component_reserved).is_err());
    }

    #[test]
    fn rejects_empty_paths_absent_storage_and_aggregate_name_overflow() {
        let record = IndexedRecord {
            index: 0,
            components: vec!["x".into()],
            kind: NodeKind::File,
            declared_size: None,
            crc32: None,
        };
        let frame = encode_response(&[record.clone()]).unwrap();
        let mut zero_depth = frame.clone();
        zero_depth[HEADER + 20..HEADER + 22].copy_from_slice(&0u16.to_le_bytes());
        assert!(decode_response(&zero_depth).is_err());
        let mut empty_component = frame.clone();
        empty_component[HEADER + 20..HEADER + 22].copy_from_slice(&1u16.to_le_bytes());
        empty_component[HEADER + 24..HEADER + 28].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode_response(&empty_component).is_err());
        let mut absent_size_data = frame.clone();
        absent_size_data[HEADER + 8] = 1;
        assert!(decode_response(&absent_size_data).is_err());
        let mut absent_crc_data = frame;
        absent_crc_data[HEADER + 16] = 1;
        assert!(decode_response(&absent_crc_data).is_err());

        let exact = vec![IndexedRecord {
            index: 0,
            components: vec!["x".repeat(MAX_NAME_BYTES - 2), "y".into()],
            kind: NodeKind::File,
            declared_size: Some(0),
            crc32: None,
        }];
        let exact_frame = encode_response(&exact).expect("8 MiB including separator is allowed");
        assert_eq!(decode_response(&exact_frame).unwrap(), exact);

        let over = vec![IndexedRecord {
            index: 0,
            components: vec!["x".repeat(MAX_NAME_BYTES - 1), "y".into()],
            kind: NodeKind::File,
            declared_size: Some(0),
            crc32: None,
        }];
        assert!(encode_response(&over).is_err());

        let cross_record_exact = vec![
            IndexedRecord {
                index: 0,
                components: vec!["a".repeat(MAX_NAME_BYTES / 2)],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            },
            IndexedRecord {
                index: 1,
                components: vec!["b".repeat(MAX_NAME_BYTES - MAX_NAME_BYTES / 2)],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            },
        ];
        let cross_record_frame = encode_response(&cross_record_exact)
            .expect("aggregate names across records may equal 8 MiB");
        assert_eq!(
            decode_response(&cross_record_frame).unwrap(),
            cross_record_exact
        );
        let cross_record_over = vec![
            IndexedRecord {
                index: 0,
                components: vec!["a".repeat(MAX_NAME_BYTES / 2)],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            },
            IndexedRecord {
                index: 1,
                components: vec!["b".repeat(MAX_NAME_BYTES - MAX_NAME_BYTES / 2 + 1)],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            },
        ];
        assert!(encode_response(&cross_record_over).is_err());

        // Build a structurally bounded frame without the encoder's name guard;
        // the decoder must apply the same aggregate separator-inclusive limit.
        let mut over_frame = exact_frame;
        let first_component_len = MAX_NAME_BYTES - 2;
        let second_len_offset = HEADER + 28 + first_component_len;
        // 長さを正しく保ち、名前合計だけが上限を 1 byte 超えるフレームにする。
        let second_len = 2u32;
        over_frame[second_len_offset..second_len_offset + 4]
            .copy_from_slice(&second_len.to_le_bytes());
        over_frame.push(b'y');
        let payload_len = (over_frame.len() - HEADER) as u32;
        over_frame[20..24].copy_from_slice(&payload_len.to_le_bytes());
        assert!(decode_response(&over_frame).is_err());
    }
}
