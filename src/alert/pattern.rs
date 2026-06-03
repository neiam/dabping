//! SmokePing-style alert patterns.
//!
//! A pattern is a comma list of element matchers, right-anchored at the
//! newest round: `>10%,>10%,>10%` = the last three rounds each had >10%.
//! Elements:
//!   `>X` `<X` `>=X` `<=X` `==X` `!=X`  — compare (trailing `%` allowed)
//!   `==U`                              — value unknown (e.g. rtt with 100% loss)
//!   `*`                                — any one round
//!   `*N*`                              — 0..N rounds of anything (window)
//! Classic: `>0%,*12*,>0%,*12*,>0%` = three loss events within ~12 rounds
//! of each other.

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
enum Elem {
    Cmp(Op, f64),
    Unknown,
    Any,
    Span(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    elems: Vec<Elem>,
}

const MAX_SPAN: u32 = 64;

impl Pattern {
    pub fn parse(s: &str) -> Result<Pattern> {
        let elems: Vec<Elem> = s
            .split(',')
            .map(|tok| parse_elem(tok.trim()).with_context(|| format!("pattern element {tok:?}")))
            .collect::<Result<_>>()?;
        if elems.is_empty() || elems.iter().all(|e| matches!(e, Elem::Span(_))) {
            bail!("pattern {s:?} matches nothing concrete");
        }
        Ok(Pattern { elems })
    }

    /// How many recent rounds the pattern can possibly inspect.
    pub fn depth(&self) -> usize {
        self.elems
            .iter()
            .map(|e| match e {
                Elem::Span(n) => *n as usize,
                _ => 1,
            })
            .sum()
    }

    /// `samples` is oldest→newest; None = unknown value.
    pub fn matches(&self, samples: &[Option<f64>]) -> bool {
        fn rec(elems: &[Elem], samples: &[Option<f64>]) -> bool {
            match elems.split_last() {
                None => true, // pattern consumed; older history is irrelevant
                Some((Elem::Span(n), rest)) => (0..=(*n as usize).min(samples.len()))
                    .any(|k| rec(rest, &samples[..samples.len() - k])),
                Some((e, rest)) => {
                    let Some((s, older)) = samples.split_last() else {
                        return false; // not enough history
                    };
                    elem_match(e, *s) && rec(rest, older)
                }
            }
        }
        rec(&self.elems, samples)
    }
}

fn elem_match(e: &Elem, sample: Option<f64>) -> bool {
    match (e, sample) {
        (Elem::Any, _) => true,
        (Elem::Unknown, s) => s.is_none(),
        (Elem::Cmp(op, v), Some(s)) => match op {
            Op::Eq => s == *v,
            Op::Ne => s != *v,
            Op::Gt => s > *v,
            Op::Lt => s < *v,
            Op::Ge => s >= *v,
            Op::Le => s <= *v,
        },
        (Elem::Cmp(..), None) => false,
        (Elem::Span(_), _) => unreachable!("spans consume in rec()"),
    }
}

fn parse_elem(tok: &str) -> Result<Elem> {
    if tok == "*" {
        return Ok(Elem::Any);
    }
    if let Some(inner) = tok.strip_prefix('*').and_then(|t| t.strip_suffix('*')) {
        let n: u32 = inner.parse().context("span must be *N*")?;
        if n == 0 || n > MAX_SPAN {
            bail!("span must be 1..={MAX_SPAN}");
        }
        return Ok(Elem::Span(n));
    }
    let (op, rest) = if let Some(r) = tok.strip_prefix("==") {
        (Op::Eq, r)
    } else if let Some(r) = tok.strip_prefix("!=") {
        (Op::Ne, r)
    } else if let Some(r) = tok.strip_prefix(">=") {
        (Op::Ge, r)
    } else if let Some(r) = tok.strip_prefix("<=") {
        (Op::Le, r)
    } else if let Some(r) = tok.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = tok.strip_prefix('<') {
        (Op::Lt, r)
    } else {
        bail!("expected an operator (==, !=, >, <, >=, <=), `*`, `*N*` or `==U`");
    };
    let rest = rest.trim();
    if rest.eq_ignore_ascii_case("U") {
        if op != Op::Eq {
            bail!("unknown only supports ==U");
        }
        return Ok(Elem::Unknown);
    }
    let v: f64 = rest
        .strip_suffix('%')
        .unwrap_or(rest)
        .trim()
        .parse()
        .with_context(|| format!("bad number {rest:?}"))?;
    Ok(Elem::Cmp(op, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(vals: &[f64]) -> Vec<Option<f64>> {
        vals.iter().map(|v| Some(*v)).collect()
    }

    #[test]
    fn parses_and_rejects() {
        assert!(Pattern::parse(">10%,>10%,>10%").is_ok());
        assert!(Pattern::parse("<0.05, >=99, ==U, *, *12*").is_ok());
        assert!(Pattern::parse("10").is_err()); // no operator
        assert!(Pattern::parse("*0*").is_err());
        assert!(Pattern::parse("*5*").is_err()); // nothing concrete
        assert!(Pattern::parse(">x%").is_err());
    }

    #[test]
    fn consecutive_right_anchored() {
        let p = Pattern::parse(">10,>10,>10").unwrap();
        assert!(p.matches(&s(&[0.0, 20.0, 30.0, 40.0])));
        assert!(!p.matches(&s(&[20.0, 30.0, 40.0, 0.0]))); // newest is fine
        assert!(!p.matches(&s(&[20.0, 30.0]))); // not enough history
        assert_eq!(p.depth(), 3);
    }

    #[test]
    fn any_and_unknown() {
        let p = Pattern::parse("==U,*,==U").unwrap();
        assert!(p.matches(&[Some(1.0), None, Some(5.0), None]));
        assert!(!p.matches(&[None, Some(5.0), Some(5.0)]));
    }

    #[test]
    fn span_windows() {
        // two loss events within 3 rounds of each other
        let p = Pattern::parse(">0,*3*,>0").unwrap();
        assert!(p.matches(&s(&[5.0, 0.0, 0.0, 5.0]))); // gap of 2
        assert!(p.matches(&s(&[5.0, 5.0]))); // gap of 0
        assert!(!p.matches(&s(&[5.0, 0.0, 0.0, 0.0, 0.0, 5.0]))); // gap of 4
        assert!(!p.matches(&s(&[0.0, 0.0, 5.0, 0.0]))); // newest must match >0
        assert_eq!(p.depth(), 5);
    }

    #[test]
    fn span_against_short_history() {
        let p = Pattern::parse(">0,*5*,>0").unwrap();
        assert!(p.matches(&s(&[5.0, 5.0]))); // span shrinks to fit
        assert!(!p.matches(&s(&[5.0]))); // but both events must exist
    }
}
