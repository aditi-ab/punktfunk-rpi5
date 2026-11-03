use super::*;

fn stats() -> Vec<HudLine> {
    [
        "player 1",
        "1920x1080@60 · HEVC 10-bit · v4l2-request · HDR→SDR",
        "received 61 fps · decoded 61 · presented 60 · 17.0 Mb/s",
        "host 0.9 ms · decode 17.1 ms · display 25.0 ms (avg)",
        "lost 0.0% · skipped 0.0% · rtt 2.5 ms",
        "host min/max 0.5/3.4 ms · display queue 6.3 + render 8.8 + compositor 9.9 ms",
        "target 20 Mb/s",
        "audio buffer 59 ms · a/v -2 ms",
        "present: Immediate · vrr no",
    ]
    .into_iter()
    .map(|text| HudLine {
        role: Role::Primary,
        text: text.into(),
    })
    .collect()
}

fn assert_pixels_in_band(surface: &mut Surface, (y, height): (u32, u32)) {
    let (w, h) = (surface.width(), surface.height());
    assert!(y + height <= h as u32);
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    assert!(surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)));
    let mut painted = false;
    for (row, bytes) in pixels.chunks_exact((w * 4) as usize).enumerate() {
        if bytes.iter().any(|b| *b != 0) {
            painted = true;
            assert!(
                (y..y + height).contains(&(row as u32)),
                "painted row {row} outside {y}..{}",
                y + height
            );
        }
    }
    assert!(painted);
}

#[test]
fn stats_damage_contains_raster_at_every_corner_and_scale() {
    let fonts = crate::theme::build_fonts().unwrap();
    let font = fonts.font(W::Regular, 14.0);
    for (width, height) in [(1920, 1080), (800, 600)] {
        for corner in HudCorner::ALL {
            // UI and stats scales multiply; their joint range is 0.25 through 16.
            for scale in [0.25, 1.0, 4.0, 16.0] {
                let mut surface = skia_safe::surfaces::raster_n32_premul((width, height)).unwrap();
                surface.canvas().clear(skia_safe::Color::TRANSPARENT);
                let rect = draw_osd_panel(
                    surface.canvas(),
                    &font,
                    &stats(),
                    (width as u32, height as u32),
                    scale,
                    corner,
                );
                let mut damage = DamageBand::new(height as u32, false);
                damage.include(rect);
                let band = damage.finish();
                assert_pixels_in_band(&mut surface, band);
                if width == 1920 && scale == 1.0 {
                    assert!(
                        band.1 < height as u32 / 2,
                        "stats must not blend the full screen"
                    );
                    assert_eq!(band.0 > height as u32 / 2, corner.bottom());
                }
            }
        }
    }
}

#[test]
fn notices_and_bottom_stats_share_their_painted_band() {
    let fonts = crate::theme::build_fonts().unwrap();
    let font = fonts.font(W::Regular, 14.0);
    for scale in [0.5, 1.0, 4.0] {
        let mut surface = skia_safe::surfaces::raster_n32_premul((1920, 1080)).unwrap();
        surface.canvas().clear(skia_safe::Color::TRANSPARENT);
        let canvas = surface.canvas();
        let mut damage = DamageBand::new(1080, false);
        damage.include(draw_osd_panel(
            canvas,
            &font,
            &stats(),
            (1920, 1080),
            scale,
            HudCorner::BottomRight,
        ));
        for (row, text) in ["Microphone muted", "Audio muted", "Access: 25 minutes"]
            .into_iter()
            .enumerate()
        {
            damage.include(corner_pill(
                canvas,
                &font,
                text,
                true,
                (1920, row, false),
                scale,
            ));
        }
        damage.include(draw_hint_pill(
            canvas,
            &font,
            exit_hint(true),
            1920,
            1080,
            1.0,
            scale,
        ));
        assert_pixels_in_band(&mut surface, damage.finish());
    }
}

#[test]
fn fullscreen_chrome_keeps_full_damage() {
    let mut damage = DamageBand::new(1080, true);
    damage.include(Rect::from_xywh(10.0, 10.0, 100.0, 100.0));
    assert_eq!(damage.finish(), (0, 1080));
}

#[test]
fn empty_or_offscreen_chrome_has_no_render_area() {
    assert_eq!(DamageBand::new(1080, false).finish().1, 0);
    for y in [-100.0, 1200.0] {
        let mut damage = DamageBand::new(1080, false);
        damage.include(Rect::from_xywh(0.0, y, 20.0, 20.0));
        assert_eq!(damage.finish().1, 0);
    }
}

#[test]
fn console_redraw_deadline_stops_idle_followups() {
    assert!(console_redraw_due(true, false, Some(Duration::ZERO)));
    assert!(console_redraw_due(true, true, None));
    assert!(!console_redraw_due(
        true,
        true,
        Some(IDLE_FRAME - Duration::from_nanos(1))
    ));
    assert!(console_redraw_due(true, true, Some(IDLE_FRAME)));
    // A freshly rendered idle frame resets its deadline, even with spare credit.
    assert!(!console_redraw_due(true, true, Some(Duration::ZERO)));
    assert!(!console_redraw_due(false, false, None));
}
#[test]
fn rpi_backdrop_override_is_bounded() {
    for value in [None, Some("0"), Some("NaN"), Some("193"), Some("-1")] {
        assert_eq!(super::rpi_field_edge(value), 192);
    }
    for edge in [64, 96, 128, 192] {
        assert_eq!(super::rpi_field_edge(Some(&edge.to_string())), edge);
    }
}
