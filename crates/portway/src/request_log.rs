use crate::logfmt::{self, BOLD, CYAN, DIM, GREEN, YELLOW};
use crate::telemetry::RequestRecord;
use crate::usage::Usage;
pub fn render(record: &RequestRecord) -> String {
    let agent_coding = record.agent_encoding.as_deref();
    let up = match record.coding.name() {
        Some(name) if record.body_len > 0 => {
            let saved = record.body_len.saturating_sub(record.wire_len) * 100 / record.body_len;
            format!(
                "{} {} -> {} {}",
                logfmt::c(DIM, "up"),
                logfmt::c(CYAN, &logfmt::human(record.body_len)),
                logfmt::c(CYAN, &logfmt::human(record.wire_len)),
                logfmt::c(DIM, &format!("({name}, -{saved}%)")),
            )
        }
        _ => format!(
            "{} {} -> {} {}",
            logfmt::c(DIM, "up"),
            logfmt::c(CYAN, &logfmt::human(record.body_len)),
            logfmt::c(CYAN, &logfmt::human(record.wire_len)),
            logfmt::c(DIM, "(identity)"),
        ),
    };
    // Trailing time: first body byte to the last byte ACKed by the far end.
    let up = match record.upload {
        Some(upload) => format!("{up} {}", logfmt::c(YELLOW, &logfmt::human_time(upload))),
        None => up,
    };

    let phases: Vec<String> = [
        ("dns", record.dns),
        ("tcp", record.tcp),
        ("tls", record.tls),
    ]
    .into_iter()
    .filter_map(|(name, elapsed)| {
        elapsed.map(|e| format!("{name} {}", logfmt::c(YELLOW, &logfmt::human_time(e))))
    })
    .collect();
    let conn = if phases.is_empty() {
        logfmt::c(GREEN, "reused")
    } else {
        phases.join(" ")
    };

    let download = match record.download {
        None => String::new(),
        Some(first) => format!(" {}", logfmt::c(YELLOW, &logfmt::human_time(first))),
    };

    let sep = logfmt::c(DIM, " | ");
    let segments = [
        format!("{} {conn}", logfmt::c(DIM, "conn")),
        up,
        format!(
            "{} {}",
            logfmt::c(DIM, "ttfb"),
            logfmt::c(YELLOW, &logfmt::human_time(record.ttfb))
        ),
        down(record, agent_coding, &download),
    ];
    let mut line = format!(
        "{} {} -> {}{sep}{}",
        logfmt::c(BOLD, record.method.as_str()),
        record.path,
        logfmt::status(record.status),
        segments.join(&sep)
    );
    // Only an answer that carried counts gets the segment: an upstream that
    // reports none (or an agent that cut the stream short) says nothing
    // rather than printing zeroes it never saw.
    if let Some(usage) = record.usage {
        line.push_str(&sep);
        line.push_str(&tokens(usage));
    }
    line
}

/// The answer and the hops it crossed, the upload's `raw -> wire (coding,
/// -N%)` read the other way round:
///
/// - `down 95KB (identity)`: nothing coded on either side;
/// - `down 95KB <- 11KB (zstd, -88%)`: what the upstream hop carried, which
///   behind a receiver is the download it saved;
/// - `down 92KB -> 11KB (identity -> zstd)`: what the agent got, re-encoded;
/// - `down 37KB <- 5KB (zstd, -86%) -> 0.6KB (zstd)`: both.
fn down(record: &RequestRecord, agent_coding: Option<&str>, download: &str) -> String {
    let coded = record.upstream_encoding != "identity";
    let mut out = format!(
        "{} {}",
        logfmt::c(DIM, "down"),
        logfmt::c(CYAN, &logfmt::human(record.received))
    );
    if coded {
        let saved =
            record.received.saturating_sub(record.received_wire) * 100 / record.received.max(1);
        out.push_str(&format!(
            " <- {} {}",
            logfmt::c(CYAN, &logfmt::human(record.received_wire)),
            logfmt::c(DIM, &format!("({}, -{saved}%)", record.upstream_encoding)),
        ));
    }
    match agent_coding {
        Some(agent) if coded => out.push_str(&format!(
            " -> {} {}",
            logfmt::c(CYAN, &logfmt::human(record.received_agent)),
            logfmt::c(DIM, &format!("({agent})")),
        )),
        Some(agent) => out.push_str(&format!(
            " -> {} {}",
            logfmt::c(CYAN, &logfmt::human(record.received_agent)),
            logfmt::c(DIM, &format!("({} -> {agent})", record.upstream_encoding)),
        )),
        None if coded => {}
        None => out.push_str(&format!(
            " {}",
            logfmt::c(DIM, &format!("({})", record.upstream_encoding))
        )),
    }
    out.push_str(download);
    out
}

/// `tok N in (M cached) -> N out (N reasoning)`: what the engine said the turn
/// cost. The counts are printed whole — a token count is the thing being
/// accounted for, and 689KB-style rounding is for sizes.
fn tokens(usage: Usage) -> String {
    let cached = match usage.cached {
        Some(cached) => format!(" {}", logfmt::c(DIM, &format!("({cached} cached)"))),
        None => String::new(),
    };
    let reasoning = match usage.reasoning {
        Some(reasoning) => format!(" {}", logfmt::c(DIM, &format!("({reasoning} reasoning)"))),
        None => String::new(),
    };
    format!(
        "{} {}{cached} -> {} out{reasoning}",
        logfmt::c(DIM, "tok"),
        logfmt::c(CYAN, &format!("{} in", usage.prompt)),
        logfmt::c(CYAN, &usage.completion.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forwarder::Coding;

    fn record(
        received_wire: u64,
        upstream: &str,
        agent: Option<&str>,
        received_agent: u64,
    ) -> RequestRecord {
        RequestRecord {
            stamp: "12:00:00".into(),
            upstream: "codex".into(),
            model: "gpt-x".into(),
            tier: None,
            method: http::Method::POST,
            path: "/responses".into(),
            status: 200,
            dns: None,
            tcp: None,
            tls: None,
            body_len: 0,
            wire_len: 0,
            coding: Coding::None,
            upload: None,
            ttfb: 1.0,
            received: 100_000,
            received_wire,
            received_agent,
            upstream_encoding: upstream.into(),
            agent_encoding: agent.map(str::to_owned),
            download: None,
            complete: true,
            usage: None,
            flight: None,
        }
    }

    /// The segment after `| down`: ANSI codes stripped, since a terminal may
    /// have turned colour on.
    fn down_segment(record: &RequestRecord) -> String {
        let line = render(record);
        let mut plain = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                plain.push(c);
            }
        }
        plain.rsplit(" | ").next().unwrap().to_string()
    }

    #[test]
    fn the_down_segment_names_every_hop_that_saved_bytes() {
        assert_eq!(
            down_segment(&record(100_000, "identity", None, 0)),
            "down 98KB (identity)"
        );
        // Behind a receiver, an agent that asks for no coding: the tunnel's saving.
        assert_eq!(
            down_segment(&record(12_000, "zstd", None, 0)),
            "down 98KB <- 12KB (zstd, -88%)"
        );
        assert_eq!(
            down_segment(&record(100_000, "identity", Some("zstd"), 11_000)),
            "down 98KB -> 11KB (identity -> zstd)"
        );
        assert_eq!(
            down_segment(&record(12_000, "zstd", Some("zstd"), 11_000)),
            "down 98KB <- 12KB (zstd, -88%) -> 11KB (zstd)"
        );
    }
}
