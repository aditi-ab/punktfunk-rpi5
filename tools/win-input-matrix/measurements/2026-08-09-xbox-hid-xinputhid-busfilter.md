# `xinputhid` as a BUS FILTER promotes our HID Xbox pad — 2026-08-09, `.173`

Three arms, same box, same session, ~15 minutes apart, all with `win-input-matrix --watch`.
Box: `.173`, Win11 26200. Pad stood up with
`punktfunk-host.exe dualsense-windows-test --xboxhid` (the shipping `SwDeviceCreate` path, so a real
`045E:0B13` identity — **not** a `devgen` node, which has no PID token).

Devnodes involved:

| role | instance |
|---|---|
| parent / transport | `SWD\PUNKTFUNK\PF_XBOX_0` — `Service=MsHidUmdf`, software key `{745a17a0-…}\0072` |
| HID child | `HID\PUNKTFUNK\1&1F9456C7&3&0000` — software key `…\0073` |

## The result

| | **baseline** (no virtual pad) | **A — control** (pad up, no change) | **B — filter ONLY** | **C — filter + `DevicePropertyFlags=1`** |
|---|---|---|---|---|
| `IG_` token on the child | — | ❌ `HID\PUNKTFUNK\…` | ❌ `HID\PUNKTFUNK\…` | ✅ **`HID\PUNKTFUNK&IG_00\…`** |
| XUSB interface registered | none | ❌ none | ❌ none | ✅ **`\\?\hid#punktfunk&ig_00#…#{ec87f1e3…}`** |
| classic XInput | all 4 slots `1167` | ❌ all `1167` | ❌ all `1167` | ⚠️ **slot 0 `rc=0`** — admitted, data wrong |
| WGI `Gamepad` | 1 (PS5 only) | ❌ absent | ❌ absent | ⚠️ **present**, `ts=0` MUTE |
| WGI `RawGameController` | 2 | ✅ **LIVE** `[045E:0B13]` | ✅ LIVE | 🛑 **MUTE** (regressed) |
| HID class (Steam/SDL/DirectInput) | — | ✅ | ✅ | ✅ |

**Arm C is the first time the HID backend has ever reached classic XInput or WGI `Gamepad` at all.**

## What the A/B proves

Arm B is arm C minus one registry value. Removing **only** `DevicePropertyFlags` reverts *all three*
structural wins at once — the `IG_` token, the XUSB interface and XInput admission. Restoring it
brings them all back.

⇒ **`DevicePropertyFlags = 1` (`BusDevice`) is the decisive ingredient, and `UpperFilters` alone does
nothing.** Microsoft's own comment in `xinputhid.inf` says exactly this and we had read past it:
`BusDevice = 0x1` — *"we're a focused bus filter driver **for the IG_ problem**"*.

This retro-explains the earlier "🛑 MEASURED REGRESSION — never ship it" result, where the filter was
installed and the device came up `CM_PROB_NONE` while "no XUSB interface [was] registered". That was
arm B. The filter was loading and then sitting inert because nothing had put it in bus-filter mode.

⚠️ Placement matters and is easy to get wrong, because the two values live in **different keys** —
exactly as `xinputhid.inf` writes them:
* `UpperFilters` (REG_MULTI_SZ) → the **hardware/instance** key, `…\Enum\SWD\PUNKTFUNK\PF_XBOX_0`
  (an INF `[X.HW]` section);
* `DevicePropertyFlags` (REG_DWORD) → the **software/driver** key,
  `…\Control\Class\{745a17a0-…}\0072` (an INF `[X]` DDInstall section).

Both go on the **PARENT**, not on the HID child. Confirmed against the real Elite, whose BTLE
transport node carries `InfSection=Btle_Bus`, `DevicePropertyFlags=1`, `ConfigFlags=1` while its HID
child carries plain `input.inf`/`HID_Raw_Inst.NT` and no filter at all.

## What is still broken, and the evidence pointing at why

Everything **enumerates**; nothing **translates**.

* XInput slot 0 reads `packet=34 buttons=0x0000 LT=0 RT=0 LX=1024 LY=0 RX=0 RY=-1`. The devtest
  sweeps LX across ±32700 — `LX=1024, RY=-1` is not that sweep, it is a misparse.
* WGI `Gamepad` lists our pad with `ts=0` for every sample.
* Our `RawGameController` entry went from LIVE to MUTE: `xinputhid` claims the HID collection
  exclusively, so the reports that used to reach WGI Raw now go into a translator that drops them.
  (A real connected Elite behaves the same way — it yields nothing to a user-mode HID reader.)
* In arm C a **second** `[045E:0B13]` entry appears with a different shape, `buttons=14 switches=0`
  against our descriptor's `buttons=15 switches=1`. That is `xinputhid`'s synthesized view, and its
  shape does not match what we declare.

⇒ **The blocker is now the report descriptor, which is WP-A's subject.** `xinputhid` is translating
a HID report into XUSB and expects the real Xbox layout. Ours differs in exactly the ways
`tools/hid-descriptor-dump` measured against a real Elite: we number our input report (the real pad
does not), we carry two Simulation-page trigger axes (the real pad carries one combined `Z`), and we
put 15 buttons *after* the hat (the real pad puts 16 *before* it).

**Next experiment:** rebuild `pf-gamepad` with the captured layout and re-run arm C. That is the
one change that would confirm or kill the descriptor theory, and it is gated on the
descriptor-vs-sealed-channel decision in `design/xbox-pad-windows-handoff.md` §3.3 — the real pad
declares no Feature report, and `0x85` is our channel-proof transport.

## Reproducing

```powershell
# baseline FIRST, with no virtual pad — a real Xbox pad owns XInput slot 0 and will fake a pass
win-input-matrix --watch 8

Start-Process punktfunk-host.exe -ArgumentList 'dualsense-windows-test','--xboxhid','--seconds','90'
# arm C:
New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Enum\SWD\PUNKTFUNK\PF_XBOX_0' `
  -Name UpperFilters -PropertyType MultiString -Value @('xinputhid') -Force
New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{745a17a0-74d3-11d0-b6fe-00a0c90f57da}\0072' `
  -Name DevicePropertyFlags -PropertyType DWord -Value 1 -Force
# restart the devtest so PnP rebuilds the stack, then measure again
```

⚠️ The software-key index (`\0072`) is assigned at install and will differ on another box — read it
from the parent's `Driver` value, do not hardcode it.

⚠️ Everything above was applied **by hand to a live devnode**. Shipping it means an `AddReg` in
`pf_gamepad.inx` (`[pfGamepad.NT.hw]` for `UpperFilters`, `[pfGamepad.NT]` for
`DevicePropertyFlags`) — and that INF today contains no `AddReg` of any kind.

All changes on `.173` were reverted: registry values removed, devnodes removed, `oem100.inf`
deleted, both certs delstored, the 6 pre-existing `pf_gamepad` packages and the production service
left untouched.
