//! The domain rules a client sets on a search or fetch tool
//! (`allowed_domains` / `blocked_domains`), applied by the proxy's own tools.
//!
//! The matching follows Anthropic's documented rules:
//!
//! * a rule is a domain with no scheme, optionally followed by a path
//!   (`example.com`, `docs.example.com`, `example.com/blog`);
//! * a domain rule covers its subdomains (`example.com` matches
//!   `docs.example.com`), while a subdomain rule matches only that subdomain
//!   and below (`docs.example.com` does NOT match `example.com`);
//! * a path rule matches URLs whose path starts with it;
//! * matching ignores case, and a leading `http(s)://` or trailing `/` in a
//!   rule is tolerated rather than rejected.
//!
//! An empty list is the same as no list. The two lists are never both set:
//! that request is rejected when it arrives
//! ([`crate::anthropic::validate_server_tool_options`]).

use crate::anthropic::ServerToolOptions;

/// Most rules put in a search query: a longer query only invites the engine
/// to ignore it. The results are filtered afterwards regardless.
const MAX_QUERY_RULES: usize = 10;

/// One parsed rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    host: String,
    /// `None`, or a path prefix that starts with `/`.
    path: Option<String>,
}

impl Rule {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        let raw = raw
            .strip_prefix("https://")
            .or_else(|| raw.strip_prefix("http://"))
            .unwrap_or(&raw);
        let (host, path) = match raw.split_once('/') {
            Some((host, path)) => (host, path.trim_end_matches('/')),
            None => (raw, ""),
        };
        let host = host.trim_matches('.');
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_owned(),
            path: (!path.is_empty()).then(|| format!("/{path}")),
        })
    }

    fn matches(&self, host: &str, path: &str) -> bool {
        let host_ok = host == self.host
            || host
                .strip_suffix(&self.host)
                .is_some_and(|rest| rest.ends_with('.'));
        host_ok && self.path.as_deref().is_none_or(|p| path.starts_with(p))
    }

    /// The rule as a search-engine `site:` operand.
    fn site(&self) -> String {
        format!("{}{}", self.host, self.path.as_deref().unwrap_or(""))
    }
}

/// The parsed allow- or block-list of one tool. The default allows
/// everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainRules {
    allowed: Vec<Rule>,
    blocked: Vec<Rule>,
}

impl DomainRules {
    #[must_use]
    pub fn from_options(options: &ServerToolOptions) -> Self {
        let parse = |list: &Option<Vec<String>>| {
            list.iter()
                .flatten()
                .filter_map(|raw| Rule::parse(raw))
                .collect()
        };
        Self {
            allowed: parse(&options.allowed_domains),
            blocked: parse(&options.blocked_domains),
        }
    }

    /// Whether no rule is set (everything is allowed).
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.allowed.is_empty() && self.blocked.is_empty()
    }

    /// Whether `url` may be searched for / fetched. A string that is not a
    /// URL with a host is never allowed while a rule is set.
    #[must_use]
    pub fn allows(&self, url: &str) -> bool {
        if self.is_unrestricted() {
            return true;
        }
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let path = parsed.path().to_ascii_lowercase();
        let listed = |rules: &[Rule]| rules.iter().any(|r| r.matches(&host, &path));
        (self.allowed.is_empty() || listed(&self.allowed)) && !listed(&self.blocked)
    }

    /// `query` with the rules added as search operators, so the engine does
    /// the narrowing: `(site:a.com OR site:b.com)` for an allow-list,
    /// `-site:x.com` for a block-list. Unchanged when unrestricted.
    #[must_use]
    pub fn narrow_query(&self, query: &str) -> String {
        let mut out = query.to_owned();
        if !self.allowed.is_empty() {
            let sites: Vec<String> = self
                .allowed
                .iter()
                .take(MAX_QUERY_RULES)
                .map(|r| format!("site:{}", r.site()))
                .collect();
            if let [only] = sites.as_slice() {
                out.push(' ');
                out.push_str(only);
            } else {
                out.push_str(&format!(" ({})", sites.join(" OR ")));
            }
        }
        for rule in self.blocked.iter().take(MAX_QUERY_RULES) {
            out.push_str(&format!(" -site:{}", rule.site()));
        }
        out
    }
}

#[cfg(test)]
mod tests;
