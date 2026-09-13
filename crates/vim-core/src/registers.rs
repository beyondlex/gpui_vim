//! Registers: `"a`-`"z`, the unnamed register, the yank register `"0`,
//! the numbered delete ring `"1`-`"9`, the small-delete register `"-`,
//! the clipboard register `"+` and the blackhole `"_`.

use crate::host::VimHost;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterKind {
    Charwise,
    Linewise,
    /// Reserved for visual-block puts.
    Blockwise,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    pub kind: RegisterKind,
}

/// The register file. `"x` reads/writes register `x`; the unnamed register is
/// the register named `"` itself and mirrors every write (vim semantics).
#[derive(Clone, Debug, Default)]
pub struct Registers {
    named: HashMap<char, Register>,
    /// Last written register for unnamed access.
    last: Option<Register>,
}

pub const UNNAMED: char = '"';
pub const BLACKHOLE: char = '_';
pub const CLIPBOARD: char = '+';
pub const SMALL_DELETE: char = '-';
pub const YANK: char = '0';

impl Registers {
    pub fn get(&self, name: char) -> Option<&Register> {
        match name {
            UNNAMED => self.last.as_ref(),
            BLACKHOLE => None,
            _ => self.named.get(&name),
        }
    }

    /// Register contents for a paste, resolving the clipboard register (and
    /// the `clipboard=unnamed` behavior through the host).
    pub fn get_for_paste(&self, name: char, host: &dyn VimHost) -> Option<Register> {
        match name {
            CLIPBOARD => host
                .clipboard_read()
                .map(|text| Register {
                    kind: if text.contains('\n') {
                        RegisterKind::Linewise
                    } else {
                        RegisterKind::Charwise
                    },
                    text,
                }),
            _ => self.get(name).cloned(),
        }
    }

    /// Write to a register. `name` of `UNNAMED` writes to the unnamed slot
    /// only. Every write also updates the unnamed mirror (except the
    /// blackhole), matching vim.
    pub fn store(&mut self, name: char, text: String, kind: RegisterKind) {
        let register = Register {
            text: text.clone(),
            kind,
        };
        if name == BLACKHOLE {
            return;
        }
        if name == UNNAMED {
            self.last = Some(register);
            return;
        }
        self.named.insert(name, register.clone());
        self.last = Some(register);
    }

    /// Yank semantics: explicit register, else `"0` + unnamed.
    pub fn store_yank(&mut self, explicit: Option<char>, text: String, kind: RegisterKind) {
        match explicit {
            Some(name) if name != UNNAMED => {
                self.store(name, text.clone(), kind);
                self.last = Some(Register { text, kind });
            }
            _ => {
                self.store(YANK, text.clone(), kind);
                self.last = Some(Register { text, kind });
            }
        }
    }

    /// Delete semantics: explicit register, else the numbered ring for
    /// multi-line deletes and `"-` for small deletes.
    pub fn store_delete(&mut self, explicit: Option<char>, text: String, kind: RegisterKind) {
        match explicit {
            Some(name) => self.store(name, text, kind),
            None => {
                let multi_line = text.contains('\n');
                if kind == RegisterKind::Linewise || multi_line {
                    // shift 1..8 -> 2..9, then record into "1
                    for i in (2..=9usize).rev() {
                        let from = char::from(b'0' + (i - 1) as u8);
                        let to = char::from(b'0' + i as u8);
                        if let Some(r) = self.named.get(&from) {
                            let r = r.clone();
                            self.named.insert(to, r);
                        }
                    }
                    self.store('1', text, kind);
                } else {
                    self.store(SMALL_DELETE, text, kind);
                }
            }
        }
    }
}
