//! Binary crash blob format ("GCRX").
//!
//! The signal handler writes this format using only `libc::write` (no allocation).
//! The startup reader parses it in normal Rust context.

/// Magic bytes identifying a valid crash file.
pub const MAGIC: [u8; 4] = *b"GCRX";

/// Current format version.
pub const VERSION: u8 = 1;

/// Maximum backtrace frames captured in the signal handler.
pub const MAX_FRAMES: usize = 64;

/// Length of the null-padded version string field.
pub const VERSION_STRING_LEN: usize = 32;

/// Fixed header size (before the variable-length frames array).
/// magic(4) version(1) signal(1) si_code(4) si_addr(8) pid(4) timestamp(8) n_frames(2) app_version(32).
/// All multi-byte integers are little-endian.
pub const HEADER_SIZE: usize = 4 + 1 + 1 + 4 + 8 + 4 + 8 + 2 + VERSION_STRING_LEN;

/// Total maximum file size: header + 64 frames * 8 bytes each.
pub const MAX_FILE_SIZE: usize = HEADER_SIZE + MAX_FRAMES * 8;

/// Parsed crash data from a `last-crash.bin` file.
#[derive(Debug, Clone)]
pub struct CrashBlob {
    pub signal: u8,
    pub si_code: i32,
    pub si_addr: u64,
    pub pid: u32,
    pub timestamp: u64,
    pub frames: Vec<usize>,
    pub app_version: String,
}

impl CrashBlob {
    /// Parse a crash blob from bytes. Returns `None` if the data is invalid.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        if data.get(..4) != Some(MAGIC.as_slice()) {
            return None;
        }
        if data.get(4).copied() != Some(VERSION) {
            return None;
        }

        let signal = *data.get(5)?;
        let si_code = i32::from_le_bytes(data.get(6..10)?.try_into().ok()?);
        let si_addr = u64::from_le_bytes(data.get(10..18)?.try_into().ok()?);
        let pid = u32::from_le_bytes(data.get(18..22)?.try_into().ok()?);
        let timestamp = u64::from_le_bytes(data.get(22..30)?.try_into().ok()?);
        let n_frames = u16::from_le_bytes(data.get(30..32)?.try_into().ok()?) as usize;

        let version_bytes = data.get(32..32 + VERSION_STRING_LEN)?;
        let app_version = std::str::from_utf8(version_bytes)
            .unwrap_or("")
            .trim_end_matches('\0')
            .to_string();

        if n_frames > MAX_FRAMES {
            return None;
        }
        let frames_start = HEADER_SIZE;
        let frames_end = frames_start + n_frames * 8;
        if data.len() < frames_end {
            return None;
        }

        let mut frames = Vec::with_capacity(n_frames);
        for i in 0..n_frames {
            let offset = frames_start + i * 8;
            let addr = u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?);
            frames.push(addr as usize);
        }

        Some(CrashBlob {
            signal,
            si_code,
            si_addr,
            pid,
            timestamp,
            frames,
            app_version,
        })
    }
}

/// Helpers for writing fields in the signal handler using raw byte copies.
/// These are used by `handler.rs` — all operations are on a pre-allocated
/// static buffer, no allocation involved.
pub mod writer {
    use super::{MAGIC, VERSION, VERSION_STRING_LEN};

    /// Write the crash blob header into `buf`, returning bytes written. `buf` must be at least `HEADER_SIZE`.
    /// # Safety
    /// Called from a signal handler. The buffer must be valid and large enough.
    pub unsafe fn write_header(
        buf: &mut [u8],
        signal: u8,
        si_code: i32,
        si_addr: u64,
        pid: u32,
        timestamp: u64,
        n_frames: u16,
        app_version: &[u8],
    ) -> usize {
        let Some(magic) = buf.get_mut(..4) else {
            return 0;
        };
        magic.copy_from_slice(&MAGIC);
        let Some(ver) = buf.get_mut(4) else {
            return 0;
        };
        *ver = VERSION;
        let Some(sig) = buf.get_mut(5) else {
            return 0;
        };
        *sig = signal;
        let Some(dst) = buf.get_mut(6..10) else {
            return 0;
        };
        dst.copy_from_slice(&si_code.to_le_bytes());
        let Some(dst) = buf.get_mut(10..18) else {
            return 0;
        };
        dst.copy_from_slice(&si_addr.to_le_bytes());
        let Some(dst) = buf.get_mut(18..22) else {
            return 0;
        };
        dst.copy_from_slice(&pid.to_le_bytes());
        let Some(dst) = buf.get_mut(22..30) else {
            return 0;
        };
        dst.copy_from_slice(&timestamp.to_le_bytes());
        let Some(dst) = buf.get_mut(30..32) else {
            return 0;
        };
        dst.copy_from_slice(&n_frames.to_le_bytes());

        // Null-pad the version string field.
        let Some(version_field) = buf.get_mut(32..32 + VERSION_STRING_LEN) else {
            return 0;
        };
        version_field.fill(0);
        let copy_len = app_version.len().min(VERSION_STRING_LEN);
        if let (Some(dst), Some(src)) = (
            version_field.get_mut(..copy_len),
            app_version.get(..copy_len),
        ) {
            dst.copy_from_slice(src);
        }

        32 + VERSION_STRING_LEN
    }

    /// Write a single frame pointer into `buf` at the given offset. Returns the new offset.
    /// # Safety
    /// The caller must ensure `buf[offset..offset+8]` is valid.
    pub unsafe fn write_frame(buf: &mut [u8], offset: usize, addr: usize) -> usize {
        let Some(dst) = buf.get_mut(offset..offset + 8) else {
            return offset;
        };
        dst.copy_from_slice(&(addr as u64).to_le_bytes());
        offset + 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_crash_blob() {
        let mut buf = [0u8; MAX_FILE_SIZE];
        let version = b"0.1.169-alpha.2";
        let frames: &[usize] = &[0xdead_beef, 0xcafe_babe, 0x1234_5678];

        unsafe {
            let mut offset = writer::write_header(
                &mut buf,
                10, // SIGBUS on macOS
                2,  // BUS_ADRERR
                0x7f8a_1234_0000,
                42,
                1_712_678_587,
                frames.len() as u16,
                version,
            );
            for &frame in frames {
                offset = writer::write_frame(&mut buf, offset, frame);
            }

            let Some(blob_bytes) = buf.get(..offset) else {
                panic!("write offset in range: {offset}");
            };
            let blob = CrashBlob::parse(blob_bytes).expect("parse should succeed");
            assert_eq!(blob.signal, 10);
            assert_eq!(blob.si_code, 2);
            assert_eq!(blob.si_addr, 0x7f8a_1234_0000);
            assert_eq!(blob.pid, 42);
            assert_eq!(blob.timestamp, 1_712_678_587);
            assert_eq!(blob.frames, frames);
            assert_eq!(blob.app_version, "0.1.169-alpha.2");
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0u8; HEADER_SIZE];
        if let Some(dst) = buf.get_mut(..4) {
            dst.copy_from_slice(b"NOPE");
        }
        assert!(CrashBlob::parse(&buf).is_none());
    }

    #[test]
    fn rejects_truncated_data() {
        assert!(CrashBlob::parse(&[]).is_none());
        assert!(CrashBlob::parse(&MAGIC).is_none());
    }
}
