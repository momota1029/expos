//! 独立 7z 読み取り helper の要求と結果を運ぶ bounded frame。

use crate::archive_manifest::{NodeKind, RawRecord};
pub(super) const REQUEST_MAGIC: &[u8; 8] = b"GP7ZREAD";
pub(super) const RESPONSE_MAGIC: &[u8; 8] = b"GP7ZRES\0";
const VERSION: u16 = 1;
pub(super) const REQUEST_HEADER: usize = 72;
const RESPONSE_HEADER: usize = 32;
pub(super) const RESPONSE_CAPACITY: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_DLL_UNITS: usize = 32_767;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) dll_path: String,
    pub(super) source_raw: u64,
    pub(super) response_raw: u64,
    pub(super) volume_serial: u32,
    pub(super) file_index: u64,
    pub(super) source_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProtocolError {
    Bounds,
    Framing,
    Encoding,
    InvalidRequest,
    InvalidResponse,
    Mapping,
}

fn valid_raw(raw: u64) -> bool {
    raw != 0
        && raw <= usize::MAX as u64
        && !(usize::MAX as u64 - 5..=usize::MAX as u64).contains(&raw)
}

pub(super) fn valid_handle_roles(request: u64, source: u64, response: u64) -> bool {
    valid_raw(request)
        && valid_raw(source)
        && valid_raw(response)
        && request != source
        && request != response
        && source != response
}

pub(super) fn valid_absolute_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' {
        return bytes[0].is_ascii_alphabetic() && matches!(bytes[2], b'\\' | b'/');
    }
    let normalized = value.replace('/', "\\");
    let Some(rest) = normalized.strip_prefix("\\\\") else {
        return false;
    };
    let mut parts = rest.split('\\').filter(|part| !part.is_empty());
    parts.next().is_some() && parts.next().is_some()
}

#[cfg(test)]
pub(super) fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    encode_request_for_magic(request, REQUEST_MAGIC)
}

pub(super) fn encode_request_for_magic(
    request: &Request,
    expected_magic: &[u8; 8],
) -> Result<Vec<u8>, ProtocolError> {
    let dll: Vec<u16> = request.dll_path.encode_utf16().collect();
    if !valid_raw(request.source_raw)
        || !valid_raw(request.response_raw)
        || request.source_raw == request.response_raw
        || dll.is_empty()
        || dll.len() > MAX_DLL_UNITS
        || dll.contains(&0)
        || !valid_absolute_windows_path(&request.dll_path)
    {
        return Err(ProtocolError::InvalidRequest);
    }
    let total = REQUEST_HEADER
        .checked_add(dll.len().checked_mul(2).ok_or(ProtocolError::Bounds)?)
        .ok_or(ProtocolError::Bounds)?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(expected_magic);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(REQUEST_HEADER as u16).to_le_bytes());
    put_u32(
        &mut out,
        u32::try_from(total).map_err(|_| ProtocolError::Bounds)?,
    );
    put_u32(
        &mut out,
        u32::try_from(dll.len()).map_err(|_| ProtocolError::Bounds)?,
    );
    put_u32(&mut out, 0);
    out.extend_from_slice(&request.source_raw.to_le_bytes());
    out.extend_from_slice(&request.response_raw.to_le_bytes());
    put_u32(&mut out, request.volume_serial);
    put_u32(&mut out, 0);
    out.extend_from_slice(&request.file_index.to_le_bytes());
    out.extend_from_slice(&request.source_length.to_le_bytes());
    put_u32(&mut out, RESPONSE_CAPACITY as u32);
    put_u32(&mut out, 0);
    debug_assert_eq!(out.len(), REQUEST_HEADER);
    for unit in dll {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(out)
}

pub(super) fn decode_request(bytes: &[u8]) -> Result<Request, ProtocolError> {
    decode_request_for_magic(bytes, REQUEST_MAGIC)
}

pub(super) fn decode_request_for_magic(
    bytes: &[u8],
    expected_magic: &[u8; 8],
) -> Result<Request, ProtocolError> {
    if bytes.len() < REQUEST_HEADER || bytes.get(..8) != Some(expected_magic) {
        return Err(ProtocolError::Framing);
    }
    if read_u16(bytes, 8)? != VERSION
        || read_u16(bytes, 10)? as usize != REQUEST_HEADER
        || read_u32(bytes, 12)? as usize != bytes.len()
        || read_u32(bytes, 20)? != 0
        || read_u32(bytes, 44)? != 0
        || read_u32(bytes, 68)? != 0
        || read_u32(bytes, 64)? as usize != RESPONSE_CAPACITY
    {
        return Err(ProtocolError::Framing);
    }
    let units = read_u32(bytes, 16)? as usize;
    if units == 0
        || units > MAX_DLL_UNITS
        || REQUEST_HEADER.checked_add(units.checked_mul(2).ok_or(ProtocolError::Bounds)?)
            != Some(bytes.len())
    {
        return Err(ProtocolError::Bounds);
    }
    let source_raw = read_u64(bytes, 24)?;
    let response_raw = read_u64(bytes, 32)?;
    if !valid_raw(source_raw) || !valid_raw(response_raw) || source_raw == response_raw {
        return Err(ProtocolError::InvalidRequest);
    }
    let mut dll_units = Vec::with_capacity(units);
    for chunk in bytes[REQUEST_HEADER..].chunks_exact(2) {
        dll_units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    if dll_units.contains(&0) {
        return Err(ProtocolError::InvalidRequest);
    }
    let dll_path = String::from_utf16(&dll_units).map_err(|_| ProtocolError::Encoding)?;
    if !valid_absolute_windows_path(&dll_path) {
        return Err(ProtocolError::InvalidRequest);
    }
    Ok(Request {
        dll_path,
        source_raw,
        response_raw,
        volume_serial: read_u32(bytes, 40)?,
        file_index: read_u64(bytes, 48)?,
        source_length: read_u64(bytes, 56)?,
    })
}

/// payload を先に書き、frame 全体が完成してから完了 header を公開する。
pub(super) unsafe fn write_response(
    destination: *mut u8,
    capacity: usize,
    records: Result<&[RawRecord], ()>,
) -> Result<(), ProtocolError> {
    if destination.is_null() || capacity != RESPONSE_CAPACITY {
        return Err(ProtocolError::Mapping);
    }
    let (status, count, payload) = match records {
        Ok(records) => (0u32, records.len(), encode_records(records)?),
        Err(()) => (1u32, 0usize, Vec::new()),
    };
    if payload
        .len()
        .checked_add(RESPONSE_HEADER)
        .is_none_or(|n| n > capacity)
    {
        return Err(ProtocolError::Bounds);
    }
    if !payload.is_empty() {
        // SAFETY: 呼び出し元が capacity bytes の書き込み可能な mapping view を保証する。
        unsafe {
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                destination.add(RESPONSE_HEADER),
                payload.len(),
            )
        };
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    let mut header = [0u8; RESPONSE_HEADER];
    header[..8].copy_from_slice(RESPONSE_MAGIC);
    header[8..10].copy_from_slice(&VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&(RESPONSE_HEADER as u16).to_le_bytes());
    header[12..16].copy_from_slice(&1u32.to_le_bytes());
    header[16..20].copy_from_slice(&status.to_le_bytes());
    header[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[24..28].copy_from_slice(&(count as u32).to_le_bytes());
    // SAFETY: header は mapping の先頭に収まり、公開を確定する最後の書き込みになる。
    unsafe { std::ptr::copy_nonoverlapping(header.as_ptr(), destination, header.len()) };
    Ok(())
}

pub(super) fn decode_response(bytes: &[u8]) -> Result<Vec<RawRecord>, ProtocolError> {
    if bytes.len() < RESPONSE_HEADER
        || bytes.len() > RESPONSE_CAPACITY
        || bytes.get(..8) != Some(RESPONSE_MAGIC)
        || read_u16(bytes, 8)? != VERSION
        || read_u16(bytes, 10)? as usize != RESPONSE_HEADER
        || read_u32(bytes, 12)? != 1
        || read_u32(bytes, 28)? != 0
    {
        return Err(ProtocolError::Framing);
    }
    let status = read_u32(bytes, 16)?;
    let payload_len = read_u32(bytes, 20)? as usize;
    let count = read_u32(bytes, 24)? as usize;
    if RESPONSE_HEADER.checked_add(payload_len) != Some(bytes.len()) || count > MAX_RECORDS {
        return Err(ProtocolError::Bounds);
    }
    if status != 0 {
        if status != 1 || payload_len != 0 || count != 0 {
            return Err(ProtocolError::InvalidResponse);
        }
        return Err(ProtocolError::InvalidResponse);
    }
    decode_records(&bytes[RESPONSE_HEADER..], count)
}

fn encode_records(records: &[RawRecord]) -> Result<Vec<u8>, ProtocolError> {
    if records.len() > MAX_RECORDS {
        return Err(ProtocolError::Bounds);
    }
    let mut payload = Vec::new();
    let mut aggregate = 0usize;
    for record in records {
        if record.components.is_empty() || record.components.len() > MAX_DEPTH {
            return Err(ProtocolError::InvalidResponse);
        }
        let path = record.components.join("/");
        aggregate = aggregate
            .checked_add(path.len())
            .ok_or(ProtocolError::Bounds)?;
        if aggregate > MAX_NAME_BYTES {
            return Err(ProtocolError::Bounds);
        }
        let path_len = u32::try_from(path.len()).map_err(|_| ProtocolError::Bounds)?;
        payload.push(match record.kind {
            NodeKind::File => 1,
            NodeKind::Directory => 2,
        });
        payload.push(u8::from(record.declared_size.is_some()));
        payload.extend_from_slice(&[0, 0]);
        put_u32(&mut payload, path_len);
        payload.extend_from_slice(&record.declared_size.unwrap_or(0).to_le_bytes());
        payload.extend_from_slice(path.as_bytes());
        if payload
            .len()
            .checked_add(RESPONSE_HEADER)
            .is_none_or(|n| n > RESPONSE_CAPACITY)
        {
            return Err(ProtocolError::Bounds);
        }
    }
    Ok(payload)
}

fn decode_records(mut payload: &[u8], count: usize) -> Result<Vec<RawRecord>, ProtocolError> {
    let mut records = Vec::with_capacity(count);
    let mut aggregate = 0usize;
    for _ in 0..count {
        let header = take(&mut payload, 16)?;
        let kind = match header[0] {
            1 => NodeKind::File,
            2 => NodeKind::Directory,
            _ => return Err(ProtocolError::InvalidResponse),
        };
        let has_size = match header[1] {
            0 => false,
            1 => true,
            _ => return Err(ProtocolError::InvalidResponse),
        };
        if header[2..4] != [0, 0] {
            return Err(ProtocolError::Framing);
        }
        let path_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let size = u64::from_le_bytes(header[8..16].try_into().unwrap());
        if !has_size && size != 0 {
            return Err(ProtocolError::InvalidResponse);
        }
        aggregate = aggregate
            .checked_add(path_len)
            .ok_or(ProtocolError::Bounds)?;
        if path_len == 0 || aggregate > MAX_NAME_BYTES {
            return Err(ProtocolError::Bounds);
        }
        let path = std::str::from_utf8(take(&mut payload, path_len)?)
            .map_err(|_| ProtocolError::Encoding)?;
        // 長さだけでなく深さと空要素を借用中に検査し、敵対的な区切り文字列から大量の String を作らない。
        let mut depth = 0;
        for component in path.split('/') {
            depth += 1;
            if depth > MAX_DEPTH || component.is_empty() {
                return Err(ProtocolError::InvalidResponse);
            }
        }
        let components = path.split('/').map(str::to_owned).collect::<Vec<_>>();
        records.push(RawRecord {
            components,
            kind,
            declared_size: has_size.then_some(size),
        });
    }
    if !payload.is_empty() {
        return Err(ProtocolError::Framing);
    }
    Ok(records)
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], ProtocolError> {
    let (head, tail) = bytes.split_at_checked(len).ok_or(ProtocolError::Framing)?;
    *bytes = tail;
    Ok(head)
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn read_u16(bytes: &[u8], at: usize) -> Result<u16, ProtocolError> {
    Ok(u16::from_le_bytes(take_at(bytes, at)?))
}
fn read_u32(bytes: &[u8], at: usize) -> Result<u32, ProtocolError> {
    Ok(u32::from_le_bytes(take_at(bytes, at)?))
}
fn read_u64(bytes: &[u8], at: usize) -> Result<u64, ProtocolError> {
    Ok(u64::from_le_bytes(take_at(bytes, at)?))
}
fn take_at<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], ProtocolError> {
    bytes
        .get(at..at + N)
        .ok_or(ProtocolError::Framing)?
        .try_into()
        .map_err(|_| ProtocolError::Framing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_unicode_and_rejects_handle_collisions() {
        let request = Request {
            dll_path: r"C:\工具\7z.dll".into(),
            source_raw: 4,
            response_raw: 8,
            volume_serial: 9,
            file_index: 10,
            source_length: 11,
        };
        assert_eq!(
            decode_request(&encode_request(&request).unwrap()).unwrap(),
            request
        );
        let mut invalid = request.clone();
        invalid.source_raw = invalid.response_raw;
        assert_eq!(encode_request(&invalid), Err(ProtocolError::InvalidRequest));
        assert!(!valid_handle_roles(4, 4, 8));
        assert!(!valid_handle_roles(4, 8, 4));
        assert!(!valid_handle_roles(4, 8, 8));
        assert!(valid_handle_roles(4, 8, 12));
        let inspection = b"GP7ZINSP";
        let frame = encode_request_for_magic(&request, inspection).unwrap();
        assert!(decode_request(&frame).is_err());
        assert!(decode_request_for_magic(&frame, inspection).is_ok());
        assert!(decode_request_for_magic(&encode_request(&request).unwrap(), inspection).is_err());
    }

    #[test]
    fn empty_mixed_unicode_and_exact_sizes_roundtrip() {
        for records in [
            vec![],
            vec![
                RawRecord {
                    components: vec!["日本語".into(), "empty".into()],
                    kind: NodeKind::File,
                    declared_size: Some(0),
                },
                RawRecord {
                    components: vec!["空".into()],
                    kind: NodeKind::Directory,
                    declared_size: None,
                },
            ],
        ] {
            let payload = encode_records(&records).unwrap();
            let mut bytes = vec![0u8; RESPONSE_HEADER];
            bytes[..8].copy_from_slice(RESPONSE_MAGIC);
            bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
            bytes[10..12].copy_from_slice(&(RESPONSE_HEADER as u16).to_le_bytes());
            bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
            bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
            bytes[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes[24..28].copy_from_slice(&(records.len() as u32).to_le_bytes());
            bytes.extend(payload);
            assert_eq!(decode_response(&bytes).unwrap(), records);
        }
    }

    #[test]
    fn rejects_partial_header_payload_reserved_fields_and_trailing_bytes() {
        let mut partial = vec![0; RESPONSE_HEADER - 1];
        assert_eq!(decode_response(&partial), Err(ProtocolError::Framing));
        partial.resize(RESPONSE_HEADER, 0);
        assert_eq!(decode_response(&partial), Err(ProtocolError::Framing));
        let mut request = encode_request(&Request {
            dll_path: r"C:\7z.dll".into(),
            source_raw: 4,
            response_raw: 8,
            volume_serial: 1,
            file_index: 2,
            source_length: 3,
        })
        .unwrap();
        request[20] = 1;
        assert_eq!(decode_request(&request), Err(ProtocolError::Framing));
    }

    fn response(payload: &[u8], count: u32, status: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; RESPONSE_HEADER];
        bytes[..8].copy_from_slice(RESPONSE_MAGIC);
        bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&(RESPONSE_HEADER as u16).to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&status.to_le_bytes());
        bytes[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[24..28].copy_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn one_record(path: &[u8], kind: u8, size_flag: u8) -> Vec<u8> {
        let mut payload = Vec::with_capacity(16 + path.len());
        payload.extend_from_slice(&[kind, size_flag, 0, 0]);
        payload.extend_from_slice(&(path.len() as u32).to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(path);
        payload
    }

    #[test]
    fn rejects_response_header_payload_and_record_corruption() {
        let valid_payload = one_record(b"a", 1, 1);
        let valid = response(&valid_payload, 1, 0);
        assert_eq!(decode_response(&valid).unwrap().len(), 1);

        let mut bad = valid.clone();
        bad[8] = 2;
        assert_eq!(decode_response(&bad), Err(ProtocolError::Framing));
        let mut bad = valid.clone();
        bad[28] = 1;
        assert_eq!(decode_response(&bad), Err(ProtocolError::Framing));
        let mut bad = valid.clone();
        bad[16..20].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(decode_response(&bad), Err(ProtocolError::InvalidResponse));
        let mut bad = valid.clone();
        bad[24..28].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(decode_response(&bad), Err(ProtocolError::Framing));
        let mut bad = valid.clone();
        bad.push(0);
        assert_eq!(decode_response(&bad), Err(ProtocolError::Bounds));
        assert_eq!(
            decode_response(&valid[..valid.len() - 1]),
            Err(ProtocolError::Bounds)
        );
        assert_eq!(
            decode_response(&response(&one_record(b"a", 3, 1), 1, 0)),
            Err(ProtocolError::InvalidResponse)
        );
        assert_eq!(
            decode_response(&response(&one_record(b"a", 1, 2), 1, 0)),
            Err(ProtocolError::InvalidResponse)
        );
        let mut reserved = one_record(b"a", 1, 1);
        reserved[2] = 1;
        assert_eq!(
            decode_response(&response(&reserved, 1, 0)),
            Err(ProtocolError::Framing)
        );
        let mut mismatch = one_record(b"a", 1, 0);
        mismatch[8] = 1;
        assert_eq!(
            decode_response(&response(&mismatch, 1, 0)),
            Err(ProtocolError::InvalidResponse)
        );
    }

    #[test]
    fn response_name_limits_are_exact_and_separators_are_checked_before_allocation() {
        let exact = vec![b'a'; MAX_NAME_BYTES];
        let decoded = decode_response(&response(&one_record(&exact, 1, 1), 1, 0)).unwrap();
        assert_eq!(decoded[0].components[0].len(), MAX_NAME_BYTES);
        let over = vec![b'a'; MAX_NAME_BYTES + 1];
        assert_eq!(
            decode_response(&response(&one_record(&over, 1, 1), 1, 0)),
            Err(ProtocolError::Bounds)
        );
        let separators = vec![b'/'; MAX_NAME_BYTES];
        assert_eq!(
            decode_response(&response(&one_record(&separators, 1, 1), 1, 0)),
            Err(ProtocolError::InvalidResponse)
        );
        assert_eq!(
            decode_response(&response(
                &valid_payload_for_count_limit(),
                (MAX_RECORDS + 1) as u32,
                0
            )),
            Err(ProtocolError::Bounds)
        );
    }

    fn valid_payload_for_count_limit() -> Vec<u8> {
        Vec::new()
    }
}
