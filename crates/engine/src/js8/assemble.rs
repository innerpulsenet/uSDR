//! Frames into messages.
//!
//! A JS8 transmission is one or more frames in consecutive slots, from one
//! station on one audio frequency, the first flagged first and the last
//! flagged last. Frames are gathered per speed and frequency (within the
//! tolerance JS8Call itself uses) until the last one arrives. A
//! transmission whose start was missed, or that stops arriving for two
//! slots, or that is cut off by another's first frame, is still reported,
//! marked incomplete, with only the text that decoded.
//!
//! The transmission is then read the way JS8Call reads it: compound
//! callsigns sent in frames of their own fill the `<....>` places in the
//! directed frame that follows, the text after a directed command is its
//! body, and a body that carries a checksum is checked.

use super::varicode::{self, COMMANDS, FrameContent, PLACEHOLDER};
use super::{Js8Frame, Js8Speed};
use std::collections::VecDeque;

/// What the decoder reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Js8Event {
    /// Every frame, as soon as its slot has decoded.
    Frame(Js8Frame),
    /// A transmission put together from its frames.
    Message(Js8Message),
}

/// A transmission put together.
#[derive(Clone, Debug, PartialEq)]
pub struct Js8Message {
    /// "heartbeat", "cq", "directed", "snrReply" or "message" (text not
    /// addressed with a command).
    pub kind: &'static str,
    pub speed: Js8Speed,
    /// UNIX ms of the first frame's slot.
    pub slot_ms: u64,
    /// The frames' mean SNR in 2500 Hz and DT, and the first one's
    /// frequency.
    pub snr_db: f32,
    pub dt_s: f32,
    pub freq_hz: f32,
    pub frames: usize,
    /// Its first and last frames both arrived, and none between them was
    /// missed.
    pub complete: bool,
    /// Slots in the middle in which this station's frame did not decode.
    pub missed: usize,
    pub from: Option<String>,
    /// A callsign or a group such as @ALLCALL.
    pub to: Option<String>,
    /// The directed command without its leading space: "SNR?", "ACK",
    /// "MSG", ">" (relay), "HEARTBEAT", or the CQ called.
    pub command: Option<String>,
    /// The number a command carried; for an SNR reply, the report.
    pub num: Option<i32>,
    pub grid: Option<String>,
    /// What followed the command, or the whole text when there was none.
    pub body: String,
    /// The whole transmission as JS8Call would show it.
    pub text: String,
    /// For the commands that close their text with a checksum: whether it
    /// matched (and was then taken off the text).
    pub checksum_ok: Option<bool>,
    /// A relay's next hop, and who it is relayed for.
    pub relay_to: Option<String>,
    pub relay_from: Option<String>,
    /// A stored message's number, from "MSG ID n".
    pub msg_id: Option<u32>,
    /// Callsigns in it that cannot be real (see
    /// `varicode::plausible_call`): sent as such, so reported, but not to
    /// be taken as a station heard.
    pub implausible_calls: Vec<String>,
}

impl Js8Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Js8Event::Frame(_) => "frame",
            Js8Event::Message(m) => m.kind,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Js8Event::Frame(f) => {
                let bad = f.content.implausible_calls();
                let note = if bad.is_empty() { String::new() } else { format!(" [not a callsign: {}]", bad.join(", ")) };
                format!("{} {:+.0} dB {:.0} Hz: {}{note}", f.speed.label(), f.snr_db, f.freq_hz, f.text().trim_end())
            }
            Js8Event::Message(m) => {
                let note = if m.complete { "" } else { " (incomplete)" };
                let bad = if m.implausible_calls.is_empty() { String::new() } else { format!(" [not a callsign: {}]", m.implausible_calls.join(", ")) };
                format!("{}{note}{bad}", m.text)
            }
        }
    }

    /// Everything, camelCase, as text.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut f = vec![("kind", self.kind().to_string())];
        let opt = |f: &mut Vec<(&'static str, String)>, k: &'static str, v: &Option<String>| {
            if let Some(v) = v {
                f.push((k, v.clone()));
            }
        };
        match self {
            Js8Event::Frame(fr) => {
                f.push(("speed", fr.speed.label().into()));
                f.push(("slot", fr.slot_ms.to_string()));
                f.push(("snr", format!("{:.0}", fr.snr_db)));
                f.push(("dt", format!("{:.2}", fr.dt_s)));
                f.push(("df", format!("{:.1}", fr.freq_hz)));
                f.push(("frameType", fr.content.frame_type().into()));
                f.push(("first", fr.is_first().to_string()));
                f.push(("last", fr.is_last().to_string()));
                f.push(("raw", fr.raw.clone()));
                f.push(("hardErrors", fr.hard_errors.to_string()));
                match &fr.content {
                    FrameContent::Heartbeat { from, grid, cq } => {
                        f.push(("from", from.clone()));
                        opt(&mut f, "grid", grid);
                        if let Some(cq) = cq {
                            f.push(("cq", cq.to_string()));
                        }
                    }
                    FrameContent::Compound { call, grid } => {
                        f.push(("call", call.clone()));
                        opt(&mut f, "grid", grid);
                    }
                    FrameContent::CompoundDirected { to, cmd, num } => {
                        f.push(("to", to.clone()));
                        f.push(("command", COMMANDS[*cmd as usize].trim().into()));
                        if let Some(n) = num {
                            f.push(("num", n.to_string()));
                        }
                    }
                    FrameContent::Directed { from, to, cmd, num } => {
                        f.push(("from", from.clone()));
                        f.push(("to", to.clone()));
                        f.push(("command", COMMANDS[*cmd as usize].trim().into()));
                        if let Some(n) = num {
                            f.push(("num", n.to_string()));
                        }
                    }
                    FrameContent::Data { compressed, .. } => {
                        f.push(("compressed", compressed.to_string()));
                    }
                }
                let bad = fr.content.implausible_calls();
                if !bad.is_empty() {
                    f.push(("implausibleCall", bad.join(" ")));
                }
                f.push(("text", fr.text()));
            }
            Js8Event::Message(m) => {
                f.push(("speed", m.speed.label().into()));
                f.push(("slot", m.slot_ms.to_string()));
                f.push(("snr", format!("{:.0}", m.snr_db)));
                f.push(("dt", format!("{:.2}", m.dt_s)));
                f.push(("df", format!("{:.1}", m.freq_hz)));
                f.push(("frames", m.frames.to_string()));
                f.push(("complete", m.complete.to_string()));
                if m.missed > 0 {
                    f.push(("missed", m.missed.to_string()));
                }
                opt(&mut f, "from", &m.from);
                opt(&mut f, "to", &m.to);
                opt(&mut f, "command", &m.command);
                if let Some(n) = m.num {
                    f.push((if m.kind == "snrReply" { "reportedSnr" } else { "num" }, n.to_string()));
                }
                opt(&mut f, "grid", &m.grid);
                if let Some(ok) = m.checksum_ok {
                    f.push(("checksumOk", ok.to_string()));
                }
                opt(&mut f, "relayTo", &m.relay_to);
                opt(&mut f, "relayFrom", &m.relay_from);
                if let Some(id) = m.msg_id {
                    f.push(("msgId", id.to_string()));
                }
                f.push(("body", m.body.clone()));
                if !m.implausible_calls.is_empty() {
                    f.push(("implausibleCall", m.implausible_calls.join(" ")));
                }
                f.push(("text", m.text.clone()));
            }
        }
        f
    }
}

/// A transmission still arriving.
struct Open {
    speed: Js8Speed,
    freq: f32,
    last_slot: u64,
    frames: Vec<Js8Frame>,
    from_first: bool,
    missed: usize,
}

/// Gathers frames into transmissions.
#[derive(Default)]
pub struct Assembler {
    open: Vec<Open>,
}

impl Assembler {
    /// One decoded slot of one speed: every frame heard in it (possibly
    /// none). Returns the transmissions it finished.
    pub fn slot(&mut self, speed: Js8Speed, slot_ms: u64, frames: &[Js8Frame]) -> Vec<Js8Message> {
        let slot_len = (speed.slot_secs() * 1000.0) as u64;
        let mut out = Vec::new();
        let mut frames: Vec<&Js8Frame> = frames.iter().collect();
        frames.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
        for f in frames {
            let near = self
                .open
                .iter()
                .enumerate()
                .filter(|(_, o)| o.speed == speed && o.last_slot < slot_ms && (o.freq - f.freq_hz).abs() <= speed.join_hz())
                .min_by(|a, b| (a.1.freq - f.freq_hz).abs().total_cmp(&(b.1.freq - f.freq_hz).abs()))
                .map(|(i, _)| i);
            let i = match (near, f.is_first()) {
                (Some(i), false) => {
                    let o = &mut self.open[i];
                    o.missed += ((slot_ms - o.last_slot) / slot_len).saturating_sub(1) as usize;
                    o.frames.push(f.clone());
                    o.freq = f.freq_hz;
                    o.last_slot = slot_ms;
                    i
                }
                (near, first) => {
                    // A first frame cuts short whatever was open there.
                    if let Some(i) = near {
                        let o = self.open.remove(i);
                        out.push(build(o, false));
                    }
                    self.open.push(Open { speed, freq: f.freq_hz, last_slot: slot_ms, frames: vec![f.clone()], from_first: first, missed: 0 });
                    self.open.len() - 1
                }
            };
            if f.is_last() {
                let o = self.open.remove(i);
                let done = o.from_first;
                out.push(build(o, done));
            }
        }
        // Two slots without a frame: the rest is not coming.
        let mut k = 0;
        while k < self.open.len() {
            let o = &self.open[k];
            if o.speed == speed && slot_ms >= o.last_slot + 2 * slot_len {
                let o = self.open.remove(k);
                out.push(build(o, false));
            } else {
                k += 1;
            }
        }
        out
    }

    /// Everything still open, as it stands: after a retune nothing more of
    /// it will be heard.
    pub fn flush(&mut self) -> Vec<Js8Message> {
        self.open.drain(..).map(|o| build(o, false)).collect()
    }

    pub fn open(&self) -> usize {
        self.open.len()
    }
}

/// A callsign followed by a colon at the start of free text: who sent it.
fn leading_call(text: &str) -> Option<String> {
    let (call, _) = text.trim_start().split_once(':')?;
    let ok = call.len() >= 3
        && call.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '/')
        && call.chars().any(|c| c.is_ascii_digit())
        && call.chars().any(|c| c.is_ascii_uppercase());
    ok.then(|| call.to_string())
}

fn build(o: Open, ends_whole: bool) -> Js8Message {
    let n = o.frames.len();
    let first = &o.frames[0];
    let mut m = Js8Message {
        kind: "message",
        speed: o.speed,
        slot_ms: first.slot_ms,
        snr_db: (o.frames.iter().map(|f| f.snr_db).sum::<f32>() / n as f32).round(),
        dt_s: o.frames.iter().map(|f| f.dt_s).sum::<f32>() / n as f32,
        freq_hz: first.freq_hz,
        frames: n,
        complete: ends_whole && o.missed == 0,
        missed: o.missed,
        from: None,
        to: None,
        command: None,
        num: None,
        grid: None,
        body: String::new(),
        text: String::new(),
        checksum_ok: None,
        relay_to: None,
        relay_from: None,
        msg_id: None,
        implausible_calls: Vec::new(),
    };
    for f in &o.frames {
        for c in f.content.implausible_calls() {
            if !m.implausible_calls.contains(&c) {
                m.implausible_calls.push(c);
            }
        }
    }
    let mut compounds: VecDeque<(String, Option<String>)> = o
        .frames
        .iter()
        .filter_map(|f| match &f.content {
            FrameContent::Compound { call, grid } => Some((call.clone(), grid.clone())),
            _ => None,
        })
        .collect();
    let lone_compound = compounds.front().cloned();
    let mut header = String::new();
    let mut cmd = None;
    let mut body = String::new();
    let mut last_slot = first.slot_ms;
    let slot_len = (o.speed.slot_secs() * 1000.0) as u64;
    for (i, f) in o.frames.iter().enumerate() {
        // Where a frame did not decode, the text has a hole; say so rather
        // than run the pieces together.
        if i > 0 && f.slot_ms > last_slot + slot_len {
            body.push_str(" … ");
        }
        last_slot = f.slot_ms;
        let heading = cmd.is_none() && m.command.is_none();
        match &f.content {
            FrameContent::Compound { .. } => {}
            FrameContent::Heartbeat { from, grid, cq } if heading => {
                m.kind = if cq.is_some() { "cq" } else { "heartbeat" };
                m.from = Some(from.clone());
                m.to = Some(if cq.is_some() { "@ALLCALL" } else { "@HB" }.into());
                m.command = Some(cq.unwrap_or("HEARTBEAT").into());
                m.grid = grid.clone();
                header = f.text();
            }
            FrameContent::Directed { from, to, cmd: c, num } if heading => {
                let (mut from, mut to) = (from.clone(), to.clone());
                if from == PLACEHOLDER
                    && let Some((call, grid)) = compounds.pop_front()
                {
                    from = call;
                    m.grid = grid;
                }
                if to == PLACEHOLDER
                    && let Some((call, _)) = compounds.pop_front()
                {
                    to = call;
                }
                header = format!("{from}: {to}{}", varicode::command_text(*c, *num));
                m.from = Some(from);
                m.to = Some(to);
                m.num = *num;
                cmd = Some(*c);
            }
            FrameContent::CompoundDirected { to, cmd: c, num } if heading => {
                let from = match compounds.pop_front() {
                    Some((call, grid)) => {
                        m.grid = grid;
                        call
                    }
                    None => PLACEHOLDER.to_string(),
                };
                header = format!("{from}: {to}{}", varicode::command_text(*c, *num));
                m.from = Some(from);
                m.to = Some(to.clone());
                m.num = *num;
                cmd = Some(*c);
            }
            // Text, or a second command inside one transmission (a
            // heartbeat's acknowledgment, say): JS8Call runs it on.
            _ => body.push_str(&f.text()),
        }
    }
    if let Some(c) = cmd {
        m.kind = if varicode::is_snr(c) { "snrReply" } else { "directed" };
        m.command = Some(COMMANDS[c as usize].trim().to_string()).filter(|s| !s.is_empty());
    }
    let mut body = body.trim_end().to_string();
    if let Some(c) = cmd {
        body = body.trim_start().to_string();
        if varicode::checksummed(c) && body.len() > 4 && body.is_char_boundary(body.len() - 4) {
            let (text, sum) = body.split_at(body.len() - 4);
            let ok = sum.starts_with(' ') && varicode::checksum16(text) == sum[1..];
            m.checksum_ok = Some(ok);
            if ok {
                body = text.to_string();
            }
        }
        if c == 15 {
            m.grid = body.split_whitespace().next().filter(|g| g.len() >= 4).map(str::to_string).or(m.grid.take());
        }
        if c == 5 {
            // A relay: "NEXT> text *DE* ORIGIN", or the last hop's text.
            if let Some((next, _)) = body.split_once('>')
                && !next.contains(' ')
                && leading_call(&format!("{next}:")).is_some()
            {
                m.relay_to = Some(next.to_string());
            }
            if let Some((_, by)) = body.rsplit_once("*DE*") {
                m.relay_from = by.split_whitespace().next().map(str::to_string);
            }
        }
    } else if m.from.is_none() {
        match &lone_compound {
            // A long callsign introducing plain text.
            Some((call, grid)) => {
                header = format!("{call}: ");
                m.from = Some(call.clone());
                m.grid = grid.clone();
            }
            None => m.from = leading_call(&body),
        }
    }
    m.text = if header.is_empty() {
        body.clone()
    } else if cmd.is_some() && !body.is_empty() && !header.ends_with(' ') && !header.ends_with('>') {
        format!("{header} {body}")
    } else {
        format!("{header}{body}")
    };
    m.text = m.text.trim_end().to_string();
    if let Some(i) = m.text.find("MSG ID ") {
        m.msg_id = m.text[i + 7..].split_whitespace().next().and_then(|v| v.parse().ok());
    }
    m.body = body;
    m
}
