//! The text the executor model reads as the tool's output.
//!
//! Plain `key: value` lines (the shape Locopilot's tool uses and models handle
//! well), one section per query, then a numbered SOURCES list so the model can
//! cite real URLs instead of inventing them.

use crate::tools::web_search::run::{Entry, Report};

/// Render a [`Report`] for the executor.
#[must_use]
pub fn render(report: &Report) -> String {
    let mut out = String::from(
        "web_search_results:\n\
         REMINDER: cite the REAL URLs below (e.g. https://example.com) right after the claims \
         they support. Do not invent URLs and do not use result_N placeholders.\n",
    );
    out.push_str(&format!("queries_used: {}\n", report.queries.len()));
    if report.dropped_queries > 0 {
        out.push_str(&format!(
            "note: {} further queries were NOT run because only {} are allowed per call.\n",
            report.dropped_queries,
            report.queries.len()
        ));
    }

    let mut sources: Vec<(&str, &str)> = Vec::new();
    for query in &report.queries {
        out.push_str("\n---\n\n");
        out.push_str(&format!("query: {}\n", query.query));
        match &query.outcome {
            Err(error) => out.push_str(&format!("results: 0\nsearch_error: {error}\n")),
            Ok(entries) if entries.is_empty() => out.push_str("results: 0\n"),
            Ok(entries) => {
                out.push_str(&format!("results: {}\n", entries.len()));
                for (index, entry) in entries.iter().enumerate() {
                    render_entry(&mut out, index + 1, entry);
                    if let Ok(page) = &entry.page {
                        let title = page.title.as_deref().unwrap_or(&entry.hit.title);
                        if !sources.iter().any(|(_, url)| *url == page.final_url) {
                            sources.push((title, &page.final_url));
                        }
                    } else if !sources.iter().any(|(_, url)| *url == entry.hit.url) {
                        sources.push((&entry.hit.title, &entry.hit.url));
                    }
                }
            }
        }
    }

    if !sources.is_empty() {
        out.push_str("\nSOURCES (cite these exact URLs):\n");
        for (index, (title, url)) in sources.iter().enumerate() {
            out.push_str(&format!("[{}] {title} — {url}\n", index + 1));
        }
    }
    out
}

/// What the model read for one result, without the per-call numbering: the
/// text carried into later turns (see [`crate::tools::web_search::history`]).
#[must_use]
pub fn replay_text(entry: &Entry) -> String {
    let mut out = format!("title: {}\nurl: {}\n", entry.hit.title, entry.hit.url);
    if let Ok(page) = &entry.page
        && page.final_url != entry.hit.url
    {
        out.push_str(&format!("final_url: {}\n", page.final_url));
    }
    if !entry.hit.snippet.is_empty() {
        out.push_str(&format!("snippet: {}\n", entry.hit.snippet));
    }
    match &entry.page {
        Ok(page) if !page.text.trim().is_empty() => {
            out.push_str(&format!("text:\n{}\n", page.text));
            if let Some(note) = &page.note {
                out.push_str(&format!("{note}\n"));
            }
        }
        Ok(_) => out.push_str("text: (no extractable text)\n"),
        Err(reason) => out.push_str(&format!("text: (failed to fetch page: {reason})\n")),
    }
    out
}

fn render_entry(out: &mut String, n: usize, entry: &Entry) {
    out.push_str(&format!("\nresult_{n}_title: {}\n", entry.hit.title));
    out.push_str(&format!("result_{n}_url: {}\n", entry.hit.url));
    if let Ok(page) = &entry.page
        && page.final_url != entry.hit.url
    {
        out.push_str(&format!("result_{n}_final_url: {}\n", page.final_url));
    }
    let snippet = if entry.hit.snippet.is_empty() {
        "(none)"
    } else {
        &entry.hit.snippet
    };
    out.push_str(&format!("result_{n}_snippet: {snippet}\n"));
    match &entry.page {
        Ok(page) => {
            let text = if page.text.trim().is_empty() {
                "(no extractable text)"
            } else {
                &page.text
            };
            out.push_str(&format!("result_{n}_text:\n{text}\n"));
            if let Some(note) = &page.note {
                out.push_str(&format!("{note}\n"));
            }
        }
        Err(reason) => out.push_str(&format!(
            "result_{n}_text: (failed to fetch page: {reason})\n"
        )),
    }
}

#[cfg(test)]
mod tests;
