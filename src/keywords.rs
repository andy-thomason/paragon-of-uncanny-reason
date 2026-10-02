//! Reserved words by language version (IEEE 1364 and IEEE 1800-2023 Annex B).
//! See `docs/design/03-grammar.md` §1.

/// A language version, as named by `--language` and `` `begin_keywords ``.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    V1995,
    V2001NoConfig,
    V2001,
    V2005,
    Sv2005,
    Sv2009,
    Sv2012,
    Sv2017,
    Sv2023,
}

impl Lang {
    /// Parse a version string such as `"1800-2017"` or `"1364-2001-noconfig"`.
    pub fn parse(s: &str) -> Option<Lang> {
        Some(match s {
            "1364-1995" => Lang::V1995,
            "1364-2001-noconfig" => Lang::V2001NoConfig,
            "1364-2001" => Lang::V2001,
            "1364-2005" => Lang::V2005,
            "1800-2005" => Lang::Sv2005,
            "1800-2009" => Lang::Sv2009,
            "1800-2012" => Lang::Sv2012,
            "1800-2017" => Lang::Sv2017,
            "1800-2023" => Lang::Sv2023,
            // Verilog-AMS keywords are not modelled yet; treat as the base standard.
            "VAMS-2.3" | "1800+VAMS" => Lang::Sv2023,
            _ => return None,
        })
    }
}

const V1995: &[&str] = &[
    "always",
    "and",
    "assign",
    "begin",
    "buf",
    "bufif0",
    "bufif1",
    "case",
    "casex",
    "casez",
    "cmos",
    "deassign",
    "default",
    "defparam",
    "disable",
    "edge",
    "else",
    "end",
    "endcase",
    "endfunction",
    "endmodule",
    "endprimitive",
    "endspecify",
    "endtable",
    "endtask",
    "event",
    "for",
    "force",
    "forever",
    "fork",
    "function",
    "highz0",
    "highz1",
    "if",
    "ifnone",
    "initial",
    "inout",
    "input",
    "integer",
    "join",
    "large",
    "macromodule",
    "medium",
    "module",
    "nand",
    "negedge",
    "nmos",
    "nor",
    "not",
    "notif0",
    "notif1",
    "or",
    "output",
    "parameter",
    "pmos",
    "posedge",
    "primitive",
    "pull0",
    "pull1",
    "pulldown",
    "pullup",
    "rcmos",
    "real",
    "realtime",
    "reg",
    "release",
    "repeat",
    "rnmos",
    "rpmos",
    "rtran",
    "rtranif0",
    "rtranif1",
    "scalared",
    "small",
    "specify",
    "specparam",
    "strong0",
    "strong1",
    "supply0",
    "supply1",
    "table",
    "task",
    "time",
    "tran",
    "tranif0",
    "tranif1",
    "tri",
    "tri0",
    "tri1",
    "triand",
    "trior",
    "trireg",
    "vectored",
    "wait",
    "wand",
    "weak0",
    "weak1",
    "while",
    "wire",
    "wor",
    "xnor",
    "xor",
];

const V2001_NOCONFIG: &[&str] = &[
    "automatic",
    "endgenerate",
    "generate",
    "genvar",
    "localparam",
    "noshowcancelled",
    "pulsestyle_ondetect",
    "pulsestyle_onevent",
    "showcancelled",
    "signed",
    "unsigned",
];

const V2001_CONFIG: &[&str] = &[
    "cell",
    "config",
    "design",
    "endconfig",
    "incdir",
    "include",
    "instance",
    "liblist",
    "library",
    "use",
];

const V2005: &[&str] = &["uwire"];

const SV2005: &[&str] = &[
    "alias",
    "always_comb",
    "always_ff",
    "always_latch",
    "assert",
    "assume",
    "before",
    "bind",
    "bins",
    "binsof",
    "bit",
    "break",
    "byte",
    "chandle",
    "class",
    "clocking",
    "const",
    "constraint",
    "context",
    "continue",
    "cover",
    "covergroup",
    "coverpoint",
    "cross",
    "dist",
    "do",
    "endclass",
    "endclocking",
    "endgroup",
    "endinterface",
    "endpackage",
    "endprogram",
    "endproperty",
    "endsequence",
    "enum",
    "expect",
    "export",
    "extends",
    "extern",
    "final",
    "first_match",
    "foreach",
    "forkjoin",
    "iff",
    "ignore_bins",
    "illegal_bins",
    "import",
    "inside",
    "int",
    "interface",
    "intersect",
    "join_any",
    "join_none",
    "local",
    "logic",
    "longint",
    "matches",
    "modport",
    "new",
    "null",
    "package",
    "packed",
    "priority",
    "program",
    "property",
    "protected",
    "pure",
    "rand",
    "randc",
    "randcase",
    "randsequence",
    "ref",
    "return",
    "sequence",
    "shortint",
    "shortreal",
    "solve",
    "static",
    "string",
    "struct",
    "super",
    "tagged",
    "this",
    "throughout",
    "timeprecision",
    "timeunit",
    "type",
    "typedef",
    "union",
    "unique",
    "var",
    "virtual",
    "void",
    "wait_order",
    "wildcard",
    "with",
    "within",
];

const SV2009: &[&str] = &[
    "accept_on",
    "checker",
    "endchecker",
    "eventually",
    "global",
    "implies",
    "let",
    "nexttime",
    "reject_on",
    "restrict",
    "s_always",
    "s_eventually",
    "s_nexttime",
    "s_until",
    "s_until_with",
    "strong",
    "sync_accept_on",
    "sync_reject_on",
    "unique0",
    "until",
    "until_with",
    "untyped",
    "weak",
];

const SV2012: &[&str] = &["implements", "interconnect", "nettype", "soft"];

/// The first version in which `word` is reserved, if it ever is.
pub fn introduced(word: &str) -> Option<Lang> {
    let tables: [(&[&str], Lang); 7] = [
        (V1995, Lang::V1995),
        (V2001_NOCONFIG, Lang::V2001NoConfig),
        (V2001_CONFIG, Lang::V2001),
        (V2005, Lang::V2005),
        (SV2005, Lang::Sv2005),
        (SV2009, Lang::Sv2009),
        (SV2012, Lang::Sv2012),
    ];
    tables
        .iter()
        .find(|(t, _)| t.contains(&word))
        .map(|(_, l)| *l)
}

/// True if `word` is a reserved keyword in `lang`.
pub fn is_keyword(word: &str, lang: Lang) -> bool {
    // `config` keywords come in at V2001, which sorts after V2001NoConfig, so
    // the -noconfig variant does not reserve them.
    introduced(word).is_some_and(|v| v <= lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_by_version() {
        assert!(is_keyword("module", Lang::V1995));
        assert!(!is_keyword("logic", Lang::V2005));
        assert!(is_keyword("logic", Lang::Sv2005));
        assert!(is_keyword("generate", Lang::V2001NoConfig));
        assert!(!is_keyword("config", Lang::V2001NoConfig));
        assert!(is_keyword("config", Lang::V2001));
        assert!(is_keyword("config", Lang::Sv2023));
        assert!(is_keyword("nettype", Lang::Sv2012));
        assert!(!is_keyword("nettype", Lang::Sv2009));
        assert!(!is_keyword("foo", Lang::Sv2023));
    }
}
