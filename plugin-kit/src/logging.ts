// Runner-journal logging: one stamped line per log call, `<ISO> [<plugin>] <message>`,
// matching the format the scripting runner journals (and what the previous hand-rolled
// plugin loggers emitted), so kit-based plugins read consistently in
// `journalctl --user -u punktfunk-scripting`.
import { Cause, Layer, Logger } from "effect";

const render = (message: unknown): string => {
	if (typeof message === "string") return message;
	if (Array.isArray(message)) return message.map(render).join(" ");
	return typeof message === "object" ? JSON.stringify(message) : String(message);
};

/** Replace the default logger with the runner-journal format. */
export const loggingLayer = (plugin: string): Layer.Layer<never> =>
	Logger.layer([
		Logger.make(({ message, logLevel, cause, date }) => {
			const level = logLevel === "Info" ? "" : ` ${logLevel.toUpperCase()}:`;
			const causeSuffix =
				cause.reasons.length > 0 ? ` ${Cause.pretty(cause)}` : "";
			console.log(
				`${date.toISOString()} [${plugin}]${level} ${render(message)}${causeSuffix}`,
			);
		}),
	]);
