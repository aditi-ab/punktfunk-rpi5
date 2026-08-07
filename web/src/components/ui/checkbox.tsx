// The console's Checkbox IS @unom/ui's radix checkbox — shadcn-compatible tokens (border-input,
// data-checked:bg-primary), the tick drawn as an animated path, plus the shared toggle sound.
//
// It is `checked`/`onCheckedChange` (radix), NOT `checked`/`onChange` — a raw `<input
// type="checkbox">` swapped in here silently loses the brand entirely, which is exactly how two of
// them survived in the library forms.
export { Checkbox } from "@unom/ui/form/checkbox";
