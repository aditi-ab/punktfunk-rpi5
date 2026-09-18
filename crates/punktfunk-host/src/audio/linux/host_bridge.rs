//! What the operator hears while a session runs, next to the stream sink.
//!
//! Both jobs key on `host`: the output the claim found elected before it
//! pointed the default at the stream sink ([`super::stream_sink::host_sink`]).
//!
//! * Playthrough (`audio.output_mode = host_and_client`): link the stream
//!   sink's monitor ports to that output, channel by channel, so the host
//!   plays what the clients hear. The links belong to this connection and
//!   die with it.
//! * Voice chat on the host (`PUNKTFUNK_AUDIO_VOICE_CHAT=host`): pin voice
//!   apps' output streams to that output (`target.object` on the `default`
//!   metadata, as `pactl move-sink-input` does) so the clients never hear
//!   their own voices back. Cleared at session end; the app follows the
//!   default again.
//!
//! Nodes, ports, the metadata object and the host name all arrive on their
//! own schedule; every event ends in [`HostBridge::sync`], which does what it
//! can with what is known.

use pipewire as pw;
use pw::proxy::ProxyT;
use std::collections::{HashMap, HashSet};

struct NodeInfo {
    name: String,
    serial: Option<String>,
    /// An output stream of a voice-chat app.
    voice: bool,
}

struct PortInfo {
    node: u32,
    output: bool,
    monitor: bool,
    channel: String,
}

pub(super) struct HostBridge {
    playthrough: bool,
    voice: bool,
    voice_apps: Vec<String>,
    /// Our stream sink's `node.name`.
    sink: String,
    host: Option<String>,
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,
    /// `(monitor port, output port)` pairs holding a live link.
    links: Vec<(u32, u32, pw::link::Link, pw::proxy::ProxyListener)>,
    /// Voice streams pinned to `host`, by node id.
    routed: HashSet<u32>,
    metadata: Option<pw::metadata::Metadata>,
}

impl HostBridge {
    /// `playthrough` / `voice`: whether this topology can do each job at all;
    /// the operator's settings decide whether it does.
    pub(super) fn new(sink: &str, playthrough: bool, voice: bool) -> HostBridge {
        let cfg = pf_host_config::config();
        HostBridge {
            playthrough: playthrough && cfg.audio_output_mode.prefers_host_hardware(),
            voice: voice && cfg.audio_voice_chat == pf_host_config::VoiceChatRoute::Host,
            voice_apps: cfg.audio_voice_apps.clone(),
            sink: sink.to_owned(),
            host: None,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            links: Vec::new(),
            routed: HashSet::new(),
            metadata: None,
        }
    }

    pub(super) fn node_name(&self, id: u32) -> Option<&str> {
        self.nodes.get(&id).map(|n| n.name.as_str())
    }

    /// Whether `name` is the output the playthrough links feed — the node
    /// that then clocks the capture group, by design.
    pub(super) fn is_host(&self, name: &str) -> bool {
        self.playthrough && !self.links.is_empty() && self.host.as_deref() == Some(name)
    }

    /// Record a registry global. Binds the `default` metadata once.
    pub(super) fn on_global(
        &mut self,
        global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
        registry: &pw::registry::RegistryRc,
    ) {
        let Some(props) = global.props else { return };
        match global.type_ {
            pw::types::ObjectType::Node => {
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let voice = props.get("media.class") == Some("Stream/Output/Audio")
                    && pf_host_config::voice_app_matches(
                        [
                            props.get("application.name"),
                            props.get("application.process.binary"),
                        ]
                        .into_iter()
                        .flatten(),
                        &self.voice_apps,
                    );
                self.nodes.insert(
                    global.id,
                    NodeInfo {
                        name: name.to_owned(),
                        serial: props.get("object.serial").map(str::to_owned),
                        voice,
                    },
                );
            }
            pw::types::ObjectType::Port => {
                let Some(node) = props.get("node.id").and_then(|v| v.parse().ok()) else {
                    return;
                };
                self.ports.insert(
                    global.id,
                    PortInfo {
                        node,
                        output: props.get("port.direction") == Some("out"),
                        monitor: props.get("port.monitor") == Some("true"),
                        channel: props.get("audio.channel").unwrap_or("").to_owned(),
                    },
                );
            }
            pw::types::ObjectType::Metadata
                if self.metadata.is_none() && props.get("metadata.name") == Some("default") =>
            {
                self.metadata = registry.bind::<pw::metadata::Metadata, _>(global).ok();
            }
            _ => {}
        }
    }

    pub(super) fn on_remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.ports.remove(&id);
        self.routed.remove(&id);
        self.links.retain(|(out, inp, ..)| *out != id && *inp != id);
    }

    /// The claim reports the host output after every claim. A change drops the
    /// old links and pins; the next [`sync`](Self::sync) rebuilds them.
    pub(super) fn set_host(&mut self, host: Option<String>) {
        if self.host == host {
            return;
        }
        self.links.clear();
        self.routed.clear();
        self.host = host;
    }

    pub(super) fn sync(&mut self, core: &pw::core::CoreRc) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let Some((host_id, serial)) = self
            .nodes
            .iter()
            .find(|(_, n)| n.name == host)
            .map(|(id, n)| (*id, n.serial.clone()))
        else {
            return;
        };
        if self.playthrough {
            self.link(core, host_id);
        }
        if self.voice {
            self.pin(host_id, serial.as_deref());
        }
    }

    fn link(&mut self, core: &pw::core::CoreRc, host_id: u32) {
        let Some(sink_id) = self
            .nodes
            .iter()
            .find(|(_, n)| n.name == self.sink)
            .map(|(id, _)| *id)
        else {
            return;
        };
        let mut pairs = Vec::new();
        for (out_id, out) in &self.ports {
            if out.node != sink_id || !out.output || !out.monitor {
                continue;
            }
            // Same-channel links only: a 5.1 sink on stereo speakers loses C/LFE/rear here.
            let Some(in_id) = self
                .ports
                .iter()
                .find(|(_, p)| p.node == host_id && !p.output && p.channel == out.channel)
                .map(|(id, _)| *id)
            else {
                continue;
            };
            if self
                .links
                .iter()
                .any(|(o, i, ..)| *o == *out_id && *i == in_id)
            {
                continue;
            }
            pairs.push((*out_id, in_id, out.channel.clone()));
        }
        let first = self.links.is_empty() && !pairs.is_empty();
        for (out_id, in_id, channel) in pairs {
            let mut props = pw::properties::PropertiesBox::new();
            props.insert("link.output.node", sink_id.to_string());
            props.insert("link.output.port", out_id.to_string());
            props.insert("link.input.node", host_id.to_string());
            props.insert("link.input.port", in_id.to_string());
            match core.create_object::<pw::link::Link>("link-factory", &props) {
                Ok(link) => {
                    let listener = link
                        .upcast_ref()
                        .add_listener_local()
                        .error(move |_seq, res, message| {
                            tracing::warn!(
                                channel,
                                res,
                                message,
                                "host playthrough link refused — the host stays silent on \
                                 this channel"
                            );
                        })
                        .register();
                    self.links.push((out_id, in_id, link, listener));
                }
                Err(e) => tracing::warn!(error = %e, "host playthrough link not created"),
            }
        }
        if first {
            tracing::info!(
                host = self.host.as_deref().unwrap_or(""),
                channels = self.links.len(),
                "host playthrough linked — the host plays what the clients hear"
            );
        }
    }

    fn pin(&mut self, host_id: u32, serial: Option<&str>) {
        let Some(md) = &self.metadata else { return };
        let host_name = self.nodes[&host_id].name.as_str();
        // pipewire-pulse pins by `object.serial`; a name is what older WirePlumber matched.
        let (type_, value) = match serial {
            Some(s) => (Some("Spa:Id"), s),
            None => (None, host_name),
        };
        for (id, node) in &self.nodes {
            if !node.voice || self.routed.contains(id) {
                continue;
            }
            md.set_property(*id, "target.object", type_, Some(value));
            tracing::info!(
                app = node.name,
                target = host_name,
                "voice-chat stream kept on the host output"
            );
            self.routed.insert(*id);
        }
    }

    /// Undo the pins so the apps follow the default again. `true` = something
    /// was written and the caller must flush before disconnecting.
    pub(super) fn clear(&mut self) -> bool {
        self.links.clear();
        let Some(md) = &self.metadata else {
            self.routed.clear();
            return false;
        };
        let wrote = !self.routed.is_empty();
        for id in self.routed.drain() {
            md.set_property(id, "target.object", None, None);
        }
        wrote
    }
}
