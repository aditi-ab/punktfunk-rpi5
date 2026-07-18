// Interactive PyroWave bitrate estimator for the docs. The formula mirrors the host's
// `resolve_bitrate_kbps_for` (crates/punktfunk-host/src/native.rs): the Automatic pin is
// ~1.6 bits/pixel for 4:2:0, ~2.6 bpp for 4:4:4, +15% for a 10-bit/HDR session, clamped to
// [0.5 Mbps, 8 Gbps]. Self-contained (no design-system imports) so it renders even where the
// full workspace UI packages are unavailable; themed via Fumadocs' `--color-fd-*` variables.
import { useState } from 'react'

type Preset = { label: string; w: number; h: number }

const RES_PRESETS: Preset[] = [
  { label: '1280 × 800 — Steam Deck', w: 1280, h: 800 },
  { label: '1920 × 1080 — 1080p', w: 1920, h: 1080 },
  { label: '2560 × 1440 — 1440p', w: 2560, h: 1440 },
  { label: '3440 × 1440 — ultrawide', w: 3440, h: 1440 },
  { label: '3840 × 2160 — 4K', w: 3840, h: 2160 },
  { label: '5120 × 1440 — super-ultrawide', w: 5120, h: 1440 },
]

const FPS_PRESETS = [30, 60, 90, 120, 144, 240]

// Practical payload ceilings (a bit under line rate — headers, FEC, framing).
const LINKS = [
  { label: 'Gigabit', mbps: 940 },
  { label: '2.5 GbE', mbps: 2350 },
  { label: '5 GbE', mbps: 4700 },
  { label: '10 GbE', mbps: 9400 },
]

const MIN_MBPS = 0.5
const MAX_MBPS = 8000

function pyrowaveMbps(
  w: number,
  h: number,
  fps: number,
  chroma444: boolean,
  hdr: boolean,
): number {
  if (!(w > 0) || !(h > 0) || !(fps > 0)) return 0
  const bppX10 = chroma444 ? 26 : 16
  let kbps = (w * h * fps * bppX10) / 10 / 1000
  if (hdr) kbps = (kbps * 115) / 100
  kbps = Math.min(Math.max(kbps, MIN_MBPS * 1000), MAX_MBPS * 1000)
  return kbps / 1000
}

const card: React.CSSProperties = {
  border: '1px solid var(--color-fd-border, #e5e7eb)',
  borderRadius: '0.75rem',
  background: 'var(--color-fd-card, transparent)',
  padding: '1.25rem',
  margin: '1.5rem 0',
}
const label: React.CSSProperties = {
  display: 'block',
  fontSize: '0.75rem',
  fontWeight: 600,
  letterSpacing: '0.02em',
  textTransform: 'uppercase',
  color: 'var(--color-fd-muted-foreground, #6b7280)',
  marginBottom: '0.35rem',
}
const field: React.CSSProperties = {
  width: '100%',
  padding: '0.45rem 0.6rem',
  borderRadius: '0.5rem',
  border: '1px solid var(--color-fd-border, #e5e7eb)',
  background: 'var(--color-fd-background, transparent)',
  color: 'var(--color-fd-foreground, inherit)',
  fontSize: '0.9rem',
}

function Toggle({
  active,
  onClick,
  children,
}: {
  active: boolean
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      style={{
        flex: 1,
        padding: '0.45rem 0.6rem',
        borderRadius: '0.5rem',
        border: '1px solid var(--color-fd-border, #e5e7eb)',
        cursor: 'pointer',
        fontSize: '0.9rem',
        fontWeight: 600,
        background: active
          ? 'var(--color-fd-primary, #6c5bf3)'
          : 'var(--color-fd-background, transparent)',
        color: active
          ? 'var(--color-fd-primary-foreground, #fff)'
          : 'var(--color-fd-foreground, inherit)',
      }}
    >
      {children}
    </button>
  )
}

export default function BitrateCalculator() {
  const [presetIdx, setPresetIdx] = useState(1) // 1080p
  const [custom, setCustom] = useState(false)
  const [cw, setCw] = useState(1920)
  const [ch, setCh] = useState(1080)
  const [fps, setFps] = useState(60)
  const [chroma444, setChroma444] = useState(false)
  const [hdr, setHdr] = useState(false)

  const preset = RES_PRESETS[presetIdx] ?? RES_PRESETS[1]!
  const w = custom ? cw : preset.w
  const h = custom ? ch : preset.h

  const mbps = pyrowaveMbps(w, h, fps, chroma444, hdr)
  const gbps = mbps / 1000
  const bpp = w > 0 && h > 0 && fps > 0 ? (mbps * 1e6) / (w * h * fps) : 0
  const frameKB = fps > 0 ? (mbps * 1e6) / 8 / fps / 1024 : 0
  const needed = LINKS.find((l) => l.mbps >= mbps)

  const big =
    mbps >= 1000 ? `${gbps.toFixed(2)} Gbps` : `${Math.round(mbps)} Mbps`

  return (
    <div style={card}>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))',
          gap: '0.9rem',
        }}
      >
        <div style={{ gridColumn: '1 / -1' }}>
          <label style={label}>Resolution</label>
          <select
            style={field}
            value={custom ? 'custom' : String(presetIdx)}
            onChange={(e) => {
              if (e.target.value === 'custom') {
                setCustom(true)
              } else {
                setCustom(false)
                setPresetIdx(Number(e.target.value))
              }
            }}
          >
            {RES_PRESETS.map((p, i) => (
              <option key={p.label} value={i}>
                {p.label}
              </option>
            ))}
            <option value="custom">Custom…</option>
          </select>
        </div>

        {custom && (
          <>
            <div>
              <label style={label}>Width</label>
              <input
                style={field}
                type="number"
                min={128}
                value={cw}
                onChange={(e) => setCw(Math.max(0, Number(e.target.value)))}
              />
            </div>
            <div>
              <label style={label}>Height</label>
              <input
                style={field}
                type="number"
                min={128}
                value={ch}
                onChange={(e) => setCh(Math.max(0, Number(e.target.value)))}
              />
            </div>
          </>
        )}

        <div>
          <label style={label}>Frame rate</label>
          <select
            style={field}
            value={fps}
            onChange={(e) => setFps(Number(e.target.value))}
          >
            {FPS_PRESETS.map((f) => (
              <option key={f} value={f}>
                {f} fps
              </option>
            ))}
          </select>
        </div>

        <div>
          <label style={label}>Chroma</label>
          <div style={{ display: 'flex', gap: '0.4rem' }}>
            <Toggle active={!chroma444} onClick={() => setChroma444(false)}>
              4:2:0
            </Toggle>
            <Toggle active={chroma444} onClick={() => setChroma444(true)}>
              4:4:4
            </Toggle>
          </div>
        </div>

        <div>
          <label style={label}>Dynamic range</label>
          <div style={{ display: 'flex', gap: '0.4rem' }}>
            <Toggle active={!hdr} onClick={() => setHdr(false)}>
              SDR (8-bit)
            </Toggle>
            <Toggle active={hdr} onClick={() => setHdr(true)}>
              HDR (10-bit)
            </Toggle>
          </div>
        </div>
      </div>

      <div
        style={{
          marginTop: '1.1rem',
          paddingTop: '1.1rem',
          borderTop: '1px solid var(--color-fd-border, #e5e7eb)',
          display: 'flex',
          flexWrap: 'wrap',
          alignItems: 'baseline',
          gap: '0.4rem 1.4rem',
        }}
      >
        <div
          style={{
            fontSize: '2rem',
            fontWeight: 700,
            color: 'var(--color-fd-primary, #6c5bf3)',
            lineHeight: 1.1,
          }}
        >
          ≈ {big}
        </div>
        <div
          style={{
            fontSize: '0.85rem',
            color: 'var(--color-fd-muted-foreground, #6b7280)',
          }}
        >
          {bpp.toFixed(2)} bits/pixel · {Math.round(frameKB)} KB per frame ·{' '}
          {needed ? `needs ${needed.label}` : 'over 10 GbE — lower the mode'}
        </div>
      </div>

      <div style={{ marginTop: '0.9rem', display: 'grid', gap: '0.4rem' }}>
        {LINKS.map((l) => {
          const fits = l.mbps >= mbps
          return (
            <div
              key={l.label}
              style={{ display: 'flex', alignItems: 'center', gap: '0.6rem' }}
            >
              <span
                style={{
                  width: '4.5rem',
                  fontSize: '0.8rem',
                  fontWeight: 600,
                  color: 'var(--color-fd-muted-foreground, #6b7280)',
                }}
              >
                {l.label}
              </span>
              <div
                style={{
                  flex: 1,
                  height: '0.5rem',
                  borderRadius: '999px',
                  background: 'var(--color-fd-muted, #eef0f4)',
                  overflow: 'hidden',
                }}
              >
                <div
                  style={{
                    width: `${Math.min(100, (mbps / l.mbps) * 100)}%`,
                    height: '100%',
                    background: fits
                      ? 'var(--color-fd-primary, #6c5bf3)'
                      : '#e5484d',
                  }}
                />
              </div>
              <span
                style={{
                  width: '2.5rem',
                  textAlign: 'right',
                  fontSize: '0.75rem',
                  color: fits
                    ? 'var(--color-fd-muted-foreground, #6b7280)'
                    : '#e5484d',
                }}
              >
                {Math.round((mbps / l.mbps) * 100)}%
              </span>
            </div>
          )
        })}
      </div>

      <p
        style={{
          marginTop: '0.9rem',
          marginBottom: 0,
          fontSize: '0.75rem',
          color: 'var(--color-fd-muted-foreground, #6b7280)',
        }}
      >
        Estimate of the Automatic bitrate a PyroWave session pins for this mode. Link bars use a
        practical payload ceiling (below line rate). The pin is capped at 8 Gbps; on a constrained
        link a host can cap it lower with <code>PUNKTFUNK_PYROWAVE_MAX_MBPS</code>.
      </p>
    </div>
  )
}
