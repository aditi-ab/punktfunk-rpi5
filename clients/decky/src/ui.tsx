// Shared UI primitives for the fullscreen page + modals. The one rule that keeps every row
// looking consistent: a Field's action(s) always sit right-aligned, with real space between
// them and the label text — never hugging it.
//
// Decky lays a Field out as `[ label .......... children ]`. When the children container is
// grown (`childrenContainerWidth="max"`, which we want so multi-button clusters have room), a
// bare `fit-content` button LEFT-aligns inside that grown container and ends up pressed against
// the label with the space wasted to its right. Wrapping the action(s) in `RowActions` pushes
// them to the right edge and evenly spaces multiples — the same treatment every row now gets.
import { Focusable } from "@decky/ui";
import { CSSProperties, FC, ReactNode } from "react";

export const RowActions: FC<{ children: ReactNode }> = ({ children }) => (
  <Focusable
    style={{
      display: "flex",
      gap: "0.5em",
      justifyContent: "flex-end",
      alignItems: "center",
    }}
  >
    {children}
  </Focusable>
);

// A single action button sized to its content (not the gamepad-UI default of 100% width), with
// a floor so short labels ("Pair", "Remove") don't render as tiny nubs and every row's button
// reads at the same weight.
export const actionButton: CSSProperties = {
  width: "fit-content",
  minWidth: "7em",
  flexShrink: 0,
};

// Square icon-only button (details ⓘ, header back arrow). Needs an explicit height or the zero
// padding collapses it to the icon's line height.
export const iconButton: CSSProperties = {
  width: "40px",
  minWidth: "40px",
  height: "40px",
  padding: 0,
  flexShrink: 0,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
};
