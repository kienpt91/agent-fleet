//! A small hand-written argument parser.
//!
//! Callers consume what they know (`flag`, `opt`, `multi`, `take_word`) and then call
//! [`Parsed::finish`], which rejects every option nobody consumed. That keeps one rule for all
//! subcommands: an option is only valid where the subcommand asked for it. Everything after a
//! literal `--` is positional, whatever it looks like.

use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;

/// Longest single argument accepted. The daemon limits are far smaller; this only stops absurd
/// input from being copied around.
pub const MAX_ARG_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Parsed {
    /// Arguments before `--`: options, option values and positionals.
    head: Vec<String>,
    /// Arguments after `--`: positional only.
    tail: Vec<String>,
}

fn is_option(s: &str) -> bool {
    s.starts_with('-') && s.len() > 1
}

impl Parsed {
    pub fn new(args: &[String]) -> Result<Parsed> {
        let mut head = Vec::new();
        let mut tail = Vec::new();
        let mut after_terminator = false;
        for a in args {
            if a.len() > MAX_ARG_BYTES {
                return Err(FleetError::usage(format!(
                    "an argument is longer than {MAX_ARG_BYTES} bytes"
                )));
            }
            if after_terminator {
                tail.push(a.clone());
            } else if a == "--" {
                after_terminator = true;
            } else {
                head.push(a.clone());
            }
        }
        Ok(Parsed { head, tail })
    }

    /// True (and consumed) when the exact switch `name` (for example `--json`) was given.
    pub fn flag(&mut self, name: &str) -> bool {
        let before = self.head.len();
        self.head.retain(|a| a != name);
        self.head.len() != before
    }

    /// The value of `name` given as `--name value` or `--name=value`. A value that itself starts
    /// with `--` must use the `=` form, so a forgotten value is reported instead of swallowing
    /// the next option.
    pub fn opt(&mut self, name: &str) -> Result<Option<String>> {
        let mut values = self.multi(name)?;
        if values.len() > 1 {
            return Err(FleetError::usage(format!(
                "option {name} was given more than once"
            )));
        }
        Ok(values.pop())
    }

    /// Every occurrence of a repeatable option, in order.
    pub fn multi(&mut self, name: &str) -> Result<Vec<String>> {
        let prefix = format!("{name}=");
        let mut out = Vec::new();
        let mut i = 0usize;
        while let Some(cur) = self.head.get(i) {
            if cur == name {
                let value = match self.head.get(i.saturating_add(1)) {
                    Some(v) if !v.starts_with("--") => v.clone(),
                    _ => {
                        return Err(FleetError::usage(format!("option {name} needs a value")));
                    }
                };
                self.head.drain(i..i.saturating_add(2));
                out.push(value);
            } else if let Some(v) = cur.strip_prefix(&prefix) {
                let v = v.to_string();
                self.head.remove(i);
                out.push(v);
            } else {
                i = i.saturating_add(1);
            }
        }
        Ok(out)
    }

    /// Removes and returns the leading word (subcommand name) when it is not an option.
    pub fn take_word(&mut self) -> Option<String> {
        match self.head.first() {
            Some(w) if !is_option(w) => Some(self.head.remove(0)),
            _ => None,
        }
    }

    /// Positional arguments left so far, without validating options.
    pub fn positionals(self) -> Vec<String> {
        self.head
            .into_iter()
            .filter(|a| !is_option(a))
            .chain(self.tail)
            .collect()
    }

    /// Rejects any unconsumed option, then returns the positional arguments.
    pub fn finish(self) -> Result<Vec<String>> {
        if let Some(bad) = self.head.iter().find(|a| is_option(a)) {
            let name = bad.split('=').next().unwrap_or(bad);
            return Err(FleetError::usage(format!("unknown option {}", show(name))));
        }
        Ok(self.head.into_iter().chain(self.tail).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Parsed {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Parsed::new(&v).unwrap()
    }

    fn usage_msg<T: std::fmt::Debug>(r: Result<T>) -> String {
        let e = r.unwrap_err();
        assert_eq!(e.code, fleet_domain::error::ErrorCode::Usage);
        e.message
    }

    #[test]
    fn opt_forms_table() {
        let cases: [(&[&str], Option<&str>); 6] = [
            (&["--name", "v"], Some("v")),
            (&["--name=v"], Some("v")),
            (&["--name=a=b"], Some("a=b")),
            (&["--name="], Some("")),
            (&["x", "--name", "-"], Some("-")),
            (&["x"], None),
        ];
        for (args, want) in cases {
            let mut p = parsed(args);
            assert_eq!(p.opt("--name").unwrap().as_deref(), want, "{args:?}");
        }
    }

    #[test]
    fn missing_value_names_the_option() {
        for args in [&["--name"][..], &["--name", "--other"], &["a", "--name"]] {
            let msg = usage_msg(parsed(args).opt("--name"));
            assert!(msg.contains("--name"), "{msg}");
            assert!(msg.contains("needs a value"), "{msg}");
        }
    }

    #[test]
    fn value_that_looks_like_an_option_needs_equals_form() {
        let mut p = parsed(&["--title=--weird"]);
        assert_eq!(p.opt("--title").unwrap().as_deref(), Some("--weird"));
    }

    #[test]
    fn duplicate_single_option_is_rejected() {
        let msg = usage_msg(parsed(&["--a", "1", "--a=2"]).opt("--a"));
        assert!(msg.contains("more than once"));
    }

    #[test]
    fn multi_collects_in_order_and_leaves_the_rest() {
        let mut p = parsed(&["--after", "a", "x", "--after=b", "--after", "c"]);
        assert_eq!(p.multi("--after").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(p.finish().unwrap(), vec!["x"]);
    }

    #[test]
    fn flags_are_consumed_anywhere_before_the_terminator() {
        let mut p = parsed(&["a", "--json", "b", "--", "--json"]);
        assert!(p.flag("--json"));
        assert!(!p.flag("--json"), "the one after -- is positional");
        assert_eq!(p.finish().unwrap(), vec!["a", "b", "--json"]);
    }

    #[test]
    fn flag_does_not_match_prefixes_or_equals_forms() {
        let mut p = parsed(&["--jsonx", "--json=1"]);
        assert!(!p.flag("--json"));
        assert!(p.finish().is_err());
    }

    #[test]
    fn unknown_options_are_rejected_and_quoted_safely() {
        let msg = usage_msg(parsed(&["--bogus\u{1b}[31m=1"]).finish());
        assert!(msg.contains("unknown option"));
        assert!(!msg.contains('\u{1b}'), "control characters are escaped");
        assert!(usage_msg(parsed(&["-x"]).finish()).contains("-x"));
    }

    #[test]
    fn terminator_makes_everything_positional() {
        let p = parsed(&["--", "--bogus", "-x", "--"]);
        assert_eq!(p.finish().unwrap(), vec!["--bogus", "-x", "--"]);
    }

    #[test]
    fn single_dash_is_a_positional() {
        let mut p = parsed(&["--spec", "-", "-"]);
        assert_eq!(p.opt("--spec").unwrap().as_deref(), Some("-"));
        assert_eq!(p.finish().unwrap(), vec!["-"]);
    }

    #[test]
    fn take_word_only_takes_a_leading_non_option() {
        let mut p = parsed(&["run", "create", "--x"]);
        assert_eq!(p.take_word().as_deref(), Some("run"));
        assert_eq!(p.take_word().as_deref(), Some("create"));
        assert_eq!(p.take_word(), None, "an option is not a word");
        let mut q = parsed(&["--x", "run"]);
        assert_eq!(q.take_word(), None);
    }

    #[test]
    fn positionals_skips_options() {
        let p = parsed(&["a", "--x", "b", "--", "--y"]);
        assert_eq!(p.positionals(), vec!["a", "b", "--y"]);
    }

    #[test]
    fn oversized_argument_is_rejected() {
        let big = "x".repeat(MAX_ARG_BYTES + 1);
        assert!(usage_msg(Parsed::new(&[big])).contains("longer"));
        let ok = "x".repeat(MAX_ARG_BYTES);
        assert!(Parsed::new(&[ok]).is_ok());
    }

    #[test]
    fn hostile_values_are_returned_verbatim() {
        let hostile = "\u{1b}]52;c;AAAA\u{7}\u{202e}evil\n";
        let mut p = parsed(&["--title", hostile]);
        assert_eq!(p.opt("--title").unwrap().as_deref(), Some(hostile));
    }

    #[test]
    fn long_value_up_to_the_cap_is_accepted() {
        let v = "y".repeat(MAX_ARG_BYTES);
        let args = vec!["--spec".to_string(), v.clone()];
        let mut p = Parsed::new(&args).unwrap();
        assert_eq!(p.opt("--spec").unwrap(), Some(v));
    }
}
