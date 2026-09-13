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
    pub textwidth: usize,
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
            textwidth: 78,
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

    /// Current value of a boolean option (`:set name!` toggling).
    pub fn bool_option(&self, name: &str) -> Option<bool> {
        match name {
            "number" | "nu" => Some(self.number),
            "relativenumber" | "rnu" => Some(self.relativenumber),
            "expandtab" | "et" => Some(self.expandtab),
            "autoindent" | "ai" => Some(self.autoindent),
            "ignorecase" | "ic" => Some(self.ignorecase),
            "smartcase" | "scs" => Some(self.smartcase),
            "hlsearch" | "hls" => Some(self.hlsearch),
            "incsearch" | "is" => Some(self.incsearch),
            "showmode" | "smd" => Some(self.showmode),
            "showcmd" | "sc" => Some(self.showcmd),
            _ => None,
        }
    }

    /// `:set name=value` for numeric options.
    pub fn set_value(&mut self, name: &str, value: &str) -> bool {
        match name {
            "tabstop" | "ts" => match value.parse() {
                Ok(v) => self.tabstop = v,
                Err(_) => return false,
            },
            "shiftwidth" | "sw" => match value.parse() {
                Ok(v) => self.shiftwidth = v,
                Err(_) => return false,
            },
            "textwidth" | "tw" => match value.parse() {
                Ok(v) => self.textwidth = v,
                Err(_) => return false,
            },
            "scrolloff" | "so" => match value.parse() {
                Ok(v) => self.scrolloff = v,
                Err(_) => return false,
            },
            _ => return false,
        }
        true
    }
}
