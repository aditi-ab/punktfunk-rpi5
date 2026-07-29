// The host row's OS mark, resolved from the host's OS-identity chain (mDNS `os` TXT /
// `--list-hosts` `os`, e.g. "linux/fedora/bazzite"): walk the chain most-specific-first and
// take the first token react-icons has a brand mark for, so an unknown distro degrades to its
// family's mark and finally to Tux. Mirrors pf-client-core's `os_icon_tokens` (aliases
// macos→apple, steamos→steam); null when the chain is absent or entirely unknown — the row
// then renders exactly as it did before the field existed.
import { FC } from "react";
import {
  FaApple,
  FaFedora,
  FaLinux,
  FaSteam,
  FaSuse,
  FaUbuntu,
  FaWindows,
} from "react-icons/fa";
import { SiArchlinux, SiDebian, SiNixos } from "react-icons/si";
import { IconType } from "react-icons";

const OS_ICONS: Record<string, IconType> = {
  windows: FaWindows,
  apple: FaApple,
  macos: FaApple,
  linux: FaLinux,
  steam: FaSteam,
  steamos: FaSteam,
  ubuntu: FaUbuntu,
  fedora: FaFedora,
  opensuse: FaSuse,
  arch: SiArchlinux,
  debian: SiDebian,
  nixos: SiNixos,
};

export function resolveOsIcon(os: string | undefined): IconType | null {
  for (const token of (os ?? "").toLowerCase().split("/").reverse()) {
    const icon = OS_ICONS[token];
    if (icon) {
      return icon;
    }
  }
  return null;
}

/** The mark itself, or nothing — sized/colored by the surrounding text like the lock glyph. */
export const OsMark: FC<{ os?: string }> = ({ os }) => {
  const Icon = resolveOsIcon(os);
  return Icon ? <Icon /> : null;
};
