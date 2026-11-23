//! Native EXIF reader for the formats cards mostly hold: JPEG, TIFF-based
//! raws (NEF, ARW, CR2, DNG, ORF, RW2, PEF, ...) and Fujifilm RAF.
//!
//! Reads the first 64 KB of the file, plus a small read further in only when
//! a pointer leads past it. It returns a result only when it finds a usable
//! DateTimeOriginal or CreateDate in EXIF. Everything else (HEIC, CR3, video,
//! XMP-only dates, odd files) goes to exiftool as before, and the tag names
//! match exiftool's, so the catalog can't tell the two paths apart.

use std::borrow::Cow;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::exif::{norm_capture, Meta};
use crate::media;
use crate::util::read_full;

const HEAD: usize = 64 * 1024;
/// Largest single read past the head (a JPEG segment is at most 64 KB).
const MAX_READ: usize = 64 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_ASCII: usize = 256;

/// Random access to the file's bytes.
trait At {
    fn at(&self, off: u64, len: usize) -> Option<Cow<'_, [u8]>>;
}

impl At for [u8] {
    fn at(&self, off: u64, len: usize) -> Option<Cow<'_, [u8]>> {
        let s = usize::try_from(off).ok()?;
        self.get(s..s.checked_add(len)?).map(Cow::Borrowed)
    }
}

/// A file: its first 64 KB in memory, anything else by pread.
struct Src {
    f: File,
    head: Vec<u8>,
}

impl At for Src {
    fn at(&self, off: u64, len: usize) -> Option<Cow<'_, [u8]>> {
        if let Some(b) = self.head.at(off, len) {
            return Some(b);
        }
        if len > MAX_READ {
            return None;
        }
        let mut v = vec![0; len];
        self.f.read_exact_at(&mut v, off).ok()?;
        Some(Cow::Owned(v))
    }
}

#[derive(Default, Debug)]
struct Found {
    model: Option<String>,
    date_original: Option<String>,
    create_date: Option<String>,
}

impl Found {
    fn into_meta(self) -> Option<Meta> {
        let dates = [("DateTimeOriginal", self.date_original), ("CreateDate", self.create_date)];
        let (tag, capture) = dates.into_iter().find_map(|(t, v)| v.as_deref().and_then(norm_capture).map(|c| (t, c)))?;
        Some(Meta { capture: Some(capture), src: format!("exif:{tag}"), camera: self.model })
    }
}

/// Capture date and camera, if this file's EXIF has a date. None means "ask
/// exiftool".
pub fn read(p: &Path) -> Option<Meta> {
    if !media::is_image(&media::ext_lower(&p.to_string_lossy())) {
        return None;
    }
    let mut f = File::open(p).ok()?;
    let mut head = vec![0; HEAD];
    let n = read_full(&mut f, &mut head).ok()?;
    head.truncate(n);
    let src = Src { f, head };
    parse(&src)?.into_meta()
}

fn parse<A: At + ?Sized>(d: &A) -> Option<Found> {
    let magic = d.at(0, 16)?;
    if magic.starts_with(&[0xFF, 0xD8]) {
        jpeg(d, 0)
    } else if magic.starts_with(b"FUJIFILMCCD-RAW") {
        // Big-endian offset of the embedded JPEG, whose EXIF is the file's.
        let o = d.at(84, 4)?;
        jpeg(d, u32::from_be_bytes([o[0], o[1], o[2], o[3]]) as u64)
    } else if magic.starts_with(b"II") || magic.starts_with(b"MM") {
        tiff(d)
    } else {
        None
    }
}

/// Walk the JPEG's segments up to the image data, looking for APP1 "Exif".
fn jpeg<A: At + ?Sized>(d: &A, start: u64) -> Option<Found> {
    if d.at(start, 2)?[..] != [0xFF, 0xD8] {
        return None;
    }
    let mut pos = start + 2;
    for _ in 0..256 {
        let m = d.at(pos, 2)?;
        if m[0] != 0xFF {
            return None;
        }
        match m[1] {
            0xFF => pos += 1, // fill byte
            0xD9 | 0xDA => return None, // end of image / start of scan: no EXIF before the data
            0x01 | 0xD0..=0xD7 => pos += 2, // no length field
            marker => {
                let l = d.at(pos + 2, 2)?;
                let len = u16::from_be_bytes([l[0], l[1]]) as usize;
                if len < 2 {
                    return None;
                }
                if marker == 0xE1 {
                    let seg = d.at(pos + 4, len - 2)?;
                    if let Some(t) = seg.strip_prefix(b"Exif\0\0") {
                        return tiff(t);
                    }
                }
                pos += 2 + len as u64;
            }
        }
    }
    None
}

struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// The entry's 4-byte value field: the value itself, or its offset.
    field: [u8; 4],
}

struct Tiff<'a, A: At + ?Sized> {
    d: &'a A,
    le: bool,
}

impl<A: At + ?Sized> Tiff<'_, A> {
    fn u16(&self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    }

    fn ifd(&self, off: u64) -> Option<Vec<Entry>> {
        let n = self.u16(&self.d.at(off, 2)?) as usize;
        if n > MAX_ENTRIES {
            return None;
        }
        let blk = self.d.at(off + 2, 12 * n)?;
        Some(
            blk.chunks_exact(12)
                .map(|e| Entry { tag: self.u16(&e[0..]), typ: self.u16(&e[2..]), count: self.u32(&e[4..]), field: [e[8], e[9], e[10], e[11]] })
                .collect(),
        )
    }

    /// ASCII (2), or UNDEFINED (7) as some cameras write it.
    fn ascii(&self, e: &Entry) -> Option<String> {
        let n = e.count as usize;
        if !matches!(e.typ, 2 | 7) || n == 0 || n > MAX_ASCII {
            return None;
        }
        let bytes = if n <= 4 { Cow::Owned(e.field[..n].to_vec()) } else { self.d.at(self.u32(&e.field) as u64, n)? };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(n);
        let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// LONG (4) or IFD (13) pointer.
    fn pointer(&self, e: &Entry) -> Option<u64> {
        (matches!(e.typ, 4 | 13) && e.count == 1).then(|| self.u32(&e.field) as u64)
    }
}

/// IFD0 for the camera model and the EXIF IFD pointer, then the EXIF IFD for
/// the dates.
fn tiff<A: At + ?Sized>(d: &A) -> Option<Found> {
    let h = d.at(0, 8)?;
    let le = match &h[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let t = Tiff { d, le };
    // 42 is TIFF; Olympus ORF uses "RO"/"SR", Panasonic RW2 0x55.
    if ![42, 0x4F52, 0x5352, 0x55].contains(&t.u16(&h[2..])) {
        return None;
    }
    let mut found = Found::default();
    let mut exif_ifd = None;
    for e in t.ifd(t.u32(&h[4..]) as u64)? {
        match e.tag {
            0x0110 => found.model = t.ascii(&e),
            0x8769 => exif_ifd = t.pointer(&e),
            _ => {}
        }
    }
    if let Some(entries) = exif_ifd.and_then(|o| t.ifd(o)) {
        for e in entries {
            match e.tag {
                0x9003 => found.date_original = t.ascii(&e),
                0x9004 => found.create_date = t.ascii(&e),
                _ => {}
            }
        }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal TIFF: IFD0 { Model, ExifIFD } -> EXIF IFD { DTO, CreateDate }.
    fn tiff_bytes(le: bool, model: &str, dto: Option<&str>, create: Option<&str>) -> Vec<u8> {
        let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let ascii = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.push(0);
            b
        };
        let model_b = ascii(model);
        let dates: Vec<(u16, Vec<u8>)> =
            [(0x9003, dto), (0x9004, create)].into_iter().filter_map(|(t, v)| v.map(|v| (t, ascii(v)))).collect();

        // Layout: header(8) | IFD0 (2 + 2*12 + 4) | EXIF IFD (2 + n*12 + 4) | data
        let ifd0 = 8u32;
        let exif = ifd0 + 2 + 24 + 4;
        let data = exif + 2 + 12 * dates.len() as u32 + 4;
        let mut out = Vec::new();
        out.extend(if le { b"II" } else { b"MM" });
        out.extend(w16(42));
        out.extend(w32(ifd0));

        let mut blob: Vec<u8> = Vec::new();
        let put = |b: &[u8], blob: &mut Vec<u8>| {
            let off = data + blob.len() as u32;
            blob.extend(b);
            off
        };
        out.extend(w16(2));
        let mo = put(&model_b, &mut blob);
        out.extend(w16(0x0110));
        out.extend(w16(2));
        out.extend(w32(model_b.len() as u32));
        out.extend(w32(mo));
        out.extend(w16(0x8769));
        out.extend(w16(4));
        out.extend(w32(1));
        out.extend(w32(exif));
        out.extend(w32(0));

        out.extend(w16(dates.len() as u16));
        for (tag, b) in &dates {
            let o = put(b, &mut blob);
            out.extend(w16(*tag));
            out.extend(w16(2));
            out.extend(w32(b.len() as u32));
            out.extend(w32(o));
        }
        out.extend(w32(0));
        out.extend(blob);
        out
    }

    fn jpeg_bytes(tiff: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // An APP0 (JFIF) and an XMP APP1 before the EXIF one, as some apps write.
        out.extend([0xFF, 0xE0, 0x00, 0x07]);
        out.extend(b"JFIF\0");
        let xmp = b"http://ns.adobe.com/xap/1.0/\0<x/>";
        out.extend([0xFF, 0xE1]);
        out.extend(((xmp.len() + 2) as u16).to_be_bytes());
        out.extend(xmp);
        out.extend([0xFF, 0xE1]);
        out.extend(((tiff.len() + 8) as u16).to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(tiff);
        out.extend([0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        out
    }

    fn meta(b: &[u8]) -> Option<Meta> {
        parse(b)?.into_meta()
    }

    #[test]
    fn tiff_both_byte_orders() {
        for le in [true, false] {
            let m = meta(&tiff_bytes(le, "X-T5", Some("2026:10:13 14:30:22"), Some("2026:10:13 14:30:23"))).unwrap();
            assert_eq!(m.capture.as_deref(), Some("2026-10-13 14:30:22"));
            assert_eq!(m.src, "exif:DateTimeOriginal");
            assert_eq!(m.camera.as_deref(), Some("X-T5"));
        }
    }

    #[test]
    fn falls_back_to_create_date_then_none() {
        let m = meta(&tiff_bytes(true, "D850", Some("0000:00:00 00:00:00"), Some("2024:01:02 03:04:05"))).unwrap();
        assert_eq!(m.capture.as_deref(), Some("2024-01-02 03:04:05"));
        assert_eq!(m.src, "exif:CreateDate");
        // No usable date: leave it to exiftool.
        assert!(meta(&tiff_bytes(true, "D850", None, None)).is_none());
    }

    #[test]
    fn jpeg_skips_other_segments() {
        let m = meta(&jpeg_bytes(&tiff_bytes(false, "ILCE-7M4", Some("2025:05:06 07:08:09"), None))).unwrap();
        assert_eq!(m.capture.as_deref(), Some("2025-05-06 07:08:09"));
        assert_eq!(m.camera.as_deref(), Some("ILCE-7M4"));
    }

    #[test]
    fn raf_reads_embedded_jpeg() {
        let jpg = jpeg_bytes(&tiff_bytes(false, "X-T5", Some("2026:01:02 03:04:05"), None));
        let mut raf = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
        raf.resize(100, 0);
        raf[84..88].copy_from_slice(&100u32.to_be_bytes());
        raf.extend(&jpg);
        let m = meta(&raf).unwrap();
        assert_eq!(m.capture.as_deref(), Some("2026-01-02 03:04:05"));
    }

    #[test]
    fn garbage_is_rejected_not_panicking() {
        let good = jpeg_bytes(&tiff_bytes(true, "X", Some("2026:01:02 03:04:05"), None));
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        let mut bad = tiff_bytes(true, "X", Some("2026:01:02 03:04:05"), None);
        bad[8] = 0xFF; // absurd entry count
        bad[9] = 0xFF;
        assert!(meta(&bad).is_none());
        assert!(meta(b"\x89PNG\r\n\x1a\n........").is_none());
    }
}
