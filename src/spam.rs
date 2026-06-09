//! Spam filtering & anti-spoofing.
//!
//! Two complementary layers:
//!
//! 1. **Heuristics** — fast, training-free rules that score authentication
//!    failures (SPF/DKIM/DMARC), display-name spoofing, suspicious wording, and
//!    other classic signals. These are strong from the very first message.
//! 2. **Bayesian** — an online classifier ([`BayesModel`]) trained from the
//!    user's own "spam"/"not spam" feedback, persisted in the local store.
//!
//! Allow/block lists (by address or domain) override both: a blocked sender is
//! always spam; an allowed sender is never spam.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The result of classifying a message.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// Combined spam probability in `0.0..=1.0`.
    pub score: f32,
    /// Whether the score crossed the configured threshold.
    pub is_spam: bool,
    /// Human-readable reasons, shown in the UI so the user can judge for
    /// themselves (important for anti-fraud, goal #3/#4).
    pub reasons: Vec<String>,
}

/// Everything the classifier looks at for one message.
#[derive(Debug, Default, Clone)]
pub struct Signals {
    pub from_name: String,
    pub from_addr: String,
    pub reply_to: String,
    pub to_addrs: String,
    pub subject: String,
    pub body: String,
    /// Raw `Authentication-Results` header value, if present.
    pub auth_results: String,
    /// The account's own address (to detect bulk/BCC mail).
    pub account: String,
}

/// Classic spam vocabulary (lower-case). Matched as whole words.
const SPAM_WORDS: &[&str] = &[
    "viagra",
    "cialis",
    "lottery",
    "winner",
    "jackpot",
    "casino",
    "prince",
    "inheritance",
    "bitcoin",
    "crypto",
    "forex",
    "investment",
    "refinance",
    "mortgage",
    "prescription",
    "pharmacy",
    "rolex",
    "replica",
    "unsubscribe",
    "guaranteed",
    "risk-free",
    "wire",
    "transfer",
    "bonus",
    "claim",
    "prize",
    "congratulations",
    "urgent",
    "verify",
    "suspended",
    "password",
    "gift",
    "voucher",
    "deadline",
    "act",
    "now",
    "limited",
    "offer",
    "cheap",
];

/// Tokenise text for the Bayesian model: lower-cased alphanumeric words of a
/// sensible length. Also used for training.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        let t = raw.to_ascii_lowercase();
        let len = t.chars().count();
        if (2..=24).contains(&len) {
            out.push(t);
        }
    }
    out
}

/// A naive-Bayes spam classifier using Paul Graham's combining method, biased
/// toward ham to keep false positives low. Persisted via the store.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BayesModel {
    pub ham_messages: u32,
    pub spam_messages: u32,
    /// token -> (ham occurrences, spam occurrences).
    pub tokens: HashMap<String, (u32, u32)>,
}

impl BayesModel {
    /// True once the user has provided at least one example.
    pub fn is_trained(&self) -> bool {
        self.ham_messages > 0 || self.spam_messages > 0
    }

    /// Learn from one labelled example. Each distinct token counts once.
    pub fn train(&mut self, text: &str, is_spam: bool) {
        if is_spam {
            self.spam_messages += 1;
        } else {
            self.ham_messages += 1;
        }
        let mut seen = std::collections::HashSet::new();
        for tok in tokenize(text) {
            if !seen.insert(tok.clone()) {
                continue;
            }
            let entry = self.tokens.entry(tok).or_insert((0, 0));
            if is_spam {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
    }

    /// Spamminess of a single token in `0.01..=0.99`.
    fn token_prob(&self, token: &str) -> f32 {
        match self.tokens.get(token) {
            // Unknown tokens lean very slightly hammy.
            None => 0.4,
            Some(&(ham, spam)) => {
                if ham + spam == 0 {
                    return 0.4;
                }
                let hm = self.ham_messages.max(1) as f32;
                let sm = self.spam_messages.max(1) as f32;
                // Ham counts are doubled to bias against false positives.
                let g = (2.0 * ham as f32 / hm).min(1.0);
                let b = (spam as f32 / sm).min(1.0);
                if g + b == 0.0 {
                    return 0.4;
                }
                (b / (g + b)).clamp(0.01, 0.99)
            }
        }
    }

    /// Combined spam probability of a message in `0.0..=1.0`. Returns `0.0`
    /// (no opinion) until the model has been trained.
    pub fn score(&self, text: &str) -> f32 {
        if !self.is_trained() {
            return 0.0;
        }
        let mut distinct: Vec<String> = tokenize(text);
        distinct.sort();
        distinct.dedup();
        if distinct.is_empty() {
            return 0.0;
        }
        let mut probs: Vec<f32> = distinct.iter().map(|t| self.token_prob(t)).collect();
        // Keep the most "interesting" tokens (farthest from 0.5).
        probs.sort_by(|a, b| {
            (b - 0.5)
                .abs()
                .partial_cmp(&(a - 0.5).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        probs.truncate(15);

        let prod: f32 = probs.iter().product();
        let inv: f32 = probs.iter().map(|p| 1.0 - p).product();
        if prod + inv == 0.0 {
            0.0
        } else {
            prod / (prod + inv)
        }
    }
}

/// Does `addr` match a list entry (full address or bare domain)?
fn list_matches(addr: &str, list: &[String]) -> bool {
    let addr = addr.trim().to_ascii_lowercase();
    if addr.is_empty() {
        return false;
    }
    let domain = domain_of(&addr);
    list.iter().any(|raw| {
        let entry = raw.trim().to_ascii_lowercase();
        !entry.is_empty() && (entry == addr || entry == domain)
    })
}

/// The domain portion of an address (after `@`), or the whole string.
fn domain_of(addr: &str) -> &str {
    addr.rsplit('@').next().unwrap_or(addr)
}

/// Score a message with the training-free heuristics, returning a delta in
/// `0.0..=1.0` plus the reasons that contributed.
fn heuristic_score(s: &Signals) -> (f32, Vec<String>) {
    let mut score = 0.0f32;
    let mut reasons = Vec::new();
    let add = |amount: f32, reason: &str, score: &mut f32, reasons: &mut Vec<String>| {
        *score += amount;
        reasons.push(reason.to_string());
    };

    // --- Authentication / anti-spoofing ---
    let auth = s.auth_results.to_ascii_lowercase();
    if auth.contains("dmarc=fail") {
        add(
            0.45,
            "Failed DMARC authentication",
            &mut score,
            &mut reasons,
        );
    }
    if auth.contains("spf=fail") || auth.contains("spf=softfail") {
        add(0.25, "Failed SPF check", &mut score, &mut reasons);
    }
    if auth.contains("dkim=fail") {
        add(0.20, "Failed DKIM signature", &mut score, &mut reasons);
    }

    // --- Sender / display-name spoofing ---
    if s.from_addr.trim().is_empty() {
        add(0.30, "Missing sender address", &mut score, &mut reasons);
    }
    let from_domain = domain_of(&s.from_addr.to_ascii_lowercase()).to_string();
    if s.from_name.contains('@')
        && !from_domain.is_empty()
        && !s.from_name.to_ascii_lowercase().contains(&from_domain)
    {
        add(
            0.35,
            "Display name shows a different email address",
            &mut score,
            &mut reasons,
        );
    }
    if !s.reply_to.trim().is_empty() && !from_domain.is_empty() {
        let reply_domain = domain_of(&s.reply_to.to_ascii_lowercase()).to_string();
        if !reply_domain.is_empty() && reply_domain != from_domain {
            add(
                0.15,
                "Reply-To uses a different domain",
                &mut score,
                &mut reasons,
            );
        }
    }

    // --- Recipient signals ---
    if !s.account.trim().is_empty()
        && !s
            .to_addrs
            .to_ascii_lowercase()
            .contains(&s.account.to_ascii_lowercase())
    {
        add(
            0.10,
            "You are not a direct recipient",
            &mut score,
            &mut reasons,
        );
    }

    // --- Subject shape ---
    let subject = s.subject.trim();
    let letters: String = subject.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.chars().count() >= 8 && letters.chars().all(|c| c.is_uppercase()) {
        add(0.15, "Subject is all capitals", &mut score, &mut reasons);
    }
    if subject.matches('!').count() >= 3 {
        add(
            0.10,
            "Excessive punctuation in subject",
            &mut score,
            &mut reasons,
        );
    }

    // --- Spam vocabulary ---
    let haystack = format!("{} {}", s.subject, s.body).to_ascii_lowercase();
    let words: std::collections::HashSet<&str> = haystack
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let hits = SPAM_WORDS.iter().filter(|w| words.contains(**w)).count();
    if hits >= 2 {
        let amount = (0.08 * hits as f32).min(0.35);
        add(
            0.0,
            &format!("Contains {hits} spam-associated terms"),
            &mut score,
            &mut reasons,
        );
        score += amount;
    }

    // --- Link spam ---
    let links = s.body.to_ascii_lowercase().matches("http").count();
    if links >= 10 {
        add(0.15, "Many links in the body", &mut score, &mut reasons);
    }

    (score.min(1.0), reasons)
}

/// Classify a message using lists, heuristics and the trained model.
pub fn classify(
    signals: &Signals,
    bayes: &BayesModel,
    block_list: &[String],
    allow_list: &[String],
    threshold: f32,
) -> Verdict {
    // Lists win outright.
    if list_matches(&signals.from_addr, block_list) {
        return Verdict {
            score: 1.0,
            is_spam: true,
            reasons: vec!["Sender is on your block list".to_string()],
        };
    }
    if list_matches(&signals.from_addr, allow_list) {
        return Verdict {
            score: 0.0,
            is_spam: false,
            reasons: vec!["Sender is on your allow list".to_string()],
        };
    }

    let (h, mut reasons) = heuristic_score(signals);
    let text = format!("{} {} {}", signals.subject, signals.from_addr, signals.body);
    let b = bayes.score(&text);

    // Probabilistic OR: either layer can raise suspicion.
    let combined = if bayes.is_trained() {
        if b >= 0.85 {
            reasons.push("Matches your learned spam patterns".to_string());
        }
        1.0 - (1.0 - h) * (1.0 - b)
    } else {
        h
    };

    Verdict {
        score: combined,
        is_spam: combined >= threshold,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> Signals {
        Signals {
            from_name: "Alice".into(),
            from_addr: "alice@example.com".into(),
            reply_to: String::new(),
            to_addrs: "me@example.com".into(),
            subject: "Lunch tomorrow?".into(),
            body: "Want to grab lunch around noon?".into(),
            auth_results: "spf=pass dkim=pass dmarc=pass".into(),
            account: "me@example.com".into(),
        }
    }

    #[test]
    fn clean_mail_is_not_spam() {
        let v = classify(&signals(), &BayesModel::default(), &[], &[], 0.5);
        assert!(!v.is_spam, "score was {}", v.score);
        assert!(v.score < 0.5);
    }

    #[test]
    fn block_list_forces_spam() {
        let block = vec!["example.com".to_string()];
        let v = classify(&signals(), &BayesModel::default(), &block, &[], 0.5);
        assert!(v.is_spam);
        assert_eq!(v.score, 1.0);
    }

    #[test]
    fn allow_list_overrides_heuristics() {
        let mut s = signals();
        s.subject = "WINNER!!! CLAIM YOUR FREE LOTTERY PRIZE".into();
        s.body = "wire transfer bitcoin guaranteed bonus".into();
        s.auth_results = "dmarc=fail spf=fail".into();
        let allow = vec!["alice@example.com".to_string()];
        let v = classify(&s, &BayesModel::default(), &[], &allow, 0.5);
        assert!(!v.is_spam);
    }

    #[test]
    fn auth_failure_and_keywords_flag_spam() {
        let mut s = signals();
        s.subject = "WINNER CLAIM YOUR FREE LOTTERY PRIZE".into();
        s.body = "wire transfer bitcoin guaranteed bonus prize".into();
        s.auth_results = "dmarc=fail spf=fail".into();
        let v = classify(&s, &BayesModel::default(), &[], &[], 0.5);
        assert!(v.is_spam, "score was {} reasons {:?}", v.score, v.reasons);
    }

    #[test]
    fn bayes_learns_from_feedback() {
        let mut m = BayesModel::default();
        for _ in 0..5 {
            m.train("cheap meds buy now discount pharmacy offer", true);
            m.train("project meeting notes agenda schedule review", false);
        }
        assert!(m.score("cheap meds discount pharmacy offer now") > 0.6);
        assert!(m.score("project meeting agenda review schedule") < 0.4);
    }
}
