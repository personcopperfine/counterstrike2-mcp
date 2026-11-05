//! Plain-Rust prompt registry: named, argument-taking texts a server offers as MCP prompts
//! (`prompts/list`, `prompts/get`, wired in [`crate::mcp`]). Like [`crate::tools::Registry`] it has
//! no transport types, so a server's playbooks are testable without a transport.
use std::collections::BTreeMap;
use std::sync::Arc;

/// One prompt argument (`prompts/list` `arguments[]`).
#[derive(Debug, Clone)]
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Renders the prompt text from the (possibly empty) string arguments the client passed.
pub type RenderFn = Arc<dyn Fn(&BTreeMap<String, String>) -> String + Send + Sync>;

#[derive(Clone)]
pub struct PromptSpec {
    pub name: &'static str,
    /// one line: what the prompt is for
    pub description: &'static str,
    pub arguments: Vec<PromptArg>,
    pub render: RenderFn,
}

#[derive(Debug)]
pub enum PromptError {
    Unknown { name: String, known: Vec<&'static str> },
    MissingArgument { prompt: &'static str, argument: &'static str },
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Unknown { name, known } => {
                write!(f, "unknown prompt {name:?}; known: {}", known.join(", "))
            }
            PromptError::MissingArgument { prompt, argument } => {
                write!(f, "prompt {prompt} needs the argument {argument}")
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct PromptSet {
    prompts: Vec<PromptSpec>,
}

impl PromptSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a prompt. Panics on a duplicate name (a programming error).
    pub fn register(&mut self, spec: PromptSpec) {
        assert!(
            !self.prompts.iter().any(|p| p.name == spec.name),
            "duplicate prompt {}",
            spec.name
        );
        self.prompts.push(spec);
    }

    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    /// In registration order.
    pub fn list(&self) -> &[PromptSpec] {
        &self.prompts
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.prompts.iter().map(|p| p.name).collect()
    }

    /// The text `prompts/get` returns for `name`; a required argument that is missing or empty
    /// is an error.
    pub fn render(
        &self,
        name: &str,
        args: &BTreeMap<String, String>,
    ) -> Result<String, PromptError> {
        let spec = self.prompts.iter().find(|p| p.name == name).ok_or_else(|| {
            PromptError::Unknown { name: name.to_string(), known: self.names() }
        })?;
        for a in &spec.arguments {
            if a.required && args.get(a.name).is_none_or(|v| v.trim().is_empty()) {
                return Err(PromptError::MissingArgument { prompt: spec.name, argument: a.name });
            }
        }
        Ok((spec.render)(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> PromptSet {
        let mut s = PromptSet::new();
        s.register(PromptSpec {
            name: "greet",
            description: "says hi",
            arguments: vec![PromptArg { name: "who", description: "name", required: true }],
            render: Arc::new(|a| format!("hi {}", a["who"])),
        });
        s
    }

    #[test]
    fn renders_and_validates() {
        let s = set();
        let mut a = BTreeMap::new();
        assert!(matches!(s.render("greet", &a), Err(PromptError::MissingArgument { .. })));
        a.insert("who".to_string(), "x".to_string());
        assert_eq!(s.render("greet", &a).unwrap(), "hi x");
        assert!(matches!(s.render("nope", &a), Err(PromptError::Unknown { .. })));
    }
}
