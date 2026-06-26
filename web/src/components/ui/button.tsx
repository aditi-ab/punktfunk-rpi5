import { AnimatedButton, buttonVariants } from "@unom/ui/button";
import type { ComponentProps } from "react";

// The console's Button IS @unom/ui's animated button — pill shape, specular
// material gloss + UI click/hover sounds (enabled via UnomProviders), driven by
// the shared brand tokens. Same variant/size vocabulary the routes already use
// (default/destructive/outline/secondary/ghost/link + default/sm/lg/icon).
export type ButtonProps = ComponentProps<typeof AnimatedButton>;

export const Button = AnimatedButton;

export { buttonVariants };
