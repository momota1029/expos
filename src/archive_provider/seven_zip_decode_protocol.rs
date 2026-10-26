use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::seven_zip_extract_plan::{IndexedRecord, Validated7zExtractPlan};

pub(super) const REQUEST_MAGIC: &[u8; 8] = b"GP7ZDECQ";
pub(super) const RECEIPT_MAGIC: &[u8; 8] = b"GP7ZDECR";
pub(super) const RECEIPT_LEN: usize = RECEIPT_SIZE;
const VERSION: u16 = 1;
const REQUEST_HEADER: usize = 116;
const RECEIPT_SIZE: usize = 124;
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024 + 512;
const MAX_DLL_UNITS: usize = 32_767;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileFact {
    pub(super) volume_serial: u32,
    pub(super) file_index: u64,
    pub(super) length: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub(super) invocation_id: [u8; 16],
    pub(super) dll_path: String,
    /// Source, spool and response handles. The request-section role is supplied
    /// separately to `valid_handle_roles` because it is the frame carrier.
    pub(super) handles: [u64; 3],
    pub(super) source: FileFact,
    pub(super) spool: FileFact,
    pub(super) plan: Arc<Validated7zExtractPlan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    pub(super) invocation_id: [u8; 16],
    pub(super) source: FileFact,
    pub(super) spool: FileFact,
    pub(super) plan_digest: [u8; 32],
    pub(super) final_total: u64,
    pub(super) final_spool_length: u64,
}

pub(super) fn encode_request(request: &Request) -> Result<Vec<u8>, ()> {
    validate_request_fields(request)?;
    let plan_bytes = canonical_plan_bytes(&request.plan)?;
    let dll = request.dll_path.encode_utf16().collect::<Vec<_>>();
    if dll.len() > MAX_DLL_UNITS {
        return Err(());
    }
    let dll_len = dll.len().checked_mul(2).ok_or(())?;
    let frame_len = REQUEST_HEADER
        .checked_add(dll_len)
        .and_then(|n| n.checked_add(plan_bytes.len()))
        .filter(|n| *n <= MAX_REQUEST_BYTES)
        .ok_or(())?;

    let mut frame = vec![0; REQUEST_HEADER];
    frame[..8].copy_from_slice(REQUEST_MAGIC);
    put_u16(&mut frame, 8, VERSION);
    put_u16(&mut frame, 10, REQUEST_HEADER as u16);
    put_u32(&mut frame, 12, frame_len as u32);
    frame[20..36].copy_from_slice(&request.invocation_id);
    put_u32(&mut frame, 36, dll.len() as u32);
    put_fact(&mut frame, 40, request.source);
    put_fact(&mut frame, 60, request.spool);
    put_u64(&mut frame, 80, request.handles[0]);
    put_u64(&mut frame, 88, request.handles[1]);
    put_u64(&mut frame, 96, request.handles[2]);
    put_u32(&mut frame, 104, plan_bytes.len() as u32);
    put_u64(&mut frame, 108, request.plan.total_size());
    for unit in dll {
        frame.extend_from_slice(&unit.to_le_bytes());
    }
    frame.extend_from_slice(&plan_bytes);
    Ok(frame)
}

pub(super) fn decode_request(frame: &[u8]) -> Result<Request, ()> {
    if frame.len() < REQUEST_HEADER
        || frame.len() > MAX_REQUEST_BYTES
        || &frame[..8] != REQUEST_MAGIC
        || read_u16(frame, 8)? != VERSION
        || read_u16(frame, 10)? as usize != REQUEST_HEADER
        || read_u32(frame, 12)? as usize != frame.len()
        || read_u32(frame, 16)? != 0
    {
        return Err(());
    }
    let dll_units = read_u32(frame, 36)? as usize;
    let plan_len = read_u32(frame, 104)? as usize;
    let dll_bytes = dll_units.checked_mul(2).ok_or(())?;
    let payload_start = REQUEST_HEADER.checked_add(dll_bytes).ok_or(())?;
    let expected_len = payload_start.checked_add(plan_len).ok_or(())?;
    if dll_units == 0 || dll_units > MAX_DLL_UNITS || expected_len != frame.len() {
        return Err(());
    }
    let dll_path = String::from_utf16(
        &frame[REQUEST_HEADER..payload_start]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>(),
    )
    .map_err(|_| ())?;
    if !super::seven_zip_reader_protocol::valid_absolute_windows_path(&dll_path) {
        return Err(());
    }
    let plan = Arc::new(decode_canonical_plan(&frame[payload_start..])?);
    if read_u64(frame, 108)? != plan.total_size() {
        return Err(());
    }
    let request = Request {
        invocation_id: frame[20..36].try_into().map_err(|_| ())?,
        dll_path,
        handles: [
            read_u64(frame, 80)?,
            read_u64(frame, 88)?,
            read_u64(frame, 96)?,
        ],
        source: read_fact(frame, 40)?,
        spool: read_fact(frame, 60)?,
        plan,
    };
    validate_request_fields(&request)?;
    Ok(request)
}

pub(super) fn bounded_request_frame_len(header: &[u8]) -> Result<usize, ()> {
    if header.len() != 16
        || &header[..8] != REQUEST_MAGIC
        || read_u16(header, 8)? != VERSION
        || read_u16(header, 10)? as usize != REQUEST_HEADER
    {
        return Err(());
    }
    let length = read_u32(header, 12)? as usize;
    if !(REQUEST_HEADER..=MAX_REQUEST_BYTES).contains(&length) {
        return Err(());
    }
    Ok(length)
}

pub(super) fn valid_handle_roles(request_mapping: u64, request: &Request) -> bool {
    valid_raw_handle(request_mapping)
        && request
            .handles
            .iter()
            .all(|handle| valid_raw_handle(*handle))
        && request_mapping != request.handles[0]
        && request_mapping != request.handles[1]
        && request_mapping != request.handles[2]
        && request.handles[0] != request.handles[1]
        && request.handles[0] != request.handles[2]
        && request.handles[1] != request.handles[2]
}

fn valid_raw_handle(raw: u64) -> bool {
    raw != 0
        && raw <= usize::MAX as u64
        // Windows pseudo handles occupy the signed sentinel range near -1.
        && !(usize::MAX as u64 - 15..=usize::MAX as u64).contains(&raw)
}

pub(super) fn make_receipt(request: &Request) -> Result<Receipt, ()> {
    Ok(Receipt {
        invocation_id: request.invocation_id,
        source: request.source,
        spool: request.spool,
        plan_digest: plan_digest(&request.plan)?,
        final_total: request.plan.total_size(),
        final_spool_length: request.plan.total_size(),
    })
}

pub(super) fn encode_receipt(receipt: &Receipt) -> Result<[u8; RECEIPT_SIZE], ()> {
    if receipt.final_spool_length != receipt.final_total {
        return Err(());
    }
    let mut frame = [0; RECEIPT_SIZE];
    frame[..8].copy_from_slice(RECEIPT_MAGIC);
    put_u16(&mut frame, 8, VERSION);
    put_u16(&mut frame, 10, RECEIPT_SIZE as u16);
    put_u32(&mut frame, 12, RECEIPT_SIZE as u32);
    frame[20..36].copy_from_slice(&receipt.invocation_id);
    put_fact(&mut frame, 36, receipt.source);
    put_fact(&mut frame, 56, receipt.spool);
    frame[76..108].copy_from_slice(&receipt.plan_digest);
    put_u64(&mut frame, 108, receipt.final_total);
    put_u64(&mut frame, 116, receipt.final_spool_length);
    Ok(frame)
}

pub(super) fn decode_receipt(frame: &[u8]) -> Result<Receipt, ()> {
    if frame.len() != RECEIPT_SIZE
        || &frame[..8] != RECEIPT_MAGIC
        || read_u16(frame, 8)? != VERSION
        || read_u16(frame, 10)? as usize != RECEIPT_SIZE
        || read_u32(frame, 12)? as usize != RECEIPT_SIZE
        || read_u32(frame, 16)? != 0
    {
        return Err(());
    }
    let receipt = Receipt {
        invocation_id: frame[20..36].try_into().map_err(|_| ())?,
        source: read_fact(frame, 36)?,
        spool: read_fact(frame, 56)?,
        plan_digest: frame[76..108].try_into().map_err(|_| ())?,
        final_total: read_u64(frame, 108)?,
        final_spool_length: read_u64(frame, 116)?,
    };
    if receipt.final_total != receipt.final_spool_length {
        return Err(());
    }
    Ok(receipt)
}

pub(super) fn receipt_matches(request: &Request, receipt: &Receipt) -> Result<bool, ()> {
    Ok(receipt.invocation_id == request.invocation_id
        && receipt.source == request.source
        && receipt.spool == request.spool
        && receipt.plan_digest == plan_digest(&request.plan)?
        && receipt.final_total == request.plan.total_size()
        && receipt.final_spool_length == request.plan.total_size())
}

fn validate_request_fields(request: &Request) -> Result<(), ()> {
    if request.invocation_id == [0; 16]
        || !super::seven_zip_reader_protocol::valid_absolute_windows_path(&request.dll_path)
        || request.dll_path.encode_utf16().count() > MAX_DLL_UNITS
        || request.dll_path.encode_utf16().any(|unit| unit == 0)
        || request.spool.length != 0
        || request.source.volume_serial == request.spool.volume_serial
            && request.source.file_index == request.spool.file_index
        || request.handles.contains(&0)
        || request
            .handles
            .iter()
            .any(|handle| !valid_raw_handle(*handle))
        || request.handles[0] == request.handles[1]
        || request.handles[0] == request.handles[2]
        || request.handles[1] == request.handles[2]
    {
        return Err(());
    }
    Ok(())
}

fn canonical_plan_bytes(plan: &Validated7zExtractPlan) -> Result<Vec<u8>, ()> {
    let records = plan
        .entries()
        .iter()
        .map(|entry| IndexedRecord {
            index: entry.index(),
            components: entry.components().to_vec(),
            kind: entry.kind(),
            declared_size: entry.declared_size(),
            crc32: entry.crc32(),
        })
        .collect::<Vec<_>>();
    super::seven_zip_inspection_protocol::encode_response(&records)
}

fn decode_canonical_plan(bytes: &[u8]) -> Result<Validated7zExtractPlan, ()> {
    let records = super::seven_zip_inspection_protocol::decode_response(bytes)?;
    let plan = Validated7zExtractPlan::validate(records).map_err(|_| ())?;
    if canonical_plan_bytes(&plan)? != bytes {
        return Err(());
    }
    Ok(plan)
}

pub(super) fn plan_digest(plan: &Validated7zExtractPlan) -> Result<[u8; 32], ()> {
    Ok(Sha256::digest(canonical_plan_bytes(plan)?).into())
}

fn put_fact(frame: &mut [u8], offset: usize, fact: FileFact) {
    put_u32(frame, offset, fact.volume_serial);
    put_u64(frame, offset + 4, fact.file_index);
    put_u64(frame, offset + 12, fact.length);
}

fn read_fact(frame: &[u8], offset: usize) -> Result<FileFact, ()> {
    Ok(FileFact {
        volume_serial: read_u32(frame, offset)?,
        file_index: read_u64(frame, offset + 4)?,
        length: read_u64(frame, offset + 12)?,
    })
}

fn put_u16(frame: &mut [u8], offset: usize, value: u16) {
    frame[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(frame: &mut [u8], offset: usize, value: u32) {
    frame[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(frame: &mut [u8], offset: usize, value: u64) {
    frame[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn read_u16(frame: &[u8], offset: usize) -> Result<u16, ()> {
    Ok(u16::from_le_bytes(
        frame
            .get(offset..offset + 2)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
fn read_u32(frame: &[u8], offset: usize) -> Result<u32, ()> {
    Ok(u32::from_le_bytes(
        frame
            .get(offset..offset + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
fn read_u64(frame: &[u8], offset: usize) -> Result<u64, ()> {
    Ok(u64::from_le_bytes(
        frame
            .get(offset..offset + 8)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_manifest::NodeKind;

    fn request() -> Request {
        Request {
            invocation_id: [7; 16],
            dll_path: r"C:\Program Files\7-Zip\7z.dll".into(),
            handles: [11, 12, 13],
            source: FileFact {
                volume_serial: 1,
                file_index: 2,
                length: 99,
            },
            spool: FileFact {
                volume_serial: 1,
                file_index: 3,
                length: 0,
            },
            plan: Arc::new(
                Validated7zExtractPlan::validate(vec![IndexedRecord {
                    index: 0,
                    components: vec!["folder".into(), "file.bin".into()],
                    kind: NodeKind::File,
                    declared_size: Some(5),
                    crc32: Some(0x1234),
                }])
                .unwrap(),
            ),
        }
    }

    #[test]
    fn request_roundtrip_and_frame_adversaries() {
        let request = request();
        let frame = encode_request(&request).unwrap();
        let decoded = decode_request(&frame).unwrap();
        assert_eq!(decoded.invocation_id, request.invocation_id);
        assert_eq!(decoded.dll_path, request.dll_path);
        assert_eq!(decoded.plan, request.plan);
        assert!(valid_handle_roles(10, &decoded));
        assert!(!valid_handle_roles(12, &decoded));
        assert!(!valid_handle_roles(u64::MAX, &decoded));
        let mut bad_roles = decoded_for_test();
        bad_roles.handles[0] = u64::MAX;
        assert!(encode_request(&bad_roles).is_err());
        let mut bad_roles = decoded_for_test();
        bad_roles.handles[1] = usize::MAX as u64 - 8;
        assert!(encode_request(&bad_roles).is_err());

        let mut bad = frame.clone();
        bad[0] ^= 1;
        assert!(decode_request(&bad).is_err());
        let mut bad = frame.clone();
        bad[8] = 2;
        assert!(decode_request(&bad).is_err());
        let mut bad = frame.clone();
        bad[16] = 1;
        assert!(decode_request(&bad).is_err());
        let mut bad = frame.clone();
        bad[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_request(&bad).is_err());
        let mut bad = frame.clone();
        bad[108] ^= 1;
        assert!(decode_request(&bad).is_err());
        let mut bad = frame.clone();
        bad[20] = 0;
        bad[21..36].fill(0);
        assert!(decode_request(&bad).is_err());
        let mut trailing = frame.clone();
        trailing.push(0);
        assert!(decode_request(&trailing).is_err());
        assert!(decode_request(&frame[..frame.len() - 1]).is_err());
        let mut bad_plan = frame;
        let dll_units = u32::from_le_bytes(bad_plan[36..40].try_into().unwrap()) as usize;
        let plan_frame = REQUEST_HEADER + dll_units * 2;
        // 単一レコードの index を飛ばし、plan の連続 index 検証で拒否させる。
        bad_plan[plan_frame + 32..plan_frame + 36].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode_request(&bad_plan).is_err());

        let mut invalid_handle = request;
        invalid_handle.handles[1] = u64::MAX;
        assert!(encode_request(&invalid_handle).is_err());
        let mut invalid_handle = decoded_for_test();
        invalid_handle.handles[2] = usize::MAX as u64 - 4;
        assert!(encode_request(&invalid_handle).is_err());
        let mut oversized = [0; 16];
        oversized[..8].copy_from_slice(REQUEST_MAGIC);
        put_u16(&mut oversized, 8, VERSION);
        put_u16(&mut oversized, 10, REQUEST_HEADER as u16);
        put_u32(&mut oversized, 12, (MAX_REQUEST_BYTES + 1) as u32);
        assert!(bounded_request_frame_len(&oversized).is_err());
    }

    #[test]
    fn receipt_is_fixed_and_bound_to_every_request_fact() {
        let request = request();
        let receipt = make_receipt(&request).unwrap();
        let encoded = encode_receipt(&receipt).unwrap();
        let decoded = decode_receipt(&encoded).unwrap();
        assert!(receipt_matches(&request, &decoded).unwrap());
        for mutate in 0..12 {
            let mut stale = decoded.clone();
            match mutate {
                0 => stale.invocation_id[0] ^= 1,
                1 => stale.source.volume_serial ^= 1,
                2 => stale.source.file_index ^= 1,
                3 => stale.source.length ^= 1,
                4 => stale.spool.volume_serial ^= 1,
                5 => stale.spool.file_index ^= 1,
                6 => stale.spool.length ^= 1,
                7 => stale.plan_digest[0] ^= 1,
                8 => stale.final_total ^= 1,
                9 => stale.final_spool_length ^= 1,
                10 => stale.source = request.spool,
                _ => stale.spool = request.source,
            }
            if stale.final_total == stale.final_spool_length {
                assert!(!receipt_matches(&request, &stale).unwrap());
            } else {
                assert!(encode_receipt(&stale).is_err());
            }
        }
        let mut reserved = encoded.to_vec();
        reserved[16] = 1;
        assert!(decode_receipt(&reserved).is_err());
        assert!(decode_receipt(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(decode_receipt(&trailing).is_err());
        let mut wrong_purpose = encoded;
        wrong_purpose[0] ^= 1;
        assert!(decode_receipt(&wrong_purpose).is_err());
        let mut wrong_version = encoded;
        wrong_version[8] = 2;
        assert!(decode_receipt(&wrong_version).is_err());
        let mut wrong_total = decoded.clone();
        wrong_total.final_total += 1;
        assert!(encode_receipt(&wrong_total).is_err());
        let mut wrong_spool_length = decoded.clone();
        wrong_spool_length.final_spool_length += 1;
        assert!(encode_receipt(&wrong_spool_length).is_err());

        let mut different_plan = request.clone();
        different_plan.plan = Arc::new(
            Validated7zExtractPlan::validate(vec![IndexedRecord {
                index: 0,
                components: vec!["other.bin".into()],
                kind: NodeKind::File,
                declared_size: Some(5),
                crc32: Some(0x1234),
            }])
            .unwrap(),
        );
        assert_eq!(different_plan.plan.total_size(), request.plan.total_size());
        assert!(!receipt_matches(&request, &make_receipt(&different_plan).unwrap()).unwrap());
    }

    fn decoded_for_test() -> Request {
        decode_request(&encode_request(&request()).unwrap()).unwrap()
    }
}
