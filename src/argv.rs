//! The one grammar atrium's launch-flag parsers share: a valued flag at the front
//! of argv, in its separated (`--grid 2x3`) or glued (`--grid=2x3`) form.
//!
//! `identity::parse` and `spawn::parse` each walk argv stripping their own flags
//! and stop at the first token that is not one — everything from there is the
//! hosted command, passed through verbatim. They used to re-implement the
//! separated/glued tokenizing independently, so an edge-case fix in one would not
//! reach the other (r10 audit B7). The tokenizing lives here; what a missing or
//! empty value *means* stays with each parser (identity ignores it, `-n` rejects it).

/// A valued flag recognized at some argv position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag<'a> {
    /// Which of the caller's names matched (as the caller spelled it).
    pub name: &'a str,
    /// The value: the next token for the separated form (`None` when the flag is
    /// last), the text after `=` for the glued form (possibly empty).
    pub value: Option<&'a str>,
    /// How many argv tokens the flag occupies (2 separated-with-value, else 1).
    pub consumed: usize,
}

/// Is `args[i]` one of `names`, separated or glued? `None` means `args[i]` is not
/// one of these flags (for a front-of-argv parser: the hosted command starts).
pub fn valued_flag<'a>(args: &'a [String], i: usize, names: &[&'a str]) -> Option<Flag<'a>> {
    let tok = args.get(i)?.as_str();
    for &name in names {
        if tok == name {
            let value = args.get(i + 1).map(String::as_str);
            let consumed = if value.is_some() { 2 } else { 1 };
            return Some(Flag {
                name,
                value,
                consumed,
            });
        }
        if let Some(rest) = tok.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(Flag {
                name,
                value: Some(rest),
                consumed: 1,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn separated_form_takes_the_next_token() {
        let args = v(&["--grid", "2x3", "claude"]);
        let f = valued_flag(&args, 0, &["--grid"]).unwrap();
        assert_eq!((f.name, f.value, f.consumed), ("--grid", Some("2x3"), 2));
    }

    #[test]
    fn a_trailing_flag_has_no_value_and_consumes_itself() {
        let args = v(&["-I"]);
        let f = valued_flag(&args, 0, &["--identity", "-I"]).unwrap();
        assert_eq!((f.name, f.value, f.consumed), ("-I", None, 1));
    }

    #[test]
    fn glued_form_takes_the_suffix_even_when_empty() {
        let args = v(&["--identity=work", "-n="]);
        let f = valued_flag(&args, 0, &["--identity"]).unwrap();
        assert_eq!((f.value, f.consumed), (Some("work"), 1));
        let f = valued_flag(&args, 1, &["-n"]).unwrap();
        assert_eq!((f.value, f.consumed), (Some(""), 1));
    }

    #[test]
    fn a_prefix_that_is_not_the_flag_does_not_match() {
        // `-nx` is not `-n`, `--gridlock` is not `--grid`: the command starts here.
        let args = v(&["-nx", "--gridlock", "claude"]);
        assert_eq!(valued_flag(&args, 0, &["-n"]), None);
        assert_eq!(valued_flag(&args, 1, &["--grid"]), None);
        assert_eq!(valued_flag(&args, 2, &["-n", "--grid"]), None);
        assert_eq!(valued_flag(&args, 3, &["-n"]), None); // past the end
    }
}
