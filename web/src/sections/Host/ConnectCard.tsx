import { Check, Copy, Smartphone } from "lucide-react";
import { type FC, useState } from "react";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/**
 * "Get a device onto this host" — the address to type, and the deep link that skips typing it.
 *
 * The console knew the host's identity and local address all along and never offered either in a
 * form you could hand to a phone: pairing meant reading an IP off the Host page and retyping it on
 * a couch. `punktfunk://connect/<unique_id>` is the shipped client grammar
 * (clients/shared/deeplink-vectors.json — the Rust, Swift and Kotlin parsers all test against it),
 * so a client that is already installed opens straight onto this host.
 *
 * No QR code: rendering one needs an encoder we do not bundle, and a wrong QR is worse than none.
 * The link is short enough to send over any chat app, which is what people actually do.
 */
export const ConnectCard: FC<{ host: HostInfo }> = ({ host }) => {
	const deepLink = `punktfunk://connect/${host.uniqueid}`;
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Smartphone className="size-4" />
					{m.connect_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="max-w-prose text-sm text-muted-foreground">
					{m.connect_help()}
				</p>
				<CopyRow label={m.connect_address()} value={host.local_ip} />
				<CopyRow label={m.connect_link()} value={deepLink} />
			</CardContent>
		</Card>
	);
};

/** One labelled, monospaced value with a copy button — the point of the card. */
const CopyRow: FC<{ label: string; value: string }> = ({ label, value }) => {
	const [copied, setCopied] = useState(false);
	const copy = async () => {
		try {
			await navigator.clipboard.writeText(value);
			setCopied(true);
			// Revert the affordance rather than leaving a permanent tick, which would stop reading as
			// feedback the second time you press it.
			setTimeout(() => setCopied(false), 1500);
		} catch {
			// Clipboard denied (insecure origin, or the user said no) — the value is on screen and
			// selectable, so there is nothing worth interrupting them about.
		}
	};
	return (
		<div className="space-y-1">
			<p className="text-xs text-muted-foreground">{label}</p>
			<div className="flex items-center gap-2">
				<code className="min-w-0 flex-1 truncate rounded-md bg-muted px-3 py-2 font-mono text-xs">
					{value}
				</code>
				<Button
					variant="outline"
					size="icon"
					aria-label={m.connect_copy()}
					onClick={copy}
				>
					{copied ? (
						<Check className="size-4 text-[var(--success)]" />
					) : (
						<Copy className="size-4" />
					)}
				</Button>
			</div>
		</div>
	);
};
