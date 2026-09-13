//! The option subset the engine reads.
//!
//! Options are plain data on the engine (vim's "global" scope only for v1).
//! Hosts can expose `:set`-style UI later; the engine only needs the values.

#[derive(Clone, Debug)]
pub struct Options {
    pub number: bool,
    pub relativenumber: bool,
    pub scrolloff: usize,
    pub tabstop: usize,
    pub shiftwidth: usize,
    pub expandtab: bool,
    pub autoindent: bool,
    pub ignorecase: bool,
    pub smartcase: bool,
    pub hlsearch: bool,
    pub incsearch: bool,
    pub showmode: bool,
    pub showcmd: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            number: false,
            relativenumber: false,
            scrolloff: 4,
            tabstop: 4,
            shiftwidth: 4,
            expandtab: true,
            autoindent: true,
            ignorecase: true,
            smartcase: true,
            hlsearch: true,
            incsearch: true,
            showmode: true,
            showcmd: true,
        }
    }
}

impl Options {
    /// Case sensitivity for a query, honoring `ignorecase` + `smartcase`.
    pub fn case_insensitive_for(&self, query: &str) -> bool {
        self.ignorecase && !(self.smartcase && query.chars().any(|c| c.is_uppercase()))
    }

    /// `:set <name>` / `:set no<name>` support for boolean options.
    pub fn set_boolean(&mut self, name: &str, value: bool) -> bool {
        match name {
            "number" | "nu" => self.number = value,
            "relativenumber" | "rnu" => self.relativenumber = value,
            "expandtab" | "et" => self.expandtab = value,
            "autoindent" | "ai" => self.autoindent = value,
            "ignorecase" | "ic" => self.ignorecase = value,
            "smartcase" | "scs" => self.smartcase = value,
            "hlsearch" | "hls" => self.hlsearch = value,
            "incsearch" | "is" => self.incsearch = value,
            "showmode" | "smd" => self.showmode = value,
            "showcmd" | "sc" => self.showcmd = value,
            _ => return false,
        }
        true
    }
}
