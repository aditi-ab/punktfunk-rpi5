// The same table as the host's `knock_sources_classify_by_address`, so the console and pairing
// never disagree about which peers are local.
import { describe, expect, it } from "bun:test";
import {
	isLocalPeer,
	isTailscaleInterface,
	routesOverTailnet,
} from "./peer-scope.mjs";

const tailnet = async () => true;
const uplink = async () => false;

describe("isLocalPeer", () => {
	it("admits this machine, the LAN and a tailnet", async () => {
		for (const ip of [
			"127.0.0.1",
			"10.1.2.3",
			"172.16.0.1",
			"192.168.1.44",
			"169.254.3.4",
			"::1",
			"0:0:0:0:0:0:0:1",
			"::ffff:192.168.1.44",
			"::ffff:c0a8:12c",
			"fd00::1",
			"FE80::1%eth0",
		]) {
			expect(await isLocalPeer(ip, uplink), ip).toBe(true);
		}
		expect(await isLocalPeer("100.96.0.7", tailnet)).toBe(true);
		expect(await isLocalPeer("::ffff:100.96.0.7", tailnet)).toBe(true);
	});

	it("refuses the internet and anything it cannot read", async () => {
		for (const ip of [
			"203.0.113.5",
			"8.8.8.8",
			"172.32.0.1",
			"100.128.0.1",
			"0.0.0.0",
			"2606:4700::1111",
			"::ffff:203.0.113.5",
			"::",
			"",
			"not-an-ip",
			undefined,
		]) {
			expect(await isLocalPeer(ip, tailnet), String(ip)).toBe(false);
		}
	});

	it("refuses a CGNAT neighbour that did not come over the tailnet", async () => {
		expect(await isLocalPeer("100.96.0.7", uplink)).toBe(false);
		expect(await isLocalPeer("::ffff:100.96.0.7", uplink)).toBe(false);
	});
});

describe("isTailscaleInterface", () => {
	it("names Tailscale's interface on each OS", () => {
		for (const name of ["tailscale0", "Tailscale", "utun4"]) {
			expect(isTailscaleInterface(name, "100.101.2.3"), name).toBe(true);
		}
	});

	it("never takes an uplink or another VPN for it", () => {
		for (const name of ["eth0", "en0", "ppp0", "Ethernet", "wlan0"]) {
			expect(isTailscaleInterface(name, "100.101.2.3"), name).toBe(false);
		}
		expect(isTailscaleInterface("utun4", "10.8.0.2")).toBe(false);
		expect(isTailscaleInterface("utun4", "fd7a:115c:a1e0::1")).toBe(false);
	});
});

describe("routesOverTailnet", () => {
	it("reads the route back to a peer, and loopback is not the tailnet", async () => {
		expect(await routesOverTailnet("127.0.0.1")).toBe(false);
	});
});
