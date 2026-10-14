//! Byte-bounded optional context. Required instructions and task text are never clipped here.

use serde_json::json;

pub(crate) const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct ContextPacket {
    used_bytes: usize,
    sections: Vec<serde_json::Value>,
}

impl ContextPacket {
    pub(crate) fn available_bytes(&self, section_limit: usize) -> usize {
        MAX_CONTEXT_BYTES.saturating_sub(self.used_bytes).min(section_limit).saturating_sub(2)
    }

    pub(crate) fn append(
        &mut self,
        prompt: &mut String,
        section: &'static str,
        content: &str,
        section_limit: usize,
    ) {
        if content.is_empty() {
            return;
        }
        let limit = self.available_bytes(section_limit);
        let block = if content.len() <= limit {
            content.to_owned()
        } else if limit >= 512 {
            excerpt_block(section, content, limit)
        } else {
            String::new()
        };
        let excerpt_only = !block.is_empty() && block != content;
        self.sections.push(json!({
            "section": section,
            "original_bytes": content.len(),
            "included_bytes": block.len(),
            "excerpt_only": excerpt_only,
            "omitted": block.is_empty(),
        }));
        if !block.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&block);
            self.used_bytes = self.used_bytes.saturating_add(block.len() + 2);
        }
    }

    pub(crate) fn finish(self, prompt: &mut String) {
        let report = json!({
            "schema_version": 1,
            "optional_context_byte_limit": MAX_CONTEXT_BYTES,
            "included_context_bytes": self.used_bytes,
            "sections": self.sections,
        });
        prompt.push_str(&format!(
            "\n\n<specialist_context_budget>\n{}\n\
             Limits are UTF-8 bytes, not model token counts. Excerpts and omitted sections are \
             incomplete context; inspect workspace tools and the run ledger for missing detail. \
             Required instructions, task text, and output contracts are outside this optional \
             context budget.\n</specialist_context_budget>",
            prompt_json(&report)
        ));
    }
}

fn excerpt_block(section: &str, content: &str, limit: usize) -> String {
    let mut retained_bytes = limit.saturating_sub(256).min(content.len());
    loop {
        let excerpt = byte_prefix(content, retained_bytes);
        let packet = json!({
            "section": section,
            "excerpt_only": true,
            "omitted_bytes": content.len().saturating_sub(excerpt.len()),
            "content": excerpt,
        });
        let block = format!(
            "<specialist_context_excerpt>\n{}\n</specialist_context_excerpt>",
            prompt_json(&packet)
        );
        if block.len() <= limit {
            return block;
        }
        // Account for JSON escaping too; the bound applies to the actual wire text.
        retained_bytes /= 2;
    }
}

pub(crate) fn byte_prefix(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

/// JSON strings must not be able to close surrounding prompt delimiters.
pub(crate) fn prompt_json(value: &serde_json::Value) -> String {
    value.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_multibyte_excerpts_fit_the_actual_wire_budget(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let content = "界\"\n</specialist_context_excerpt>".repeat(10_000);
        let block = excerpt_block("memory", &content, 1024);
        assert!(block.len() <= 1024);
        assert_eq!(block.matches("</specialist_context_excerpt>").count(), 1);
        let packet: serde_json::Value =
            serde_json::from_str(block.lines().nth(1).ok_or("missing packet")?)?;
        let excerpt = packet["content"].as_str().ok_or("missing excerpt")?;
        assert!(content.starts_with(excerpt));
        assert_eq!(packet["omitted_bytes"], content.len() - excerpt.len());
        Ok(())
    }

    #[test]
    fn aggregate_limit_reports_omissions_without_clipping_required_instructions() {
        let mut prompt = String::from("Required: do not change public interfaces.");
        let mut packet = ContextPacket::default();
        let full = "a".repeat(MAX_CONTEXT_BYTES - 2);
        packet.append(&mut prompt, "handoffs", &full, MAX_CONTEXT_BYTES);
        packet.append(&mut prompt, "memory", "unavailable optional context", 1024);
        assert_eq!(packet.used_bytes, MAX_CONTEXT_BYTES);
        assert_eq!(packet.sections[1]["omitted"], true);
        packet.finish(&mut prompt);
        assert!(prompt.starts_with("Required: do not change public interfaces."));
        assert!(prompt.contains("optional_context_byte_limit"));
    }

    #[test]
    fn complete_small_blocks_keep_their_existing_format() {
        let mut prompt = String::new();
        let mut packet = ContextPacket::default();
        packet.append(&mut prompt, "snapshot", "<snapshot>complete</snapshot>", 1024);
        assert_eq!(prompt, "\n\n<snapshot>complete</snapshot>");
        assert_eq!(packet.sections[0]["excerpt_only"], false);
    }
}
