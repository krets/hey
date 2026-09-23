//! Prompt assembly (spec section 8).

use crate::context::CommandContext;

#[derive(Debug, Default)]
pub struct PromptParts {
    pub environment: Vec<(String, String)>,
    pub command: Option<CommandContext>,
    pub output: Option<String>,
    pub question: String,
}

/// Sections appear only when non-empty, in fixed order.
pub fn user_message(p: &PromptParts) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !p.environment.is_empty() {
        let body: Vec<String> = p
            .environment
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        sections.push(format!("<environment>\n{}\n</environment>", body.join("\n")));
    }

    if let Some(c) = &p.command
        && !c.command.trim().is_empty()
    {
        let attr = c
            .exit_code
            .map(|n| format!(" exit_code=\"{n}\""))
            .unwrap_or_default();
        sections.push(format!("<command{attr}>\n{}\n</command>", c.command.trim()));
    }

    if let Some(o) = &p.output
        && !o.trim().is_empty()
    {
        sections.push(format!("<output>\n{o}\n</output>"));
    }

    if !p.question.trim().is_empty() {
        sections.push(format!("<question>\n{}\n</question>", p.question.trim()));
    }

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_message_matches_spec_layout() {
        let p = PromptParts {
            environment: vec![
                ("os".into(), "Ubuntu 24.04 (WSL2)".into()),
                ("shell".into(), "bash".into()),
            ],
            command: Some(CommandContext { command: "git push origin main".into(), exit_code: Some(128) }),
            output: Some("fatal: ...".into()),
            question: "why".into(),
        };
        assert_eq!(
            user_message(&p),
            "<environment>\nos: Ubuntu 24.04 (WSL2)\nshell: bash\n</environment>\n\n\
             <command exit_code=\"128\">\ngit push origin main\n</command>\n\n\
             <output>\nfatal: ...\n</output>\n\n\
             <question>\nwhy\n</question>"
        );
    }

    #[test]
    fn empty_sections_are_omitted() {
        let p = PromptParts { question: "hi".into(), ..Default::default() };
        assert_eq!(user_message(&p), "<question>\nhi\n</question>");
    }

    #[test]
    fn command_without_status_has_no_attribute() {
        let p = PromptParts {
            command: Some(CommandContext { command: "make".into(), exit_code: None }),
            question: "q".into(),
            ..Default::default()
        };
        assert!(user_message(&p).starts_with("<command>\nmake\n</command>"));
    }
}
