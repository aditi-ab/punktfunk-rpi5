//! The decode order, once. [`ladder`] is the walk
//! [`Decoder::new`](crate::video::Decoder::new) makes for a stored preference, and
//! [`demotions`] reads where a failing rung goes off the same walk. Both are pure and
//! take the OS as a value, so the table in this file's tests pins Linux and Windows
//! on any host. The CPU rung follows every walk and is never listed.
//!
//! A vendor rule is one line here or one predicate it calls:
//! [`VulkanDecodeDevice::prefer_vulkan_first`], `vaapi_auto_ok`, `v4l2_auto_ok` and
//! [`usable_decode_ops`](crate::video::usable_decode_ops), which masks the ops before
//! the ladder sees them. `build_rung` asks `tests::scripted` first, so the test
//! walks a real `Decoder` with no decoder behind it.

use crate::video::{native_rung_admitted, native_vulkan_usable, NativeRung, VulkanDecodeDevice};
use crate::video_caps::{is_auto, v4l2_auto_ok, vaapi_auto_ok};

/// The OS a ladder is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Linux,
    Windows,
}

impl Os {
    /// The OS this build runs on.
    pub(crate) const HOST: Os = if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    };

    /// The rung of the OS's own decode API.
    fn platform_rung(self) -> NativeRung {
        match self {
            Os::Linux => NativeRung::Vaapi,
            Os::Windows => NativeRung::D3d11va,
        }
    }

    /// The rungs a stored preference can pin here.
    fn pins(self) -> &'static [NativeRung] {
        match self {
            Os::Linux => &[
                NativeRung::Vulkan,
                NativeRung::Vaapi,
                NativeRung::V4l2,
                #[cfg(all(target_os = "linux", feature = "rpi5-v4l2-request"))]
                NativeRung::V4l2Request,
            ],
            Os::Windows => &[NativeRung::Vulkan, NativeRung::D3d11va],
        }
    }
}

/// One step of the build walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Build this rung. A refusal informs and the walk goes on.
    Try(NativeRung),
    /// The preference names this rung: it goes first, past the vendor order and the
    /// presenter-import gates. A refusal warns.
    Pin(NativeRung),
    /// A rung the walk passes over, said when the walk gets there.
    Note(Note),
}

impl Step {
    /// The rung this step builds.
    pub(crate) fn rung(self) -> Option<NativeRung> {
        match self {
            Step::Try(rung) | Step::Pin(rung) => Some(rung),
            Step::Note(_) => None,
        }
    }
}

/// Why the walk passes a rung over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Note {
    /// `native-vulkan` on a device whose decode family lacks the codec.
    VulkanPinRefused,
    /// VAAPI's turn in the vendor order, yielded to a proven Vulkan rung this device runs.
    VaapiYields,
    /// VAAPI outside the presenter's `auto` gate.
    VaapiGated,
    /// A `software` preference: no hardware rung is tried.
    SoftwareOnly,
    /// A value that is no pin, `auto` or `software` here: it reads as `auto`.
    UnknownPreference,
}

/// The steps `Decoder::new` walks for `choice`, the resolved preference.
///
/// A pin goes first, and a refused pin walks on as `auto` without it. Any value but
/// a pin and `software` reads as `auto`. `auto` puts Vulkan Video first on NVIDIA and
/// AMD and the platform rung first elsewhere, unless that rung is unproven and a
/// proven Vulkan rung can run ([`native_rung_admitted`]). The platform rung and V4L2
/// need a presenter that imports their pictures.
pub(crate) fn ladder(os: Os, choice: &str, wire: u8, vk: Option<&VulkanDecodeDevice>) -> Vec<Step> {
    if choice == "software" {
        return vec![Step::Note(Note::SoftwareOnly)];
    }
    let video_decode = vk.is_some_and(|v| v.video_decode);
    let caps = vk.map_or(0, |v| v.decode_video_caps);
    let usable = native_vulkan_usable(wire, video_decode, caps);
    let mut steps = Vec::new();
    let pin = os.pins().iter().copied().find(|r| r.name() == choice);
    match pin {
        Some(NativeRung::Vulkan) if !usable => steps.push(Step::Note(Note::VulkanPinRefused)),
        Some(rung) => steps.push(Step::Pin(rung)),
        None if !is_auto(choice) => steps.push(Step::Note(Note::UnknownPreference)),
        None => {}
    }
    // A pin is its rung's one try: the walk after it never names that rung.
    let open = |rung| pin != Some(rung);
    let vulkan = (open(NativeRung::Vulkan) && usable).then_some(Step::Try(NativeRung::Vulkan));
    let platform_turn = !vk
        .filter(|v| v.video_decode)
        .is_some_and(|v| v.prefer_vulkan_first());
    match os {
        Os::Linux => {
            // Vulkan sits under VAAPI only where this device runs it.
            let below = usable.then_some(NativeRung::Vulkan);
            let leads = platform_turn && native_rung_admitted(NativeRung::Vaapi, wire, below);
            let vaapi = open(NativeRung::Vaapi).then(|| {
                if vaapi_auto_ok(vk) {
                    Step::Try(NativeRung::Vaapi)
                } else {
                    Step::Note(Note::VaapiGated)
                }
            });
            if platform_turn && !leads && vaapi.is_some() {
                steps.push(Step::Note(Note::VaapiYields));
            }
            if leads {
                steps.extend(vaapi);
                steps.extend(vulkan);
            } else {
                steps.extend(vulkan);
                steps.extend(vaapi);
            }
            // V4L2 last: the rung of devices that have neither of the above.
            if open(NativeRung::V4l2) && v4l2_auto_ok(vk) {
                steps.push(Step::Try(NativeRung::V4l2));
            }
        }
        Os::Windows => {
            // Intel advertises Vulkan Video too, so its caps alone would not keep it off
            // that rung. No rung is named below D3D11VA: it is admitted whatever its evidence.
            let leads = platform_turn && native_rung_admitted(NativeRung::D3d11va, wire, None);
            // A DXVA frame reaches the screen through the Vulkan presenter's win32 import, or
            // as a pool slice on the D3D11 presenter.
            let d3d11 = (open(NativeRung::D3d11va)
                && vk.is_some_and(|v| v.d3d11_import || v.d3d11_present))
            .then_some(Step::Try(NativeRung::D3d11va));
            if leads {
                steps.extend(d3d11);
                steps.extend(vulkan);
            } else {
                steps.extend(vulkan);
                steps.extend(d3d11);
            }
        }
    }
    steps
}

/// The hardware rungs a failing rung may demote onto, in order; the CPU rung
/// follows. The platform rung, then Vulkan, each where the `auto` walk offers it
/// whatever the session's preference was. `ran` holds the failing rung and every
/// one before it; none is entered again, so the two cannot bounce the session.
/// V4L2 is never a target.
pub(crate) fn demotions(
    os: Os,
    wire: u8,
    vk: Option<&VulkanDecodeDevice>,
    ran: &[NativeRung],
) -> Vec<NativeRung> {
    let offered: Vec<NativeRung> = ladder(os, "auto", wire, vk)
        .into_iter()
        .filter_map(Step::rung)
        .collect();
    [os.platform_rung(), NativeRung::Vulkan]
        .into_iter()
        .filter(|rung| offered.contains(rung) && !ran.contains(rung))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use anyhow::anyhow;
    use anyhow::Result;
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};

    use super::{demotions, ladder, Note, Os, Step};
    use crate::video::{
        DecodeHealth, DecodedImage, Decoder, NativeRung, NoSoftwareRung, StreamFormat,
        VulkanDecodeDevice, VIDEO_CODEC_OP_DECODE_AV1, VIDEO_CODEC_OP_DECODE_H264,
        VIDEO_CODEC_OP_DECODE_H265,
    };
    use crate::video_caps::tests::decode_device;
    use crate::video_caps::usable_decode_ops_on;
    use crate::video_rung::Rung;

    /// Columns: OS; GPU vendor (`none` is no presenter device); the codecs its Vulkan
    /// Video advertises (`all`, `h26x` without AV1, `none`); whether the presenter
    /// imports the platform rung's pictures (dma-buf, D3D11 texture), or `d3d` for the
    /// D3D11 presenter, which samples them and has no Vulkan Video; wire codec;
    /// stored preference (`empty` is `""`). Then the rungs tried in order, and for
    /// each hardware rung the session can land on, the rungs it demotes through. A
    /// target that does not build passes the session to the next rung of its chain.
    const ORDER: &str = "
        os      gpu    vulkan import codec pref           | tries                            | a failing rung demotes through
        linux   nvidia all    yes    h264  auto           | vulkan v4l2 software             | vulkan>software v4l2>vulkan>software
        linux   amd    all    yes    h264  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    hevc  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    h26x   yes    av1   auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   intel  all    yes    h264  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   other  all    yes    av1   auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  none   yes    h264  auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   nvidia none   yes    h264  auto           | v4l2 software                    | v4l2>software
        linux   amd    all    no     h264  auto           | vulkan software                  | vulkan>software
        linux   amd    none   no     h264  auto           | software                         | -
        linux   none   -      -      h264  auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   amd    all    yes    h264  hardware       | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  empty          | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  software       | software                         | -
        linux   amd    all    yes    h264  native-vulkan  | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   native-vulkan  | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   nvidia all    yes    h264  native-vaapi   | vaapi vulkan v4l2 software       | vaapi>vulkan>software vulkan>software v4l2>vulkan>software
        linux   amd    all    yes    h264  native-vaapi   | vaapi vulkan v4l2 software       | vaapi>vulkan>software vulkan>vaapi>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   native-vaapi   | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   amd    all    yes    h264  native-v4l2    | v4l2 vulkan vaapi software       | v4l2>vaapi>vulkan>software vulkan>vaapi>software vaapi>vulkan>software
        linux   amd    all    yes    h264  something-else | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  native-d3d11va | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        windows nvidia all    yes    h264  auto           | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    all    yes    h264  auto           | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    h26x   yes    av1   auto           | d3d11va software                 | d3d11va>software
        windows intel  all    yes    h264  auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows intel  all    yes    av1   auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows other  all    yes    h264  auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows nvidia none   yes    h264  auto           | d3d11va software                 | d3d11va>software
        windows nvidia all    no     h264  auto           | vulkan software                  | vulkan>software
        windows intel  all    no     h264  auto           | vulkan software                  | vulkan>software
        windows none   -      -      h264  auto           | software                         | -
        windows nvidia all    yes    h264  hardware       | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows nvidia all    yes    h264  software       | software                         | -
        windows intel  all    yes    h264  native-vulkan  | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    h26x   yes    av1   native-vulkan  | d3d11va software                 | d3d11va>software
        windows nvidia all    yes    h264  native-d3d11va | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows intel  all    yes    h264  native-d3d11va | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows nvidia all    no     h264  native-d3d11va | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>software
        windows nvidia all    yes    h264  something-else | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows nvidia all    yes    h264  native-vaapi   | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows qualcomm none d3d    h264  auto           | d3d11va software                 | d3d11va>software
        windows qualcomm none d3d    hevc  auto           | d3d11va software                 | d3d11va>software
        windows qualcomm none d3d    av1   auto           | d3d11va software                 | d3d11va>software
        windows nvidia all    d3d    h264  auto           | d3d11va software                 | d3d11va>software
    ";

    struct Row {
        os: &'static str,
        gpu: &'static str,
        vulkan: &'static str,
        import: &'static str,
        codec: &'static str,
        pref: &'static str,
        tries: &'static str,
        demotes: &'static str,
    }

    impl Row {
        /// One table line; the two result cells are empty when the line has only facts.
        fn parse(line: &'static str) -> Row {
            let mut cells = line.split('|').map(str::trim);
            let facts: Vec<&str> = cells.next().unwrap_or("").split_whitespace().collect();
            assert_eq!(facts.len(), 6, "{line}");
            Row {
                os: facts[0],
                gpu: facts[1],
                vulkan: facts[2],
                import: facts[3],
                codec: facts[4],
                pref: facts[5],
                tries: cells.next().unwrap_or(""),
                demotes: cells.next().unwrap_or(""),
            }
        }

        fn os(&self) -> Os {
            match self.os {
                "linux" => Os::Linux,
                "windows" => Os::Windows,
                other => panic!("os {other}"),
            }
        }

        fn wire(&self) -> u8 {
            match self.codec {
                "h264" => CODEC_H264,
                "hevc" => CODEC_HEVC,
                "av1" => CODEC_AV1,
                other => panic!("codec {other}"),
            }
        }

        fn pref(&self) -> &'static str {
            match self.pref {
                "empty" => "",
                other => other,
            }
        }

        /// The presenter's device as the ladder sees it: the advertised ops already
        /// masked, as the presenter masks them. The D3D11 presenter runs no Vulkan Video.
        fn device(&self) -> Option<VulkanDecodeDevice> {
            let vendor = match self.gpu {
                "nvidia" => 0x10DE,
                "amd" => 0x1002,
                "intel" => 0x8086,
                "qualcomm" => 0x5143,
                "other" => 0x5143,
                "none" => return None,
                other => panic!("gpu {other}"),
            };
            let h26x = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
            let ops = match self.vulkan {
                "all" => h26x | VIDEO_CODEC_OP_DECODE_AV1,
                "h26x" => h26x,
                "none" => 0,
                other => panic!("vulkan {other}"),
            };
            let d3d = self.import == "d3d";
            let ops = if d3d { 0 } else { ops };
            let mut vk = decode_device(vendor, self.gpu);
            vk.video_decode = ops != 0;
            vk.decode_video_caps = usable_decode_ops_on(self.os(), vendor, ops);
            vk.dmabuf_import = self.import == "yes" && self.os() == Os::Linux;
            vk.d3d11_import = self.import == "yes" && self.os() == Os::Windows;
            vk.d3d11_present = d3d && self.os() == Os::Windows;
            Some(vk)
        }

        fn label(&self) -> String {
            format!(
                "{} {} {} {} {} {}",
                self.os, self.gpu, self.vulkan, self.import, self.codec, self.pref
            )
        }
    }

    #[cfg(all(target_os = "linux", feature = "rpi5-v4l2-request"))]
    #[test]
    fn pi_request_pin_precedes_native_rungs_without_changing_auto() {
        let steps = ladder(Os::Linux, "v4l2-request", CODEC_HEVC, None);
        assert_eq!(steps.first(), Some(&Step::Pin(NativeRung::V4l2Request)));
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.rung() == Some(NativeRung::V4l2Request))
                .count(),
            1
        );
        assert!(steps.iter().any(|s| s.rung() == Some(NativeRung::V4l2)));
        for pref in ["auto", "software", "native-v4l2"] {
            assert!(ladder(Os::Linux, pref, CODEC_HEVC, None)
                .iter()
                .all(|s| s.rung() != Some(NativeRung::V4l2Request)));
        }
    }

    fn rows() -> Vec<Row> {
        let lines = ORDER.lines().map(str::trim).filter(|l| !l.is_empty());
        let rows: Vec<Row> = lines.skip(1).map(Row::parse).collect();
        assert!(rows.iter().all(|r| !r.demotes.is_empty()), "a short row");
        rows
    }

    /// Demotion chains as a table cell.
    fn chains_cell(chains: &[String]) -> String {
        if chains.is_empty() {
            "-".to_string()
        } else {
            chains.join(" ")
        }
    }

    /// Rungs as a table cell, the CPU rung last: it follows every walk.
    fn cell(rungs: &[NativeRung], between: &str) -> String {
        let hardware = rungs.iter().map(|r| r.name().trim_start_matches("native-"));
        let all: Vec<&str> = hardware.chain(["software"]).collect();
        all.join(between)
    }

    /// What `build_rung` does while a walk is scripted: it records every rung asked
    /// for, refuses the first `refuse`, and lands the rest on a [`Failing`] rung.
    struct Script {
        tried: Vec<NativeRung>,
        refuse: usize,
    }

    thread_local! {
        static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
    }

    /// `build_rung`'s answer under a script. `None` builds the real rung.
    pub(crate) fn scripted(rung: NativeRung) -> Option<Result<Box<dyn Rung>>> {
        SCRIPT.with_borrow_mut(|script| {
            let script = script.as_mut()?;
            script.tried.push(rung);
            if script.refuse > 0 {
                script.refuse -= 1;
                return Some(Err(anyhow!("scripted refusal")));
            }
            Some(Ok(Box::new(Failing(rung)) as Box<dyn Rung>))
        })
    }

    fn take_tried() -> Vec<NativeRung> {
        SCRIPT.with_borrow_mut(|s| std::mem::take(&mut s.as_mut().expect("scripted").tried))
    }

    /// A rung that refuses every access unit as a device fact, which demotes at once.
    struct Failing(NativeRung);

    impl Rung for Failing {
        fn id(&self) -> Option<NativeRung> {
            Some(self.0)
        }

        fn name(&self) -> &'static str {
            self.0.name()
        }

        fn decode(&mut self, _: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
            let refused = pf_vkdecode::CapsError::SliceSegments { segments: 2 };
            Err(pf_vkdecode::VkDecodeError::Caps(refused).into())
        }

        // The CPU rung reports none, so `Some` tells the two apart.
        fn health(&self) -> Option<DecodeHealth> {
            Some(DecodeHealth::default())
        }
    }

    /// `Decoder::new` for `row`, with the first `refuse` hardware builds refused.
    fn start(row: &Row, refuse: usize) -> (Result<Decoder>, Vec<NativeRung>) {
        SCRIPT.set(Some(Script {
            tried: Vec::new(),
            refuse,
        }));
        let vk = row.device();
        let decoder = Decoder::new(row.wire(), row.pref(), vk.as_ref(), StreamFormat::SDR_420_8);
        (decoder, take_tried())
    }

    /// The walk ended on the CPU rung: it runs, or refused a codec it has no decoder for.
    fn on_cpu_rung(row: &Row, landed: std::result::Result<&Decoder, &anyhow::Error>) -> bool {
        match landed {
            Ok(d) => d.decode_health().is_none(),
            Err(e) => row.codec == "hevc" && e.downcast_ref::<NoSoftwareRung>().is_some(),
        }
    }

    /// Land on the `step`th rung `row` tries, then fail every access unit: the rungs
    /// the session demotes through. `refuse_targets` refuses every demotion build.
    fn demotes(row: &Row, step: usize, refuse_targets: bool) -> Vec<NativeRung> {
        let (decoder, tried) = start(row, step);
        let mut decoder = decoder.expect("the scripted rung lands");
        let mut chain = vec![*tried.last().expect("a rung landed")];
        if refuse_targets {
            SCRIPT.with_borrow_mut(|s| s.as_mut().expect("scripted").refuse = usize::MAX);
        }
        loop {
            let unit = decoder.decode(&[]);
            let built = take_tried();
            if built.is_empty() || refuse_targets {
                let landed = unit.as_ref().map(|_| &decoder);
                assert!(on_cpu_rung(row, landed), "{}", row.label());
                chain.extend(built);
                return chain;
            }
            assert_eq!(built.len(), 1, "{}: one target lands", row.label());
            chain.extend(built);
        }
    }

    /// The table holds for the walk this build runs: what `Decoder::new` tries, and
    /// where each rung it can land on goes when it keeps failing.
    #[test]
    fn the_decoder_walks_the_order_table() {
        // `Decoder::new` reads the override first: a set one answers for every row.
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let table = rows();
        let here: Vec<&Row> = table.iter().filter(|r| r.os() == Os::HOST).collect();
        assert!(here.len() > 10, "the table lost this OS's rows");
        for row in here {
            let (landed, tried) = start(row, usize::MAX);
            assert!(on_cpu_rung(row, landed.as_ref()), "{}", row.label());
            assert_eq!(cell(&tried, " "), row.tries, "{}", row.label());

            let mut chains = Vec::new();
            for step in 0..tried.len() {
                let chain = demotes(row, step, false);
                // A target that does not build passes the session to the next one.
                assert_eq!(demotes(row, step, true), chain, "{}", row.label());
                chains.push(cell(&chain, ">"));
            }
            assert_eq!(chains_cell(&chains), row.demotes, "{}", row.label());
        }
    }

    /// The table is the ladder's, on every OS: what [`ladder`] lists, and where
    /// [`demotions`] sends each rung as the session works down.
    #[test]
    fn the_ladder_is_the_order_table() {
        for row in rows() {
            let (os, wire, vk) = (row.os(), row.wire(), row.device());
            let steps = ladder(os, row.pref(), wire, vk.as_ref());
            let tried: Vec<NativeRung> = steps.into_iter().filter_map(Step::rung).collect();
            assert_eq!(cell(&tried, " "), row.tries, "{}", row.label());

            let mut chains = Vec::new();
            for rung in &tried {
                // The rungs run so far are the chain itself.
                let mut chain = vec![*rung];
                while let Some(&next) = demotions(os, wire, vk.as_ref(), &chain).first() {
                    chain.push(next);
                }
                // A target that does not build passes the session to the next one.
                let refused = demotions(os, wire, vk.as_ref(), &[*rung]);
                assert_eq!(refused, chain[1..], "{}", row.label());
                chains.push(cell(&chain, ">"));
            }
            assert_eq!(chains_cell(&chains), row.demotes, "{}", row.label());
        }
    }

    /// A rung whose pictures the presenter can't show demotes as a failing decode does:
    /// onto Vulkan Video, which imports nothing, then the CPU rung. A new format's walk
    /// passes over every rung on a refused lane, V4L2's dma-bufs with VAAPI's.
    #[test]
    fn a_presenter_refusal_demotes_onto_a_rung_it_can_show() {
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let (facts, platform) = match Os::HOST {
            Os::Linux => ("linux amd all yes hevc native-vaapi", NativeRung::Vaapi),
            Os::Windows => (
                "windows nvidia all yes hevc native-d3d11va",
                NativeRung::D3d11va,
            ),
        };
        let (decoder, tried) = start(&Row::parse(facts), 0);
        let mut d = decoder.expect("the scripted rung lands");
        assert_eq!(tried, [platform]);
        d.demote_for_presenter().expect("Vulkan Video lands");
        assert_eq!(take_tried(), [NativeRung::Vulkan]);
        let e = d.demote_for_presenter().expect_err("HEVC has no CPU rung");
        assert!(e.downcast_ref::<NoSoftwareRung>().is_some(), "{e:#}");
        assert!(take_tried().is_empty(), "no refused rung is built again");
        d.reformat(CODEC_H264, StreamFormat::SDR_420_8)
            .expect("H.264 has a CPU rung");
        assert!(
            take_tried().is_empty(),
            "the walk passes over every refused rung"
        );
        assert!(d.decode_health().is_none(), "the CPU rung decodes");
    }

    /// A pin goes first, warns, and is its rung's one try; a rung the walk passes over
    /// is said in its turn.
    #[test]
    fn the_walk_says_what_it_passes_over() {
        use NativeRung::{D3d11va, V4l2, Vaapi, Vulkan};
        use Step::{Pin, Try};
        let walk = |facts: &'static str| {
            let row = Row::parse(facts);
            ladder(row.os(), row.pref(), row.wire(), row.device().as_ref())
        };
        assert_eq!(
            walk("linux intel all yes h264 auto"),
            [
                Step::Note(Note::VaapiYields),
                Try(Vulkan),
                Try(Vaapi),
                Try(V4l2)
            ]
        );
        assert_eq!(
            walk("linux nvidia all yes h264 auto"),
            [Try(Vulkan), Step::Note(Note::VaapiGated), Try(V4l2)]
        );
        assert_eq!(
            walk("linux intel all yes av1 native-vulkan"),
            [Step::Note(Note::VulkanPinRefused), Try(Vaapi), Try(V4l2)]
        );
        assert_eq!(
            walk("linux intel all yes h264 native-vaapi"),
            [Pin(Vaapi), Try(Vulkan), Try(V4l2)]
        );
        assert_eq!(
            walk("windows intel all yes h264 native-d3d11va"),
            [Pin(D3d11va), Try(Vulkan)]
        );
        assert_eq!(
            walk("windows nvidia all yes h264 software"),
            [Step::Note(Note::SoftwareOnly)]
        );
        assert_eq!(
            walk("linux amd all yes h264 something-else")[0],
            Step::Note(Note::UnknownPreference)
        );
    }
}
