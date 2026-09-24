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
        match agent_coding {
            // Two sizes, like the upload: what the answer was, and what the
            // agent received after the hop re-encoded it.
            Some(agent) => format!(
                "{} {} -> {} {}{download}",
                logfmt::c(DIM, "down"),
                logfmt::c(CYAN, &logfmt::human(record.received)),
                logfmt::c(CYAN, &logfmt::human(record.received_agent)),
                logfmt::c(DIM, &format!("({} -> {agent})", record.upstream_encoding)),
            ),
            None => format!(
                "{} {} {}{download}",
                logfmt::c(DIM, "down"),
                logfmt::c(CYAN, &logfmt::human(record.received)),
                logfmt::c(DIM, &format!("({})", record.upstream_encoding)),
            ),
        },
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
