//! User-visible delivery sanitizer.
//!
//! This guard is intentionally conservative about normal language and strict
//! about internal reasoning patterns. It is a last line of defense for channel
//! delivery paths, not a replacement for prompt policy.

pub const BLOCKED_INTERNAL_OUTPUT_FALLBACK: &str = "Output internal terdeteksi dan diblokir.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedDelivery {
    pub text: String,
    pub changed: bool,
    pub blocked: bool,
}

pub fn sanitize_delivery_text(text: &str) -> SanitizedDelivery {
    sanitize_delivery_text_inner(text, true)
}

pub fn sanitize_delivery_text_partial(text: &str) -> SanitizedDelivery {
    sanitize_delivery_text_inner(text, false)
}

fn sanitize_delivery_text_inner(text: &str, fallback_on_empty: bool) -> SanitizedDelivery {
    let original = text.trim();
    if original.is_empty() {
        return SanitizedDelivery {
            text: String::new(),
            changed: text != original,
            blocked: false,
        };
    }

    let normalized = normalize_delivery_wrappers(original);
    let without_blocks = strip_reasoning_blocks(&normalized);
    let without_fences = strip_reasoning_fences(&without_blocks);
    let without_label = strip_leading_reasoning_label(&without_fences);
    let without_preamble = strip_visible_reasoning_preamble(&without_label);
    let sanitized = without_preamble.trim().to_string();

    if sanitized.is_empty() {
        return SanitizedDelivery {
            text: if fallback_on_empty {
                BLOCKED_INTERNAL_OUTPUT_FALLBACK.to_string()
            } else {
                String::new()
            },
            changed: true,
            blocked: true,
        };
    }

    SanitizedDelivery {
        changed: sanitized != original,
        text: sanitized,
        blocked: false,
    }
}

fn normalize_delivery_wrappers(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim().to_ascii_lowercase();
            !matches!(trimmed.as_str(), "(continued)" | "(continues...)")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn strip_reasoning_blocks(text: &str) -> String {
    let mut output = text.to_string();
    for tag in ["think", "thinking", "reasoning", "analysis"] {
        output = strip_tag_blocks(&output, tag);
    }
    output
}

fn strip_tag_blocks(text: &str, tag: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    let start_prefix = format!("<{tag}");
    let end_tag = format!("</{tag}>");

    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find(&start_prefix) else {
            result.push_str(rest);
            break;
        };

        let Some(open_end_rel) = lower[start..].find('>') else {
            result.push_str(&rest[..start]);
            break;
        };

        result.push_str(&rest[..start]);
        let content_start = start + open_end_rel + 1;
        if let Some(end_rel) = lower[content_start..].find(&end_tag) {
            let next = content_start + end_rel + end_tag.len();
            rest = &rest[next..];
        } else {
            break;
        }
    }

    result
}

fn strip_reasoning_fences(text: &str) -> String {
    let mut output = Vec::new();
    let mut dropping: Option<&str> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(fence) = dropping {
            if trimmed.starts_with(fence) {
                dropping = None;
            }
            continue;
        }

        let fence = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };

        if let Some(fence) = fence {
            let label = trimmed
                .trim_start_matches(fence)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(
                label.as_str(),
                "think" | "thinking" | "reasoning" | "analysis" | "internal"
            ) {
                dropping = Some(fence);
                continue;
            }
        }

        output.push(line);
    }

    output.join("\n")
}

fn strip_leading_reasoning_label(text: &str) -> String {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    const LABELS: &[&str] = &[
        "reasoning:",
        "thinking:",
        "analysis:",
        "internal reasoning:",
        "internal analysis:",
    ];

    if !LABELS.iter().any(|label| lower.starts_with(label)) {
        return trimmed.to_string();
    }

    if let Some(start) = visible_answer_start(trimmed) {
        return trimmed[start..].trim_start().to_string();
    }

    String::new()
}

fn looks_like_visible_reasoning_preamble(text: &str) -> bool {
    let lower = text
        .trim_start()
        .chars()
        .take(1600)
        .collect::<String>()
        .to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "the user is asking",
        "user is asking",
        "the user asked",
        "the user is saying",
        "user is saying",
        "the user says",
        "user says",
        "the user wants",
        "user wants",
        "the question is",
        "this likely refers",
        "okay, i need",
        "ok, i need",
        "actually let me",
        "actually, let me",
        "hmm,",
        "hmm ",
        "i need to answer",
        "i need to ",
        "i should ",
        "let me check",
        "let me search",
        "let me look",
        "let me try",
        "let me also",
        "let me write",
        "now let me",
        "i need to check",
        "i should check",
        "i will check",
        "i'll check",
        "i'm going to",
        "we need to",
        "let's analyze",
        "looking at the",
        "actually, looking",
        "actually,",
        "so the user",
        "from the memory context",
        "the memory context",
        "based on the memory",
        "i found",
        "jadi ini cron job",
        "jadi ini ",
        "ini cron job",
        "aku harus ",
        "aku akan ",
        "aku akan cek",
        "aku cek ",
        "cek dulu",
        "coba aku",
        "mari aku",
        "sepertinya",
        "mau laporan diulang",
    ];
    MARKERS.iter().any(|marker| lower.starts_with(marker))
}

fn visible_answer_start(text: &str) -> Option<usize> {
    if let Some(start) = line_visible_answer_start(text) {
        return Some(start);
    }

    let lower = text.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "\n📊",
        "\n💰",
        "\n💵",
        "\n📈",
        "\n✅",
        "\n⚠️",
        "\n#",
        "\n- ",
        "\n• ",
        "\noh, ",
        "\njawabannya",
        "\nlaporan",
        "\nintinya",
        "\ndetailnya:",
        "\naturannya",
        "\nbot interaction rules:",
        "\nmax exchange",
        "\nbenar,",
        "\nbenar ",
        "\noke",
        "\nbaik",
        "\nini dia",
        "\nuntuk ",
        "\nyang sudah dilakukan",
        "\nsolusi",
        "\ntapi untungnya",
        "\nlaporan ihsg",
        "\nihsg",
        "\nusd/idr",
        "\ndata penutupan",
        "\nsentimen pasar",
        "\nnilai:",
        "\nperubahan:",
        "\nsumber:",
        "\nnilai tukar",
        "\nkesimpulan",
        "\nfinal answer:",
        "\nanswer:",
        "\nsure,",
        "\nhere is",
        "\nhere's",
        "\nthe answer is",
        "\nyes,",
        "\nno,",
        "📊",
        "💰",
        "💵",
        "📈",
        "✅",
        "⚠️",
        "jawabannya",
        "benar,",
        "benar ",
        "oke,",
        "baik,",
        "ini dia",
        "yang sudah dilakukan",
        "tapi untungnya",
        "laporan ihsg",
        "data penutupan",
        "sentimen pasar",
        "nilai tukar",
        "kesimpulan",
        "sure,",
        "here is",
        "here's",
        "the answer is",
        "yes,",
        "no,",
    ];

    MARKERS
        .iter()
        .filter_map(|marker| {
            lower
                .find(marker)
                .map(|pos| pos + if marker.starts_with('\n') { 1 } else { 0 })
        })
        .filter(|pos| *pos > 0 || !looks_like_visible_reasoning_preamble(text))
        .min()
}

fn line_visible_answer_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    for segment in text.split_inclusive('\n') {
        let raw = segment.strip_suffix('\n').unwrap_or(segment);
        let trimmed = raw.trim_start();
        let leading = raw.len().saturating_sub(trimmed.len());
        if is_visible_answer_line(trimmed) {
            return Some(offset + leading);
        }
        offset += segment.len();
    }
    None
}

fn is_visible_answer_line(line: &str) -> bool {
    let normalized = line
        .trim_start_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '/' | '#' | '$'))
        })
        .trim_start()
        .to_ascii_lowercase();
    if normalized.is_empty() || answer_line_is_internal(&normalized) {
        return false;
    }

    normalized.starts_with("laporan ")
        || normalized.starts_with("usd/idr")
        || normalized.starts_with("ihsg")
        || normalized.starts_with("data penutupan")
        || normalized.starts_with("sentimen pasar")
        || normalized.starts_with("nilai tukar")
        || normalized.starts_with("nilai:")
        || normalized.starts_with("perubahan:")
        || normalized.starts_with("sumber:")
        || normalized.starts_with("kesimpulan")
        || normalized.starts_with("final answer:")
        || normalized.starts_with("answer:")
        || normalized.starts_with("here is")
        || normalized.starts_with("here's")
        || normalized.starts_with("the answer is")
}

fn answer_line_is_internal(lower: &str) -> bool {
    const MARKERS: &[&str] = &[
        "the user",
        "let me",
        "i need",
        "i should",
        "actually",
        "hmm",
        "browser tool",
        "web search",
        "search is blocked",
        "tool",
        "jadi ini",
        "minta ",
        "aku harus",
        "aku akan",
        "aku cek",
        "cek dulu",
        "coba aku",
        "sepertinya",
        "harus cari",
        "akan cek",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn reasoning_paragraph_prefix(paragraph: &str) -> bool {
    let lower = paragraph.trim_start().to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "the user is asking",
        "user is asking",
        "the user asked",
        "the user is saying",
        "user is saying",
        "the user says",
        "user says",
        "the user wants",
        "user wants",
        "the question is",
        "this likely refers",
        "okay, i need",
        "ok, i need",
        "actually let me",
        "actually, let me",
        "hmm,",
        "hmm ",
        "let me ",
        "i need to ",
        "i should ",
        "i will ",
        "i'll ",
        "i'm going to",
        "we need to",
        "let's analyze",
        "looking at ",
        "actually, ",
        "so the user",
        "from the memory context",
        "the memory context",
        "based on ",
        "i found ",
        "jadi ini cron job",
        "jadi ini ",
        "ini cron job",
        "aku harus ",
        "aku akan ",
        "aku cek ",
        "cek dulu",
        "coba aku",
        "mari aku",
        "sepertinya",
        "mau laporan diulang",
    ];
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

fn strip_visible_reasoning_preamble(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if looks_like_visible_reasoning_preamble(trimmed) {
        if let Some(start) = visible_answer_start(trimmed) {
            return trimmed[start..].trim_start().to_string();
        }
    } else if let Some(start) = embedded_answer_after_internal_narration(trimmed) {
        return trimmed[start..].trim_start().to_string();
    } else {
        return trimmed.to_string();
    }

    let mut kept = Vec::new();
    let mut dropping = true;
    for paragraph in trimmed.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if dropping && reasoning_paragraph_prefix(paragraph) {
            continue;
        }
        dropping = false;
        kept.push(paragraph);
    }

    kept.join("\n\n").trim().to_string()
}

fn embedded_answer_after_internal_narration(text: &str) -> Option<usize> {
    let internal_start = first_internal_narration_marker(text)?;
    visible_answer_start(&text[internal_start..])
        .filter(|pos| *pos > 0)
        .map(|pos| internal_start + pos)
}

fn first_internal_narration_marker(text: &str) -> Option<usize> {
    const MARKERS: &[&str] = &[
        "akses otomatis",
        "aku coba",
        "browser tool",
        "cek dulu",
        "coba aku",
        "coba endpoint",
        "directly accessing",
        "http_request",
        "i need to ",
        "i should ",
        "i will ",
        "i'll ",
        "let me ",
        "actually let me",
        "actually, let me",
        "now let me",
        "pakai tool",
        "returned empty",
        "returning 503",
        "search is blocked",
        "tool failed",
        "try another approach",
        "try directly",
        "using the ",
        "web search is blocked",
        "jadi ini cron job",
        "aku harus ",
        "aku akan ",
        "sepertinya",
        "mau laporan diulang",
    ];

    let lower = text.to_ascii_lowercase();
    MARKERS.iter().filter_map(|marker| lower.find(marker)).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_screenshot_style_reasoning_before_report() {
        let input = "The user is asking me to fetch the current USD/IDR exchange rate and provide it. Hmm, sepertinya cron ini.\n📊 USD/IDR - Pagi\n• Nilai: Rp 17.921,95";
        let out = sanitize_delivery_text(input);
        assert!(out.changed);
        assert!(!out.text.contains("The user is asking"));
        assert!(out.text.starts_with("📊 USD/IDR"));
    }

    #[test]
    fn strips_indonesian_cron_reasoning_before_report() {
        let input = "Jadi ini cron job yang minta nilai USD/IDR. Aku harus cari kurs terkini dari sumber terpercaya. Aku akan cek beberapa sumber.\nUSD/IDR - Pagi\nNilai: Rp 17.921,95\nSumber: BI";
        let out = sanitize_delivery_text(input);
        assert!(out.changed);
        assert!(!out.text.contains("Jadi ini cron job"));
        assert!(!out.text.contains("Aku harus"));
        assert!(out.text.starts_with("USD/IDR - Pagi"));
        assert!(out.text.contains("Sumber: BI"));
    }

    #[test]
    fn strips_continued_wrapper_and_english_reasoning_before_report() {
        let input = "(continued)\n\nActually let me look more at the big picture. Let me search for recent IHSG news. Now let me write the final report based on all the data collected.\n\nLaporan IHSG Sore\nTanggal: 11 Juni 2026\n\nData Penutupan\nNilai: 5.886,03";
        let out = sanitize_delivery_text(input);
        assert!(out.changed);
        assert!(!out.text.contains("(continued)"));
        assert!(!out.text.contains("Actually let me"));
        assert!(!out.text.contains("Let me search"));
        assert!(out.text.starts_with("Laporan IHSG Sore"));
        assert!(out.text.contains("Data Penutupan"));
    }

    #[test]
    fn strips_embedded_tool_narration_before_report() {
        let input = "Mau laporan diulang ya? Cek dulu hasil yang tadi. Yang tadi gagal dapet data dari Morningstar karena diblokir. Coba aku ulangi sekarang. Web search is blocked too.\n\n📊 USD/IDR - Pagi\n• Nilai: Rp 17.921,95";
        let out = sanitize_delivery_text(input);
        assert!(out.changed);
        assert!(!out.text.contains("Cek dulu"));
        assert!(!out.text.contains("Web search is blocked"));
        assert!(out.text.starts_with("📊 USD/IDR"));
    }

    #[test]
    fn strips_reasoning_tags() {
        let input = "<think>private plan</think>\nJawabannya: aman.";
        let out = sanitize_delivery_text(input);
        assert_eq!(out.text, "Jawabannya: aman.");
    }

    #[test]
    fn strips_reasoning_fences() {
        let input = "```reasoning\nprivate plan\n```\nHere is the answer.";
        let out = sanitize_delivery_text(input);
        assert_eq!(out.text, "Here is the answer.");
    }

    #[test]
    fn keeps_normal_english() {
        let input = "The user guide is below.\n\n1. Open settings.\n2. Save.";
        let out = sanitize_delivery_text(input);
        assert_eq!(out.text, input);
        assert!(!out.blocked);
    }

    #[test]
    fn blocks_when_only_reasoning_remains() {
        let input = "The user is asking about cron. Let me inspect the files first.";
        let out = sanitize_delivery_text(input);
        assert!(out.blocked);
        assert_eq!(out.text, BLOCKED_INTERNAL_OUTPUT_FALLBACK);
    }

    #[test]
    fn partial_can_be_empty() {
        let input = "The user is asking about cron. Let me inspect the files first.";
        let out = sanitize_delivery_text_partial(input);
        assert!(out.blocked);
        assert!(out.text.is_empty());
    }
}
