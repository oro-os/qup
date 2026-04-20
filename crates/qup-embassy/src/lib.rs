#![cfg_attr(not(test), no_std)]

use core::marker::PhantomData;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;

pub struct EmbassyEndpoint {
    raw_mutex: PhantomData<NoopRawMutex>,
    parser: qup_core::Parser,
}

impl EmbassyEndpoint {
    pub const fn new() -> Self {
        Self {
            raw_mutex: PhantomData,
            parser: qup_core::Parser::new(),
        }
    }

    pub const fn parser(&self) -> qup_core::Parser {
        self.parser
    }
}

impl Default for EmbassyEndpoint {
    fn default() -> Self {
        Self::new()
    }
}
