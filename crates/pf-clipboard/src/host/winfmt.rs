//! Pure byte conversions between the Win32 clipboard formats and the portable wire MIMEs
//! (`design/clipboard-and-file-transfer.md` §3.5). Kept free of any `windows`-crate dependency so it
//! compiles on every host and its unit tests exercise the fiddly bits (CF_HTML offset math, UTF-16
//! (de)serialization) without a Windows box. The [`super::windows`] backend is the only production
//! consumer; it wraps these with the actual `GetClipboardData`/`SetClipboardData` calls.
//!
//! Format map (Win32 ↔ wire):
//! * `CF_UNICODETEXT` (UTF-16LE + NUL) ↔ `text/plain;charset=utf-8`
//! * `"HTML Format"` (CF_HTML, UTF-8 + ASCII header) ↔ `text/html`
//! * `"Rich Text Format"` (raw RTF) ↔ `text/rtf`
//! * `"PNG"` (raw PNG) ↔ `image/png` — identity, handled inline by the backend.

// ---- CF_UNICODETEXT ↔ text/plain;charset=utf-8 -----------------------------------------------

/// `CF_UNICODETEXT` HGLOBAL bytes → UTF-8 wire bytes. `raw` is the exact `GlobalSize`-length buffer;
/// it holds little-endian UTF-16 code units terminated by a single `0x0000`.
pub fn text_from_utf16(raw: &[u8]) -> Vec<u8> {
    // Reinterpret each LE 2-byte pair as a UTF-16 code unit; a stray odd trailing byte (never present
    // in valid data) is dropped by `chunks_exact`.
    let mut units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Strip exactly one trailing NUL terminator if present (guard against eating a real code unit).
    if units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units).into_bytes()
}

/// UTF-8 wire bytes → `CF_UNICODETEXT` HGLOBAL bytes (UTF-16LE + a required `0x0000` terminator).
pub fn text_to_utf16(wire: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(wire);
    let mut out = Vec::with_capacity(wire.len() * 2 + 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // REQUIRED NUL terminator for CF_UNICODETEXT
    out
}

// ---- "HTML Format" (CF_HTML) ↔ text/html -----------------------------------------------------
//
// CF_HTML is UTF-8: an ASCII `Key:Value\r\n` header carrying byte offsets, then the HTML with
// `<!--StartFragment-->`/`<!--EndFragment-->` markers. Offsets are byte counts from buffer start;
// the offsets live *inside* the header, so their digit-width feeds back into the header length. The
// spec-blessed fix (Chromium/Firefox/LibreOffice) is fixed-width 10-digit zero-padded offsets, which
// makes the header a compile-time constant and every offset a one-pass computation.

const CF_HTML_HEADER: &str = "Version:0.9\r\n\
    StartHTML:0000000000\r\n\
    EndHTML:0000000000\r\n\
    StartFragment:0000000000\r\n\
    EndFragment:0000000000\r\n";
const CF_HTML_PREFIX: &str = "<html><body>\r\n<!--StartFragment-->";
const CF_HTML_SUFFIX: &str = "<!--EndFragment-->\r\n</body></html>";

/// UTF-8 HTML fragment (wire bytes) → a `CF_HTML` buffer, NUL-terminated. The trailing NUL is the
/// conventional CF_HTML expectation (§4); `EndHTML` still points at content end, before the NUL.
pub fn html_to_cf(wire: &[u8]) -> Vec<u8> {
    let fragment = String::from_utf8_lossy(wire);
    let start_html = CF_HTML_HEADER.len(); // 105
    let start_fragment = start_html + CF_HTML_PREFIX.len(); // 139
    let end_fragment = start_fragment + fragment.len(); // byte length — fragment may be multibyte
    let end_html = end_fragment + CF_HTML_SUFFIX.len();

    let mut buf = Vec::with_capacity(end_html + 1);
    buf.extend_from_slice(CF_HTML_HEADER.as_bytes());
    buf.extend_from_slice(CF_HTML_PREFIX.as_bytes());
    buf.extend_from_slice(fragment.as_bytes());
    buf.extend_from_slice(CF_HTML_SUFFIX.as_bytes());

    // Overwrite the four zero-padded fields in place, restricting the search to the header region so a
    // fragment that happens to contain "StartHTML:" can't fool the patcher.
    patch_offset(&mut buf[..start_html], b"StartHTML:", start_html);
    patch_offset(&mut buf[..start_html], b"EndHTML:", end_html);
    patch_offset(&mut buf[..start_html], b"StartFragment:", start_fragment);
    patch_offset(&mut buf[..start_html], b"EndFragment:", end_fragment);

    buf.push(0); // conventional NUL terminator
    buf
}

/// A `CF_HTML` buffer → the UTF-8 HTML fragment (wire bytes). Uses the `StartFragment`/`EndFragment`
/// offsets; falls back to `StartHTML`/`EndHTML`, then to the whole buffer, if the markers are absent.
pub fn html_from_cf(raw: &[u8]) -> Vec<u8> {
    let range = header_range(raw, b"StartFragment:", b"EndFragment:")
        .or_else(|| header_range(raw, b"StartHTML:", b"EndHTML:"));
    match range {
        // Content is UTF-8 per spec; return the exact slice (drop any trailing NUL for cleanliness).
        Some((start, end)) => {
            let slice = &raw[start..end];
            strip_trailing_nul(slice).to_vec()
        }
        None => strip_trailing_nul(raw).to_vec(),
    }
}

/// Resolve `[start_label .. end_label]` into a validated byte range within `raw`.
fn header_range(raw: &[u8], start_label: &[u8], end_label: &[u8]) -> Option<(usize, usize)> {
    let start = read_header_offset(raw, start_label)?;
    let end = read_header_offset(raw, end_label)?;
    if start <= end && end <= raw.len() {
        Some((start, end))
    } else {
        None
    }
}

/// Overwrite the 10 ASCII digits following `label` in `header` with `value`, zero-padded.
fn patch_offset(header: &mut [u8], label: &[u8], value: usize) {
    if let Some(pos) = find(header, label) {
        let at = pos + label.len();
        if at + 10 <= header.len() {
            let digits = format!("{value:010}");
            header[at..at + 10].copy_from_slice(digits.as_bytes());
        }
    }
}

/// Read the decimal integer following `label:` in the ASCII header. The colon-suffixed labels only
/// match in the header, never the marker comments (`<!--StartFragment-->`) or fragment text.
fn read_header_offset(raw: &[u8], label: &[u8]) -> Option<usize> {
    let mut at = find(raw, label)? + label.len();
    let mut n: usize = 0;
    let mut any = false;
    while let Some(&b) = raw.get(at) {
        if b.is_ascii_digit() {
            n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
            any = true;
            at += 1;
        } else {
            break; // stops at '\r'
        }
    }
    any.then_some(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---- "Rich Text Format" ↔ text/rtf -----------------------------------------------------------

/// `"Rich Text Format"` HGLOBAL bytes → RTF wire bytes. RTF is `{ }`-delimited; some producers append
/// a NUL past the final `}`, so strip a single trailing NUL to keep the wire payload byte-clean.
pub fn rtf_from_cf(raw: &[u8]) -> Vec<u8> {
    strip_trailing_nul(raw).to_vec()
}

fn strip_trailing_nul(b: &[u8]) -> &[u8] {
    match b.last() {
        Some(0) => &b[..b.len() - 1],
        _ => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_and_handles_terminator() {
        // UTF-8 → UTF-16LE+NUL → UTF-8.
        let wire = "héllo 🌍".as_bytes();
        let cf = text_to_utf16(wire);
        // Ends with a single 0x0000 terminator.
        assert_eq!(&cf[cf.len() - 2..], &[0, 0]);
        assert_eq!(text_from_utf16(&cf), wire);

        // A CF buffer *without* a terminator still decodes (no code unit eaten).
        let no_term: Vec<u8> = "hi".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(text_from_utf16(&no_term), b"hi");

        // Empty text → just the terminator → empty wire.
        assert_eq!(text_to_utf16(b""), vec![0, 0]);
        assert_eq!(text_from_utf16(&[0, 0]), b"");
    }

    #[test]
    fn cf_html_matches_the_spec_offsets() {
        // The worked example from the format reference: fragment "Hello".
        let cf = html_to_cf(b"Hello");
        let s = String::from_utf8(cf.clone()).unwrap();
        assert!(s.contains("StartHTML:0000000105"), "{s}");
        assert!(s.contains("EndHTML:0000000178"), "{s}");
        assert!(s.contains("StartFragment:0000000139"), "{s}");
        assert!(s.contains("EndFragment:0000000144"), "{s}");
        // The declared fragment range must slice back to exactly "Hello".
        let start = read_header_offset(&cf, b"StartFragment:").unwrap();
        let end = read_header_offset(&cf, b"EndFragment:").unwrap();
        assert_eq!(&cf[start..end], b"Hello");
        // Trailing NUL present, and EndHTML points *before* it.
        assert_eq!(*cf.last().unwrap(), 0);
        assert_eq!(read_header_offset(&cf, b"EndHTML:").unwrap(), cf.len() - 1);
    }

    #[test]
    fn cf_html_round_trips_including_multibyte() {
        for frag in [
            "Hello",
            "<b>bold</b> & <i>ital</i>",
            "café ☕ <span>x</span>",
            "",
        ] {
            let cf = html_to_cf(frag.as_bytes());
            assert_eq!(html_from_cf(&cf), frag.as_bytes(), "fragment {frag:?}");
        }
    }

    #[test]
    fn cf_html_extract_tolerates_foreign_producers() {
        // A producer that adds SourceURL and uses Version 1.0 — offsets must still drive extraction,
        // never a hardcoded 105-byte header.
        let fragment = "picked";
        let prefix = "<html><body><!--StartFragment-->";
        let header_body = format!(
            "Version:1.0\r\nStartHTML:{sh:010}\r\nEndHTML:{eh:010}\r\n\
             StartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\nSourceURL:https://x/\r\n",
            sh = 0,
            eh = 0,
            sf = 0,
            ef = 0,
        );
        // Compute real offsets against this ad-hoc layout.
        let start_html = header_body.len();
        let start_fragment = start_html + prefix.len();
        let end_fragment = start_fragment + fragment.len();
        let end_html = end_fragment + "<!--EndFragment--></body></html>".len();
        let full = format!(
            "Version:1.0\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\nSourceURL:https://x/\r\n\
             {prefix}{fragment}<!--EndFragment--></body></html>"
        );
        assert_eq!(html_from_cf(full.as_bytes()), fragment.as_bytes());
    }

    #[test]
    fn cf_html_extract_falls_back_without_markers() {
        // No fragment markers at all → whole buffer (minus any NUL).
        let mut b = b"<p>no markers</p>".to_vec();
        assert_eq!(html_from_cf(&b), b"<p>no markers</p>");
        b.push(0);
        assert_eq!(html_from_cf(&b), b"<p>no markers</p>");
    }

    #[test]
    fn rtf_strips_one_trailing_nul() {
        assert_eq!(rtf_from_cf(br"{\rtf1 hi}"), br"{\rtf1 hi}");
        assert_eq!(rtf_from_cf(b"{\\rtf1 hi}\0"), br"{\rtf1 hi}");
        // Only one NUL is stripped.
        assert_eq!(rtf_from_cf(b"x\0\0"), b"x\0");
    }

    #[test]
    fn png_dib_round_trip() {
        // A 3x2 RGBA PNG with distinct corner pixels survives PNG -> DIB -> PNG.
        let mut png = Vec::new();
        let img = image::RgbaImage::from_fn(3, 2, |x, y| {
            image::Rgba([x as u8 * 40, y as u8 * 80, 200, 255])
        });
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dib = image_to_dib(&png).expect("png -> dib");
        // BITMAPINFOHEADER sanity: 40-byte header, 3x2, 32bpp.
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), 2);
        let png2 = dib_to_png(&dib).expect("dib -> png");
        let back = image::load_from_memory(&png2).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (3, 2));
        assert_eq!(back.get_pixel(2, 1), img.get_pixel(2, 1));
    }

    #[test]
    fn dib_to_png_rejects_garbage() {
        assert!(dib_to_png(&[0u8; 10]).is_none());
        assert!(dib_to_png(&[0xFFu8; 200]).is_none());
    }
}

// ---- CF_DIB ↔ image/png ------------------------------------------------------------------------
//
// Most Windows apps speak the bitmap clipboard family (`CF_DIB`, from which Windows synthesizes
// `CF_BITMAP`/`CF_DIBV5`), not the registered `"PNG"` format — so images only interoperate when
// the backend converts. A CF_DIB HGLOBAL is a BMP file minus its 14-byte BITMAPFILEHEADER:
// BITMAPINFOHEADER (or V4/V5) + optional palette/masks + pixel rows.

/// Image wire bytes (PNG / JPEG / GIF — any format the `image` crate sniffs) → `CF_DIB` HGLOBAL
/// bytes (BITMAPINFOHEADER, 32bpp BGRA, BI_RGB, bottom-up). GIFs contribute their first frame.
/// `None` when the bytes don't decode — the caller leaves the format unrendered (empty paste).
pub fn image_to_dib(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 || w > 32767 || h > 32767 {
        return None;
    }
    let mut out = Vec::with_capacity(40 + w * h * 4);
    // BITMAPINFOHEADER: 32bpp BI_RGB needs no masks/palette; positive height = bottom-up rows.
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight (bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // XPels/YPels/ClrUsed/ClrImportant
                                       // Rows bottom-up, pixels BGRA (32bpp rows are already 4-byte aligned).
    for row in rgba.rows().rev() {
        for px in row {
            let [r, g, b, a] = px.0;
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(out)
}

/// `CF_DIB` HGLOBAL bytes → PNG wire bytes: prepend the BITMAPFILEHEADER a BMP decoder expects
/// (computing the pixel offset from the header + palette/mask layout) and re-encode. `None` on
/// any malformed input — the caller declines the fetch.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hdr_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if hdr_size < 40 || hdr_size > dib.len() {
        return None;
    }
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().ok()?) as usize;
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    let clr_used = u32::from_le_bytes(dib[32..36].try_into().ok()?) as usize;
    // Palette entries (≤8bpp) or BI_BITFIELDS masks follow the header before the pixels.
    let palette = if bit_count <= 8 {
        (if clr_used != 0 {
            clr_used
        } else {
            1 << bit_count
        }) * 4
    } else if compression == 3 {
        // BI_BITFIELDS: 3 DWORD masks (only when the header is a plain BITMAPINFOHEADER —
        // V4/V5 headers already contain the masks within hdr_size).
        if hdr_size == 40 {
            12
        } else {
            0
        }
    } else {
        0
    };
    let pixel_offset = 14 + hdr_size + palette;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}
