pub struct HostStack {
    parser: qup_core::Parser,
}

impl HostStack {
    pub const fn new() -> Self {
        Self {
            parser: qup_core::Parser::new(),
        }
    }

    pub const fn parser(&self) -> qup_core::Parser {
        self.parser
    }

    pub const fn hello(&self) -> &'static str {
        "qup host-side scaffolding"
    }
}

impl Default for HostStack {
    fn default() -> Self {
        Self::new()
    }
}

pub const fn hello() -> &'static str {
    HostStack::new().hello()
}
